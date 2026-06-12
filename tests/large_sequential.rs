//! Sequential integration tests for the wait-free large-run path.
//!
//! Covers: run-class mapping, per-class alloc/dealloc, alignment (including
//! larger than SPAN_SIZE), run recycling, Policy-1 whole-run reuse, exhaustion
//! recovery, publish/acquire via the helping lanes, dispatch boundary,
//! mixed small+large from ONE region, step-count bounds, and null dealloc.

use std::alloc::Layout;

use wf_alloc::region::OwnedRegion;
use wf_alloc::{
    HELP_BUDGET_H, LARGE_LOCAL_RUN_LIMIT_K, MAX_BLOCK_SIZE, MAX_LARGE_RUN_CLASSES,
    MAX_SUPPORTED_CLASSES, SPAN_SIZE, StepCounter, WfSpanAllocator, run_class_bytes,
    run_class_for_layout,
};

const N: usize = 2;
const C: usize = 8;

/// Single region serving both the small-span and large-run paths.
fn setup(spans: usize) -> (&'static WfSpanAllocator<N, C>, OwnedRegion) {
    let region = OwnedRegion::new(spans);
    let alloc = Box::leak(Box::new(WfSpanAllocator::<N, C>::new()));
    // SAFETY: init once, before sharing; leaked box never moves.
    unsafe { alloc.init(region.ptr(), region.len()) };
    (alloc, region)
}

/// Largest payload (at 8-byte alignment) that still maps to run `class`.
fn max_payload(class: usize) -> usize {
    run_class_bytes(class)
        - wf_alloc::config::SPAN_HEADER_RESERVE
        - core::mem::size_of::<wf_alloc::LargeAllocHeader>()
        - 7
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// API-level run-class mapping sanity (unit tests cover the boundaries).
#[test]
fn run_class_mapping() {
    let l = Layout::from_size_align(MAX_BLOCK_SIZE + 1, 8).unwrap();
    assert_eq!(run_class_for_layout(l), Some(0));

    let l = Layout::from_size_align(max_payload(0), 8).unwrap();
    assert_eq!(run_class_for_layout(l), Some(0));
    let l = Layout::from_size_align(max_payload(0) + 1, 8).unwrap();
    assert_eq!(run_class_for_layout(l), Some(1));

    // Over-aligned but small: slack forces a larger run.
    let l = Layout::from_size_align(64, 2 * SPAN_SIZE).unwrap();
    assert_eq!(run_class_for_layout(l), Some(2));

    // Beyond the largest run class.
    let l = Layout::from_size_align(wf_alloc::MAX_LARGE_SIZE, 8).unwrap();
    assert_eq!(run_class_for_layout(l), None);
}

/// Run classes 0..6: alloc, write, read, dealloc, each at its exact class.
#[test]
fn basic_alloc_dealloc_per_class() {
    // Classes 0..6 carve 1+2+4+8+16+32 = 63 spans; 80 leaves headroom.
    let (alloc, _region) = setup(80);
    let token = alloc.register_thread().unwrap();

    for class in 0..6usize {
        let layout = Layout::from_size_align(max_payload(class), 8).unwrap();
        assert_eq!(run_class_for_layout(layout), Some(class));
        // SAFETY: valid token, single thread.
        let p = unsafe { alloc.alloc_with_token(layout, token) };
        assert!(!p.is_null(), "run class {class} alloc failed");

        let tag = 0xDEAD_CAFE_0000_0000u64 | class as u64;
        // SAFETY: payload is at least 8 bytes.
        unsafe { (p as *mut u64).write(tag) };
        // Also touch the LAST byte of the payload: it lives in the run's
        // final span, proving the whole contiguous run belongs to us.
        // SAFETY: payload is max_payload(class) bytes long.
        unsafe { p.add(max_payload(class) - 1).write(class as u8) };
        assert_eq!(unsafe { (p as *const u64).read() }, tag);
        assert_eq!(
            unsafe { p.add(max_payload(class) - 1).read() },
            class as u8,
            "run class {class} tail corrupted"
        );
        // SAFETY: freed once.
        unsafe { alloc.dealloc_with_token(p, layout, token) };
    }
    // SAFETY: quiescent single-threaded.
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Payload must honor the requested alignment, including > SPAN_SIZE.
#[test]
fn alignment_variants() {
    let (alloc, _region) = setup(16);
    let token = alloc.register_thread().unwrap();
    // Size stays oversized for C=8 (class_to_size(7) = 2048) so every
    // request lands on the large path.
    let size = 4096usize;

    for &align in &[16usize, 64, 4096, 32768, SPAN_SIZE, 2 * SPAN_SIZE] {
        let layout = Layout::from_size_align(size, align).unwrap();
        // SAFETY: valid token.
        let p = unsafe { alloc.alloc_with_token(layout, token) };
        assert!(!p.is_null(), "align={align} alloc failed");
        assert_eq!(p as usize % align, 0, "ptr not {align}-byte aligned");
        // SAFETY: payload is `size` bytes; write both ends.
        unsafe {
            (p as *mut u64).write(align as u64);
            p.add(size - 1).write(0x5A);
        }
        assert_eq!(unsafe { (p as *const u64).read() }, align as u64);
        // SAFETY: freed once.
        unsafe { alloc.dealloc_with_token(p, layout, token) };
    }
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Free + realloc of the same layout must reuse the run from the local list.
#[test]
fn recycling_reuses_same_run() {
    let (alloc, _region) = setup(4);
    let token = alloc.register_thread().unwrap();
    let layout = Layout::from_size_align(4096, 8).unwrap();

    // SAFETY: valid token, single thread.
    let p1 = unsafe { alloc.alloc_with_token(layout, token) };
    assert!(!p1.is_null());
    // SAFETY: freed once.
    unsafe { alloc.dealloc_with_token(p1, layout, token) };

    // Same class + same placement → must reuse the run just freed.
    // SAFETY: valid token.
    let p2 = unsafe { alloc.alloc_with_token(layout, token) };
    assert_eq!(p1, p2, "freed run must be reused");
    // SAFETY: freed once.
    unsafe { alloc.dealloc_with_token(p2, layout, token) };
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Policy 1: when the exact class is unavailable and the pool is exhausted,
/// a whole larger free run is used as-is, and freeing it returns it to ITS
/// OWN class, not the requested one.
#[test]
fn policy1_whole_larger_run_reuse() {
    // Exactly 2 spans: one class-1 run consumes the whole pool.
    let (alloc, _region) = setup(2);
    let token = alloc.register_thread().unwrap();

    let class1_layout = Layout::from_size_align(SPAN_SIZE, 8).unwrap();
    assert_eq!(run_class_for_layout(class1_layout), Some(1));
    // SAFETY: valid token, single thread.
    let p1 = unsafe { alloc.alloc_with_token(class1_layout, token) };
    assert!(!p1.is_null(), "class-1 carve failed");
    let run_base = p1 as usize & !(SPAN_SIZE - 1);
    // SAFETY: freed once. The class-1 run is now in local_runs[1].
    unsafe { alloc.dealloc_with_token(p1, class1_layout, token) };

    // Pool is exhausted; a class-0 request must escalate to the class-1 run.
    let class0_layout = Layout::from_size_align(4096, 8).unwrap();
    assert_eq!(run_class_for_layout(class0_layout), Some(0));
    // SAFETY: valid token.
    let p0 = unsafe { alloc.alloc_with_token(class0_layout, token) };
    assert!(!p0.is_null(), "escalated whole-run alloc failed");
    assert!(
        (p0 as usize) >= run_base && (p0 as usize) + 4096 <= run_base + 2 * SPAN_SIZE,
        "escalated payload must lie inside the class-1 run"
    );
    // SAFETY: freed once.
    unsafe { alloc.dealloc_with_token(p0, class0_layout, token) };

    // The run went back to class 1: a class-1 request succeeds again even
    // though the raw pool is empty.
    // SAFETY: valid token.
    let p1b = unsafe { alloc.alloc_with_token(class1_layout, token) };
    assert!(!p1b.is_null(), "run did not return to its own class");
    // SAFETY: freed once.
    unsafe { alloc.dealloc_with_token(p1b, class1_layout, token) };
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Fill the pool with class-0 runs to exhaustion, free everything, and
/// verify the full capacity is re-allocatable from the run lists.
#[test]
fn exhaustion_then_recycle() {
    let spans = 8usize;
    let (alloc, _region) = setup(spans);
    let token = alloc.register_thread().unwrap();
    let layout = Layout::from_size_align(4096, 8).unwrap();

    let mut ptrs = Vec::new();
    loop {
        // SAFETY: valid token, single thread.
        let p = unsafe { alloc.alloc_with_token(layout, token) };
        if p.is_null() {
            break;
        }
        // SAFETY: payload is at least 8 bytes.
        unsafe { (p as *mut u64).write(ptrs.len() as u64 ^ p as u64) };
        ptrs.push(p);
    }
    assert_eq!(ptrs.len(), spans, "every span must serve one class-0 run");

    let mut sorted = ptrs.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), ptrs.len(), "duplicate run handed out");

    for (idx, &p) in ptrs.iter().enumerate() {
        // SAFETY: pattern written above.
        unsafe { assert_eq!((p as *const u64).read(), idx as u64 ^ p as u64) };
        // SAFETY: freed once.
        unsafe { alloc.dealloc_with_token(p, layout, token) };
    }

    // Full capacity must be re-allocatable (local list + public list after
    // the K-limit publish).
    let mut again = Vec::new();
    for i in 0..ptrs.len() {
        // SAFETY: valid token.
        let p = unsafe { alloc.alloc_with_token(layout, token) };
        assert!(!p.is_null(), "run {i} unavailable after exhaust+free");
        again.push(p);
    }
    for &p in &again {
        // SAFETY: freed once.
        unsafe { alloc.dealloc_with_token(p, layout, token) };
    }
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Freeing more than LARGE_LOCAL_RUN_LIMIT_K runs publishes the surplus to
/// the public run-list, where ANOTHER token acquires it via the bounded
/// helping protocol (runlists_acquire_run).
#[test]
fn publish_then_acquire_via_helping() {
    let spans = LARGE_LOCAL_RUN_LIMIT_K + 4;
    let (alloc, _region) = setup(spans);
    let token_a = alloc.register_thread().unwrap();
    let token_b = alloc.register_thread().unwrap();
    let layout = Layout::from_size_align(4096, 8).unwrap();

    // Token A carves the whole pool, then frees everything: K stay local,
    // the surplus is published.
    let mut ptrs = Vec::new();
    for _ in 0..spans {
        // SAFETY: tokens are used sequentially by this single thread.
        let p = unsafe { alloc.alloc_with_token(layout, token_a) };
        assert!(!p.is_null());
        ptrs.push(p);
    }
    for &p in &ptrs {
        // SAFETY: freed once.
        unsafe { alloc.dealloc_with_token(p, layout, token_a) };
    }
    assert!(
        alloc.stats.published_runs.load(std::sync::atomic::Ordering::Relaxed) >= 4,
        "surplus runs must be published"
    );

    // Token B has no local runs and the pool is empty: its allocation must
    // come from A's public run-list through the helping lane.
    let mut step = StepCounter::new();
    // SAFETY: token b used by the same single thread, sequentially.
    let p = unsafe { alloc.alloc_with_token_counted(layout, token_b, &mut step) };
    assert!(!p.is_null(), "public run acquisition failed");
    step.assert_large_bounds(N, HELP_BUDGET_H, N, MAX_LARGE_RUN_CLASSES);
    assert!(
        alloc.stats.acquired_public_runs.load(std::sync::atomic::Ordering::Relaxed) >= 1,
        "run must come from a public list"
    );
    // SAFETY: freed once.
    unsafe { alloc.dealloc_with_token(p, layout, token_b) };
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Regression for the x86_64 CAS2 backend: public large-run acquisition uses
/// `cmpxchg16b` to pop from SPMC run lists. The asm memory operand must not
/// alias rbx while rbx carries the replacement low word.
#[test]
fn public_large_run_acquire_64k_and_1m() {
    let cases = [
        Layout::from_size_align(64 * 1024, 8).unwrap(),
        Layout::from_size_align(1024 * 1024, 8).unwrap(),
    ];

    for layout in cases {
        let class = run_class_for_layout(layout).unwrap();
        let spans = (LARGE_LOCAL_RUN_LIMIT_K + 4) * (1 << class);
        let (alloc, _region) = setup(spans);
        let token_a = alloc.register_thread().unwrap();
        let token_b = alloc.register_thread().unwrap();

        let mut ptrs = Vec::new();
        for _ in 0..(LARGE_LOCAL_RUN_LIMIT_K + 4) {
            // SAFETY: valid token, single thread.
            let p = unsafe { alloc.alloc_with_token(layout, token_a) };
            assert!(!p.is_null(), "initial large allocation failed");
            ptrs.push(p);
        }
        for &p in &ptrs {
            // SAFETY: freed once; surplus runs publish after the local limit.
            unsafe { alloc.dealloc_with_token(p, layout, token_a) };
        }

        let mut acquired = Vec::new();
        for _ in 0..4 {
            let mut step = StepCounter::new();
            // SAFETY: token B has no local runs, so this must acquire from
            // token A's public run list while the raw pool is exhausted.
            let p = unsafe { alloc.alloc_with_token_counted(layout, token_b, &mut step) };
            assert!(!p.is_null(), "public large-run acquisition failed");
            step.assert_large_bounds(N, HELP_BUDGET_H, N, MAX_LARGE_RUN_CLASSES);
            acquired.push(p);
        }
        assert!(
            alloc
                .stats
                .acquired_public_runs
                .load(std::sync::atomic::Ordering::Relaxed)
                >= 4,
            "large runs must come from public lists"
        );

        for &p in &acquired {
            // SAFETY: freed once.
            unsafe { alloc.dealloc_with_token(p, layout, token_b) };
        }
        unsafe { wf_alloc::verify::check_quiescent(alloc) };
    }
}

/// Verify the exact small/large dispatch boundary with all small classes.
#[test]
fn size_class_boundary() {
    type FullAlloc = WfSpanAllocator<2, MAX_SUPPORTED_CLASSES>;
    let region = OwnedRegion::new(8);
    let alloc: &'static FullAlloc = Box::leak(Box::new(FullAlloc::new()));
    // SAFETY: init once, before sharing.
    unsafe { alloc.init(region.ptr(), region.len()) };
    let token = alloc.register_thread().unwrap();

    // MAX_BLOCK_SIZE → small path (class C-1 < C).
    let layout = Layout::from_size_align(MAX_BLOCK_SIZE, 8).unwrap();
    // SAFETY: valid token.
    let p = unsafe { alloc.alloc_with_token(layout, token) };
    assert!(!p.is_null(), "MAX_BLOCK_SIZE must succeed via small path");
    // SAFETY: freed once.
    unsafe { alloc.dealloc_with_token(p, layout, token) };

    // MAX_BLOCK_SIZE + 1 → size_to_class returns None → large-run path.
    let layout = Layout::from_size_align(MAX_BLOCK_SIZE + 1, 8).unwrap();
    // SAFETY: valid token.
    let p = unsafe { alloc.alloc_with_token(layout, token) };
    assert!(!p.is_null(), "MAX_BLOCK_SIZE+1 must succeed via large path");
    // SAFETY: freed once.
    unsafe { alloc.dealloc_with_token(p, layout, token) };

    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Small and large allocations from ONE region must never overlap.
#[test]
fn mixed_small_and_large_single_region() {
    let (alloc, _region) = setup(24);
    let token = alloc.register_thread().unwrap();

    let small_layout = Layout::from_size_align(64, 8).unwrap();
    let large_size = 4096usize;
    let large_layout = Layout::from_size_align(large_size, 8).unwrap();

    let mut small_ptrs = Vec::new();
    let mut large_ptrs = Vec::new();
    for _ in 0..10 {
        // SAFETY: valid token, single thread.
        let sp = unsafe { alloc.alloc_with_token(small_layout, token) };
        let lp = unsafe { alloc.alloc_with_token(large_layout, token) };
        assert!(!sp.is_null() && !lp.is_null());
        small_ptrs.push(sp);
        large_ptrs.push(lp);
    }

    // No small pointer may fall inside a live large payload (they come from
    // the same region, so check ranges, not just identity).
    for &sp in &small_ptrs {
        for &lp in &large_ptrs {
            let (s, l) = (sp as usize, lp as usize);
            assert!(
                s + 64 <= l || s >= l + large_size,
                "small block {sp:p} overlaps large payload {lp:p}"
            );
        }
    }

    for &p in &small_ptrs {
        // SAFETY: freed once.
        unsafe { alloc.dealloc_with_token(p, small_layout, token) };
    }
    for &p in &large_ptrs {
        // SAFETY: freed once.
        unsafe { alloc.dealloc_with_token(p, large_layout, token) };
    }
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Every large alloc/dealloc stays within the wait-freedom step bounds —
/// the structural proof that no unbounded retry loop exists on the path.
#[test]
fn step_counts_stay_bounded() {
    let (alloc, _region) = setup(8);
    let token = alloc.register_thread().unwrap();
    let layout = Layout::from_size_align(4096, 8).unwrap();

    let mut ptrs = Vec::new();
    // Run past exhaustion so carve, local-reuse, escalation, and failure
    // paths are all exercised.
    for _ in 0..12 {
        let mut step = StepCounter::new();
        // SAFETY: valid token, single thread.
        let p = unsafe { alloc.alloc_with_token_counted(layout, token, &mut step) };
        step.assert_large_bounds(N, HELP_BUDGET_H, N, MAX_LARGE_RUN_CLASSES);
        if !p.is_null() {
            ptrs.push(p);
        }
    }
    for &p in &ptrs {
        let mut step = StepCounter::new();
        // SAFETY: freed once.
        unsafe { alloc.dealloc_with_token_counted(p, layout, token, &mut step) };
        step.assert_large_bounds(N, HELP_BUDGET_H, N, MAX_LARGE_RUN_CLASSES);
    }
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// `dealloc(null)` must be a no-op — no panic, no UB.
#[test]
fn null_dealloc_is_noop() {
    let (alloc, _region) = setup(2);
    let token = alloc.register_thread().unwrap();
    let layout = Layout::from_size_align(4096, 8).unwrap();
    // SAFETY: null is explicitly handled by dealloc_with_token.
    unsafe { alloc.dealloc_with_token(core::ptr::null_mut(), layout, token) };
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}
