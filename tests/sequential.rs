//! Sequential tests (Milestone 1/2 acceptance): span init, exhaustion,
//! reuse, size classes, alignment, span_from_ptr. Miri-compatible.

use std::alloc::Layout;

use wf_alloc::WfSpanAllocator;
use wf_alloc::region::OwnedRegion;
use wf_alloc::size_class::blocks_per_span;
use wf_alloc::span::span_from_ptr;
use wf_alloc::{SPAN_SIZE, class_to_size};

const N: usize = 2;
const C: usize = 8;

fn setup(spans: usize) -> (&'static WfSpanAllocator<N, C>, OwnedRegion) {
    let region = OwnedRegion::new(spans);
    let alloc = Box::leak(Box::new(WfSpanAllocator::<N, C>::new()));
    // SAFETY: init once, before sharing; leaked box never moves.
    unsafe { alloc.init(region.ptr(), region.len()) };
    (alloc, region)
}

#[test]
fn one_span_alloc_until_exhaustion_and_reuse() {
    let (alloc, _region) = setup(1);
    let token = alloc.register_thread().unwrap();
    let bs = class_to_size(0);
    let layout = Layout::from_size_align(bs, 8).unwrap();
    let m = blocks_per_span(bs);

    let mut ptrs = Vec::new();
    // block_count allocations succeed
    for _ in 0..m {
        // SAFETY: valid token, single thread.
        let p = unsafe { alloc.alloc_with_token(layout, token) };
        assert!(!p.is_null());
        ptrs.push(p);
    }
    // the next allocation returns null (single span, fixed pool)
    // SAFETY: as above.
    let p = unsafe { alloc.alloc_with_token(layout, token) };
    assert!(p.is_null(), "exhausted pool must return null");

    // all pointers distinct and span-resolvable
    let mut sorted = ptrs.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), m, "duplicate allocation detected");
    let span = span_from_ptr(ptrs[0]);
    for &p in &ptrs {
        assert_eq!(span_from_ptr(p), span);
        assert!((p as usize) - (span as usize) < SPAN_SIZE);
    }

    // deallocated blocks can be reallocated
    for &p in &ptrs {
        // SAFETY: each pointer freed exactly once.
        unsafe { alloc.dealloc_with_token(p, layout, token) };
    }
    for _ in 0..m {
        // SAFETY: as above.
        let p = unsafe { alloc.alloc_with_token(layout, token) };
        assert!(!p.is_null(), "freed blocks must be reusable");
    }

    // SAFETY: quiescent single-threaded test.
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

#[test]
fn all_size_classes_and_alignment() {
    let (alloc, _region) = setup(C * 2);
    let token = alloc.register_thread().unwrap();

    for class in 0..C {
        let size = class_to_size(class);
        let layout = Layout::from_size_align(size, size.min(4096)).unwrap();
        // SAFETY: valid token.
        let p = unsafe { alloc.alloc_with_token(layout, token) };
        assert!(!p.is_null(), "class {class} alloc failed");
        assert_eq!(
            p as usize % size,
            0,
            "class {class} block not naturally aligned"
        );
        // SAFETY: freed once.
        unsafe { alloc.dealloc_with_token(p, layout, token) };
    }

    // alignment greater than size is handled
    let layout = Layout::from_size_align(8, 256).unwrap();
    // SAFETY: valid token.
    let p = unsafe { alloc.alloc_with_token(layout, token) };
    assert!(!p.is_null());
    assert_eq!(p as usize % 256, 0);
    // SAFETY: freed once.
    unsafe { alloc.dealloc_with_token(p, layout, token) };

    // SAFETY: quiescent.
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

#[test]
fn unsupported_large_returns_null() {
    let (alloc, _region) = setup(2);
    let token = alloc.register_thread().unwrap();
    for (size, align) in [(SPAN_SIZE, 8), (SPAN_SIZE / 2, 8), (64, SPAN_SIZE)] {
        let layout = Layout::from_size_align(size, align).unwrap();
        // SAFETY: valid token.
        let p = unsafe { alloc.alloc_with_token(layout, token) };
        assert!(p.is_null(), "size={size} align={align} must be unsupported");
    }
}

#[test]
fn registration_bounded() {
    let (alloc, _region) = setup(1);
    assert!(alloc.register_thread().is_some());
    assert!(alloc.register_thread().is_some());
    // registration fails after MAX_THREADS (N = 2)
    assert!(alloc.register_thread().is_none());
}
