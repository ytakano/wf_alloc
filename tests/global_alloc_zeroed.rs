//! GlobalAlloc::alloc_zeroed coverage for the hosted wrapper.

#![cfg(feature = "global")]

use std::alloc::{GlobalAlloc, Layout};

use wf_alloc::SPAN_SIZE;
use wf_alloc::global::HostedLazyGlobalWfSpanAllocator;

const C: usize = 4;

#[test]
fn hosted_lazy_alloc_zeroed_clears_wfspan_allocation() {
    static G: HostedLazyGlobalWfSpanAllocator<C> = HostedLazyGlobalWfSpanAllocator::new(1, 4);
    let layout = Layout::from_size_align(257, 16).unwrap();

    // SAFETY: GlobalAlloc contract upheld manually in this test.
    unsafe {
        let p = G.alloc_zeroed(layout);
        assert!(!p.is_null());
        for i in 0..layout.size() {
            assert_eq!(p.add(i).read(), 0);
        }
        G.dealloc(p, layout);
    }
}

#[test]
fn hosted_lazy_alloc_zeroed_clears_system_fallback_allocation() {
    static G: HostedLazyGlobalWfSpanAllocator<C, 1> = HostedLazyGlobalWfSpanAllocator::new(1, 8);
    let layout = Layout::from_size_align(SPAN_SIZE * 5, 64).unwrap();

    // SAFETY: GlobalAlloc contract upheld manually in this test.
    unsafe {
        let p = G.alloc_zeroed(layout);
        assert!(!p.is_null());
        assert_eq!((p as usize) % layout.align(), 0);
        assert_eq!(p.read(), 0);
        assert_eq!(p.add(layout.size() - 1).read(), 0);
        assert_eq!(G.stats().system_allocations, 1);
        G.dealloc(p, layout);
        assert_eq!(G.stats().system_deallocations, 1);
    }
}
