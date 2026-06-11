//! Sequential integration tests for the large-object allocator path.
//!
//! Covers: no-region null returns, per-class alloc/dealloc, alignment,
//! block recycling, exhaustion recovery, small/large boundary, pattern
//! isolation, mixed small+large, and null-dealloc safety.

use std::alloc::Layout;

use wf_alloc::region::OwnedRegion;
use wf_alloc::{
    LARGE_CLASSES, MAX_BLOCK_SIZE, MAX_LARGE_SIZE, MAX_SUPPORTED_CLASSES, MIN_LARGE_SIZE,
    WfSpanAllocator, large_size_class,
};

const N: usize = 2;
const C: usize = 8;

/// Allocator with both small and large regions.
fn setup(
    small_spans: usize,
    large_spans: usize,
) -> (&'static WfSpanAllocator<N, C>, OwnedRegion, OwnedRegion) {
    let small_region = OwnedRegion::new(small_spans);
    let large_region = OwnedRegion::new(large_spans);
    let alloc = Box::leak(Box::new(WfSpanAllocator::<N, C>::new()));
    // SAFETY: init once, before sharing; leaked box never moves.
    unsafe {
        alloc.init(small_region.ptr(), small_region.len());
        alloc.init_large(large_region.ptr(), large_region.len());
    }
    (alloc, small_region, large_region)
}

/// Allocator with only a small region — large requests must return null.
fn setup_no_large(small_spans: usize) -> (&'static WfSpanAllocator<N, C>, OwnedRegion) {
    let region = OwnedRegion::new(small_spans);
    let alloc = Box::leak(Box::new(WfSpanAllocator::<N, C>::new()));
    // SAFETY: as above.
    unsafe { alloc.init(region.ptr(), region.len()) };
    (alloc, region)
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// Without `init_large`, any request that overflows the small pool must return null.
#[test]
fn no_large_region_returns_null() {
    let (alloc, _small) = setup_no_large(2);
    let token = alloc.register_thread().unwrap();
    for size in [MIN_LARGE_SIZE, MIN_LARGE_SIZE * 4, 1024 * 1024] {
        let layout = Layout::from_size_align(size, 8).unwrap();
        // SAFETY: valid token, single thread.
        let p = unsafe { alloc.alloc_with_token(layout, token) };
        assert!(p.is_null(), "size={size}: expected null without large region");
    }
}

/// First 6 large size classes (32 KiB … 1 MiB): alloc, write, read, dealloc.
#[test]
fn basic_alloc_dealloc_per_class() {
    // 64 spans = 4 MiB; the 6 class alloc_sizes sum to ~2 MiB; 2× for
    // worst-case alignment rounding still fits comfortably.
    let (alloc, _small, _large) = setup(2, 64);
    let token = alloc.register_thread().unwrap();

    for class in 0..6usize {
        // `alloc_size - 16` keeps `needed = back_offset(16) + size` equal to
        // `alloc_size` exactly, so the request maps to large class `class`
        // rather than bumping to class+1 (which would double memory usage and
        // risk exceeding the test region).
        let alloc_size = MIN_LARGE_SIZE << class;
        let size = alloc_size - 16;
        let layout = Layout::from_size_align(size, 8).unwrap();
        // SAFETY: valid token, single thread.
        let p = unsafe { alloc.alloc_with_token(layout, token) };
        assert!(!p.is_null(), "large class {class} (alloc_size={alloc_size}) alloc failed");

        let tag = 0xDEAD_CAFE_0000_0000u64 | class as u64;
        // SAFETY: block is at least 8 bytes.
        unsafe { (p as *mut u64).write(tag) };
        assert_eq!(
            unsafe { (p as *const u64).read() },
            tag,
            "large class {class} data corrupted"
        );
        // SAFETY: freed once.
        unsafe { alloc.dealloc_with_token(p, layout, token) };
    }
    // SAFETY: quiescent single-threaded.
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Payload pointer must be aligned to the requested alignment for each variant.
#[test]
fn alignment_variants() {
    // 4 spans = 256 KiB; recycling means only 2 fresh blocks are ever bumped.
    let (alloc, _small, _large) = setup(2, 4);
    let token = alloc.register_thread().unwrap();
    // size stays sub-class-0 so all requests land in class 0 (32 KiB).
    let size = MIN_LARGE_SIZE / 2;

    for &align in &[16usize, 64, 256, 4096, 32768] {
        let layout = Layout::from_size_align(size, align).unwrap();
        // SAFETY: valid token.
        let p = unsafe { alloc.alloc_with_token(layout, token) };
        assert!(!p.is_null(), "align={align} alloc failed");
        assert_eq!(p as usize % align, 0, "ptr not {align}-byte aligned");
        // SAFETY: freed once.
        unsafe { alloc.dealloc_with_token(p, layout, token) };
    }
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Deallocating then reallocating the same layout must reuse the freed block.
#[test]
fn recycling_reuses_same_block() {
    // 4 spans (256 KiB) — one class-0 block (32 KiB) is more than enough.
    let (alloc, _small, _large) = setup(2, 4);
    let token = alloc.register_thread().unwrap();
    let layout = Layout::from_size_align(MIN_LARGE_SIZE / 2, 8).unwrap();

    // SAFETY: valid token, single thread.
    let p1 = unsafe { alloc.alloc_with_token(layout, token) };
    assert!(!p1.is_null());
    // SAFETY: freed once.
    unsafe { alloc.dealloc_with_token(p1, layout, token) };

    // Same class + same back_offset → must pop the freed block from the Treiber stack.
    // SAFETY: valid token.
    let p2 = unsafe { alloc.alloc_with_token(layout, token) };
    assert_eq!(p1, p2, "freed large block must be reused");
    // SAFETY: freed once.
    unsafe { alloc.dealloc_with_token(p2, layout, token) };
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Fill the large pool to exhaustion, then verify all freed blocks are reusable.
#[test]
fn exhaustion_then_recycle() {
    // 4 spans (256 KiB) → 8 class-0 blocks of 32 KiB each.
    let (alloc, _small, _large) = setup(2, 4);
    let token = alloc.register_thread().unwrap();
    let layout = Layout::from_size_align(MIN_LARGE_SIZE / 2, 8).unwrap();

    let mut ptrs = Vec::new();
    loop {
        // SAFETY: valid token, single thread.
        let p = unsafe { alloc.alloc_with_token(layout, token) };
        if p.is_null() {
            break;
        }
        // SAFETY: block is at least 8 bytes.
        unsafe { (p as *mut u64).write(ptrs.len() as u64 ^ p as u64) };
        ptrs.push(p);
    }
    assert!(!ptrs.is_empty(), "must allocate at least one large block");

    // All pointers must be distinct.
    let mut sorted = ptrs.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), ptrs.len(), "duplicate large allocation detected");

    for (idx, &p) in ptrs.iter().enumerate() {
        // Verify pattern integrity before freeing.
        // SAFETY: pattern written above.
        unsafe { assert_eq!((p as *const u64).read(), idx as u64 ^ p as u64) };
        // SAFETY: freed once.
        unsafe { alloc.dealloc_with_token(p, layout, token) };
    }

    // All freed blocks must now be re-allocatable from the Treiber stacks.
    for i in 0..ptrs.len() {
        // SAFETY: valid token.
        let p = unsafe { alloc.alloc_with_token(layout, token) };
        assert!(!p.is_null(), "block {i}: recycled block unavailable after exhaust+free");
    }
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Verify the exact small/large dispatch boundary.
#[test]
fn size_class_boundary() {
    // Use all small size classes so MAX_BLOCK_SIZE is the true boundary.
    type FullAlloc = WfSpanAllocator<2, MAX_SUPPORTED_CLASSES>;
    let small_region = OwnedRegion::new(4);
    let large_region = OwnedRegion::new(4);
    let alloc: &'static FullAlloc = Box::leak(Box::new(FullAlloc::new()));
    // SAFETY: init once, before sharing.
    unsafe {
        alloc.init(small_region.ptr(), small_region.len());
        alloc.init_large(large_region.ptr(), large_region.len());
    }
    let token = alloc.register_thread().unwrap();

    // MAX_BLOCK_SIZE → small path (class C-1 < C).
    let layout = Layout::from_size_align(MAX_BLOCK_SIZE, 8).unwrap();
    // SAFETY: valid token.
    let p = unsafe { alloc.alloc_with_token(layout, token) };
    assert!(!p.is_null(), "MAX_BLOCK_SIZE must succeed via small path");
    // SAFETY: freed once.
    unsafe { alloc.dealloc_with_token(p, layout, token) };

    // MAX_BLOCK_SIZE + 1 → size_to_class returns None → large path.
    let layout = Layout::from_size_align(MAX_BLOCK_SIZE + 1, 8).unwrap();
    // SAFETY: valid token.
    let p = unsafe { alloc.alloc_with_token(layout, token) };
    assert!(!p.is_null(), "MAX_BLOCK_SIZE+1 must succeed via large path");
    // SAFETY: freed once.
    unsafe { alloc.dealloc_with_token(p, layout, token) };

    // large_size_class boundary checks (no allocation needed).
    assert_eq!(large_size_class(MAX_LARGE_SIZE), Some(LARGE_CLASSES - 1));
    assert_eq!(large_size_class(MAX_LARGE_SIZE + 1), None);
    assert_eq!(large_size_class(usize::MAX), None);

    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Multiple concurrent live blocks of different classes must not corrupt each other.
#[test]
fn write_pattern_no_overlap() {
    // 64 spans = 4 MiB; fits the sum of all allocations below.
    let (alloc, _small, _large) = setup(2, 64);
    let token = alloc.register_thread().unwrap();

    // (size, align) pairs that map to different alloc_bases or back_offsets.
    let cases: &[(usize, usize)] = &[
        (MIN_LARGE_SIZE / 2, 8),   // class 0, back_offset 16
        (MIN_LARGE_SIZE, 8),       // class 1, back_offset 16
        (MIN_LARGE_SIZE * 2, 8),   // class 2, back_offset 16
        (MIN_LARGE_SIZE / 2, 256), // class 0, back_offset 256
    ];

    let mut live: Vec<(*mut u8, Layout, u64)> = Vec::new();
    for (i, &(size, align)) in cases.iter().enumerate() {
        let layout = Layout::from_size_align(size, align).unwrap();
        // SAFETY: valid token.
        let p = unsafe { alloc.alloc_with_token(layout, token) };
        assert!(!p.is_null(), "case {i}: alloc failed");
        let tag = (0xBEEF_0000u64 << 32) | (i as u64) << 16 | (p as u64 & 0xFFFF);
        // SAFETY: block is at least 8 bytes.
        unsafe { (p as *mut u64).write(tag) };
        live.push((p, layout, tag));
    }

    // Read back all patterns while all blocks are simultaneously live.
    for &(p, layout, tag) in &live {
        assert_eq!(unsafe { (p as *const u64).read() }, tag, "pattern corrupted");
        // SAFETY: freed once.
        unsafe { alloc.dealloc_with_token(p, layout, token) };
    }
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Small and large allocations from the same allocator must occupy distinct addresses.
#[test]
fn mixed_small_and_large() {
    let (alloc, _small, _large) = setup(4, 16);
    let token = alloc.register_thread().unwrap();

    let small_layout = Layout::from_size_align(64, 8).unwrap();
    let large_layout = Layout::from_size_align(MIN_LARGE_SIZE / 2, 8).unwrap();

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

    // No address from the large pool must coincide with a small-pool address.
    for &sp in &small_ptrs {
        for &lp in &large_ptrs {
            assert_ne!(sp, lp, "small and large ptrs must not overlap");
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

/// `dealloc(null)` must be a no-op — no panic, no UB.
#[test]
fn null_dealloc_is_noop() {
    let (alloc, _small, _large) = setup(2, 4);
    let token = alloc.register_thread().unwrap();
    let layout = Layout::from_size_align(MIN_LARGE_SIZE / 2, 8).unwrap();
    // SAFETY: null is explicitly handled by dealloc_with_token.
    unsafe { alloc.dealloc_with_token(core::ptr::null_mut(), layout, token) };
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}
