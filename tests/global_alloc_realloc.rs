//! GlobalAlloc::realloc coverage for the hosted wrapper.

#![cfg(feature = "global")]

use std::alloc::{GlobalAlloc, Layout};

use wf_alloc::SPAN_SIZE;
use wf_alloc::global::HostedLazyGlobalWfSpanAllocator;

const C: usize = 4;

unsafe fn fill(ptr: *mut u8, len: usize) {
    for i in 0..len {
        unsafe { ptr.add(i).write((i % 251) as u8) };
    }
}

unsafe fn assert_prefix(ptr: *mut u8, len: usize) {
    for i in 0..len {
        assert_eq!(unsafe { ptr.add(i).read() }, (i % 251) as u8);
    }
}

#[test]
fn hosted_lazy_realloc_grows_and_shrinks_wfspan_allocations() {
    static G: HostedLazyGlobalWfSpanAllocator<C> = HostedLazyGlobalWfSpanAllocator::new(1, 8);
    let small = Layout::from_size_align(64, 16).unwrap();
    let large = Layout::from_size_align(1024, 16).unwrap();
    let shrink = Layout::from_size_align(32, 16).unwrap();

    // SAFETY: GlobalAlloc contract upheld manually in this test.
    unsafe {
        let p = G.alloc(small);
        assert!(!p.is_null());
        fill(p, small.size());

        let p = G.realloc(p, small, large.size());
        assert!(!p.is_null());
        assert_eq!((p as usize) % large.align(), 0);
        assert_prefix(p, small.size());
        fill(p, large.size());

        let p = G.realloc(p, large, shrink.size());
        assert!(!p.is_null());
        assert_eq!((p as usize) % shrink.align(), 0);
        assert_prefix(p, shrink.size());
        G.dealloc(p, shrink);
    }
}

#[test]
fn hosted_lazy_realloc_moves_between_wfspan_and_system() {
    static G: HostedLazyGlobalWfSpanAllocator<C, 1> = HostedLazyGlobalWfSpanAllocator::new(1, 8);
    let small = Layout::from_size_align(128, 64).unwrap();
    let fallback = Layout::from_size_align(SPAN_SIZE * 5, 64).unwrap();

    // SAFETY: GlobalAlloc contract upheld manually in this test.
    unsafe {
        let p = G.alloc(small);
        assert!(!p.is_null());
        fill(p, small.size());

        let p = G.realloc(p, small, fallback.size());
        assert!(!p.is_null());
        assert_eq!((p as usize) % fallback.align(), 0);
        assert_prefix(p, small.size());
        assert_eq!(G.stats().system_allocations, 1);

        let p = G.realloc(p, fallback, small.size());
        assert!(!p.is_null());
        assert_eq!((p as usize) % small.align(), 0);
        assert_prefix(p, small.size());
        assert_eq!(G.stats().system_deallocations, 1);
        G.dealloc(p, small);
    }
}

#[test]
fn hosted_lazy_realloc_after_allocating_thread_exit() {
    static G: HostedLazyGlobalWfSpanAllocator<C> = HostedLazyGlobalWfSpanAllocator::new(1, 8);
    let old = Layout::from_size_align(64, 8).unwrap();
    let new = Layout::from_size_align(512, 8).unwrap();

    let addr = std::thread::spawn(move || {
        // SAFETY: GlobalAlloc contract upheld manually in this test.
        unsafe {
            let p = G.alloc(old);
            assert!(!p.is_null());
            fill(p, old.size());
            p as usize
        }
    })
    .join()
    .unwrap();

    // SAFETY: old pointer is still live after the allocating thread exited.
    unsafe {
        let p = G.realloc(addr as *mut u8, old, new.size());
        assert!(!p.is_null());
        assert_prefix(p, old.size());
        G.dealloc(p, new);
    }
}

#[test]
fn hosted_lazy_realloc_null_and_zero_size_edges() {
    static G: HostedLazyGlobalWfSpanAllocator<C> = HostedLazyGlobalWfSpanAllocator::new(1, 4);
    let layout = Layout::from_size_align(64, 8).unwrap();

    // SAFETY: null realloc is treated as alloc; zero-size realloc frees and
    // returns null for this wrapper.
    unsafe {
        let p = G.realloc(core::ptr::null_mut(), layout, layout.size());
        assert!(!p.is_null());
        let q = G.realloc(p, layout, 0);
        assert!(q.is_null());
    }
}
