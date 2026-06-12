//! Sequential integration tests for the wait-free huge-slot path.
//!
//! Uses a tiny granule (HUGE_GRANULE_SPANS = 1, i.e. 64 KiB) so the
//! GiB-default behavior is exercised without GiB-scale memory (guide
//! B.16). Covers: class mapping, exact-granule allocations, rounding,
//! over-max null, alignment (including > SPAN_SIZE), slot reuse, slot
//! exhaustion, dispatch boundaries, mixed three-path coexistence on one
//! region, step-count bounds, and double-free detection.

use std::alloc::Layout;
use std::sync::atomic::Ordering;

use wf_alloc::region::OwnedRegion;
use wf_alloc::{
    MAX_HUGE_GRANULES, MAX_HUGE_RUN_CLASSES, MAX_HUGE_RUNS_PER_CLASS, SPAN_SIZE, StepCounter,
    WfSpanAllocator,
};

const N: usize = 2;
const C: usize = 4;
/// Tiny granule: 1 span = 64 KiB. Threshold = 64 KiB; huge classes are
/// 1/2/4 spans.
const HG: usize = 1;
type HugeAlloc = WfSpanAllocator<N, C, HG>;

const GRANULE: usize = HG * SPAN_SIZE;

fn setup(spans: usize) -> (&'static HugeAlloc, OwnedRegion) {
    let region = OwnedRegion::new(spans);
    let alloc = Box::leak(Box::new(HugeAlloc::new()));
    // SAFETY: init once, before sharing; leaked box never moves.
    unsafe { alloc.init(region.ptr(), region.len()) };
    (alloc, region)
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// Header-less class mapping: exactly 2^r granules fit class r (B.16 #1-4).
#[test]
fn huge_class_mapping() {
    let l = |size, align| Layout::from_size_align(size, align).unwrap();
    assert_eq!(HugeAlloc::huge_class_for_layout(l(GRANULE, 8)), Some(0));
    assert_eq!(HugeAlloc::huge_class_for_layout(l(2 * GRANULE, 8)), Some(1));
    // 3 granules round up to class 2 (4 granules).
    assert_eq!(HugeAlloc::huge_class_for_layout(l(3 * GRANULE, 8)), Some(2));
    assert_eq!(HugeAlloc::huge_class_for_layout(l(4 * GRANULE, 8)), Some(2));
    // Above the largest class.
    assert_eq!(
        HugeAlloc::huge_class_for_layout(l(MAX_HUGE_GRANULES * GRANULE + 1, 8)),
        None
    );
    // align > SPAN_SIZE costs slack: one granule at 2-span alignment needs
    // class 1.
    assert_eq!(
        HugeAlloc::huge_class_for_layout(l(GRANULE, 2 * SPAN_SIZE)),
        Some(1)
    );
}

/// Exactly 1/2/4 granules: alloc, write both ends, read, dealloc — each at
/// its exact class (the header-less design's reason to exist).
#[test]
fn exact_granule_alloc_per_class() {
    let (alloc, _region) = setup(16);
    let token = alloc.register_thread().unwrap();

    for class in 0..MAX_HUGE_RUN_CLASSES {
        let size = (1usize << class) * GRANULE; // exactly the run size
        let layout = Layout::from_size_align(size, 8).unwrap();
        assert_eq!(HugeAlloc::huge_class_for_layout(layout), Some(class));
        // SAFETY: valid token, single thread.
        let p = unsafe { alloc.alloc_with_token(layout, token) };
        assert!(!p.is_null(), "huge class {class} alloc failed");

        let tag = 0xC0FF_EE00_0000_0000u64 | class as u64;
        // SAFETY: payload is `size` bytes; write both ends.
        unsafe {
            (p as *mut u64).write(tag);
            p.add(size - 1).write(class as u8);
        }
        assert_eq!(unsafe { (p as *const u64).read() }, tag);
        assert_eq!(unsafe { p.add(size - 1).read() }, class as u8);
        // SAFETY: freed once.
        unsafe { alloc.dealloc_with_token(p, layout, token) };
    }
    assert_eq!(alloc.stats.allocated_huge_runs.load(Ordering::Relaxed), 3);
    // SAFETY: quiescent single-threaded.
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Payload honors the requested alignment, including > SPAN_SIZE.
#[test]
fn alignment_variants() {
    let (alloc, _region) = setup(16);
    let token = alloc.register_thread().unwrap();

    for &align in &[8usize, 4096, SPAN_SIZE, 2 * SPAN_SIZE] {
        let layout = Layout::from_size_align(GRANULE, align).unwrap();
        // SAFETY: valid token.
        let p = unsafe { alloc.alloc_with_token(layout, token) };
        assert!(!p.is_null(), "align={align} alloc failed");
        assert_eq!(p as usize % align, 0, "ptr not {align}-byte aligned");
        // SAFETY: payload is GRANULE bytes.
        unsafe {
            (p as *mut u64).write(align as u64);
            p.add(GRANULE - 1).write(0xA5);
        }
        assert_eq!(unsafe { (p as *const u64).read() }, align as u64);
        // SAFETY: freed once.
        unsafe { alloc.dealloc_with_token(p, layout, token) };
    }
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Dealloc returns the slot to FREE; the next same-class alloc reuses the
/// same carved run (B.16 #6).
#[test]
fn slot_reuse_same_base() {
    let (alloc, _region) = setup(4);
    let token = alloc.register_thread().unwrap();
    let layout = Layout::from_size_align(GRANULE, 8).unwrap();

    // SAFETY: valid token, single thread.
    let p1 = unsafe { alloc.alloc_with_token(layout, token) };
    assert!(!p1.is_null());
    // SAFETY: freed once.
    unsafe { alloc.dealloc_with_token(p1, layout, token) };
    // SAFETY: valid token.
    let p2 = unsafe { alloc.alloc_with_token(layout, token) };
    assert_eq!(p1, p2, "freed huge slot must be reused");
    // SAFETY: freed once.
    unsafe { alloc.dealloc_with_token(p2, layout, token) };
    assert_eq!(
        alloc.stats.allocated_huge_runs.load(Ordering::Relaxed),
        1,
        "second alloc must reuse, not carve"
    );
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// All slots of every class allocated → further huge allocs return null;
/// freeing one slot makes the next alloc succeed (B.16 #8, sequential).
#[test]
fn slot_exhaustion_returns_null() {
    // Directory capacity: 4×1 + 4×2 + 4×4 = 28 spans.
    let (alloc, _region) = setup(32);
    let token = alloc.register_thread().unwrap();

    let mut live = Vec::new();
    for class in 0..MAX_HUGE_RUN_CLASSES {
        let layout = Layout::from_size_align((1usize << class) * GRANULE, 8).unwrap();
        for _ in 0..MAX_HUGE_RUNS_PER_CLASS {
            // SAFETY: valid token, single thread.
            let p = unsafe { alloc.alloc_with_token(layout, token) };
            assert!(!p.is_null(), "directory slot for class {class} unavailable");
            live.push((p, layout));
        }
    }

    // Directory full: any huge request must fail in bounded steps.
    let layout = Layout::from_size_align(GRANULE, 8).unwrap();
    let mut step = StepCounter::new();
    // SAFETY: valid token.
    let p = unsafe { alloc.alloc_with_token_counted(layout, token, &mut step) };
    assert!(p.is_null(), "full directory must return null");
    step.assert_huge_bounds(MAX_HUGE_RUN_CLASSES, MAX_HUGE_RUNS_PER_CLASS);

    // Freeing one class-0 run makes a class-0 alloc succeed again.
    let (p0, l0) = live.remove(0);
    // SAFETY: freed once.
    unsafe { alloc.dealloc_with_token(p0, l0, token) };
    // SAFETY: valid token.
    let p = unsafe { alloc.alloc_with_token(layout, token) };
    assert!(!p.is_null(), "freed slot must be claimable again");
    live.push((p, layout));

    for (p, l) in live {
        // SAFETY: freed once.
        unsafe { alloc.dealloc_with_token(p, l, token) };
    }
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Dispatch boundaries: threshold−1 byte goes large, threshold goes huge
/// (verified through the carve counters).
#[test]
fn dispatch_boundary() {
    let (alloc, _region) = setup(8);
    let token = alloc.register_thread().unwrap();

    // One byte below the threshold → large-run path.
    let layout = Layout::from_size_align(GRANULE - 1, 8).unwrap();
    // SAFETY: valid token.
    let p = unsafe { alloc.alloc_with_token(layout, token) };
    assert!(!p.is_null());
    assert_eq!(alloc.stats.allocated_runs.load(Ordering::Relaxed), 1);
    assert_eq!(alloc.stats.allocated_huge_runs.load(Ordering::Relaxed), 0);
    // SAFETY: freed once.
    unsafe { alloc.dealloc_with_token(p, layout, token) };

    // Exactly the threshold → huge path.
    let layout = Layout::from_size_align(GRANULE, 8).unwrap();
    // SAFETY: valid token.
    let p = unsafe { alloc.alloc_with_token(layout, token) };
    assert!(!p.is_null());
    assert_eq!(alloc.stats.allocated_huge_runs.load(Ordering::Relaxed), 1);
    // SAFETY: freed once.
    unsafe { alloc.dealloc_with_token(p, layout, token) };

    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Small, large, and huge allocations coexist on ONE region without
/// overlap (asserted structurally by check_quiescent's page occupancy).
#[test]
fn mixed_three_paths_single_region() {
    let (alloc, _region) = setup(24);
    let token = alloc.register_thread().unwrap();

    let small = Layout::from_size_align(64, 8).unwrap(); // small class
    let large = Layout::from_size_align(4096, 8).unwrap(); // large run
    let huge = Layout::from_size_align(GRANULE, 8).unwrap(); // huge slot

    let mut live = Vec::new();
    for i in 0..4u64 {
        for &l in &[small, large, huge] {
            // SAFETY: valid token, single thread.
            let p = unsafe { alloc.alloc_with_token(l, token) };
            assert!(!p.is_null());
            // SAFETY: every payload is at least 8 bytes.
            unsafe { (p as *mut u64).write(i << 32 | l.size() as u64) };
            live.push((p, l, i << 32 | l.size() as u64));
        }
    }
    for &(p, l, tag) in &live {
        assert_eq!(unsafe { (p as *const u64).read() }, tag, "pattern corrupted");
        // SAFETY: freed once.
        unsafe { alloc.dealloc_with_token(p, l, token) };
    }
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Every huge alloc/dealloc (success and failure) stays within the
/// wait-freedom step bounds — no unbounded loops on the path (B.16 #9).
#[test]
fn step_counts_stay_bounded() {
    let (alloc, _region) = setup(8);
    let token = alloc.register_thread().unwrap();
    let layout = Layout::from_size_align(GRANULE, 8).unwrap();

    let mut ptrs = Vec::new();
    // Run past both slot and pool exhaustion.
    for _ in 0..16 {
        let mut step = StepCounter::new();
        // SAFETY: valid token, single thread.
        let p = unsafe { alloc.alloc_with_token_counted(layout, token, &mut step) };
        step.assert_huge_bounds(MAX_HUGE_RUN_CLASSES, MAX_HUGE_RUNS_PER_CLASS);
        if !p.is_null() {
            ptrs.push(p);
        }
    }
    for &p in &ptrs {
        let mut step = StepCounter::new();
        // SAFETY: freed once.
        unsafe { alloc.dealloc_with_token_counted(p, layout, token, &mut step) };
        step.assert_huge_bounds(MAX_HUGE_RUN_CLASSES, MAX_HUGE_RUNS_PER_CLASS);
    }
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Double free is caught by the slot-state debug assert (B.16 #7).
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "double free of a huge run")]
fn double_free_detected_in_debug() {
    let (alloc, _region) = setup(4);
    let token = alloc.register_thread().unwrap();
    let layout = Layout::from_size_align(GRANULE, 8).unwrap();
    // SAFETY: valid token, single thread.
    let p = unsafe { alloc.alloc_with_token(layout, token) };
    assert!(!p.is_null());
    // SAFETY: first free is valid; the second is the contract violation
    // this test asserts is caught in debug builds.
    unsafe {
        alloc.dealloc_with_token(p, layout, token);
        alloc.dealloc_with_token(p, layout, token);
    }
}
