//! Shard growth policy coverage for the hosted wrapper.

#![cfg(feature = "global")]

use std::alloc::{GlobalAlloc, Layout};
use std::sync::{Arc, Barrier};

use wf_alloc::SPAN_SIZE;
use wf_alloc::global::HostedLazyGlobalWfSpanAllocator;

const C: usize = 4;

#[test]
fn token_pressure_shards_use_configured_region_size() {
    static G: HostedLazyGlobalWfSpanAllocator<C> = HostedLazyGlobalWfSpanAllocator::new(1, 3);
    const THREADS: usize = 4;

    let layout = Layout::from_size_align(64, 8).unwrap();
    let barrier = Arc::new(Barrier::new(THREADS + 1));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                // SAFETY: GlobalAlloc contract upheld manually in this test.
                unsafe {
                    let p = G.alloc(layout);
                    assert!(!p.is_null());
                    barrier.wait();
                    barrier.wait();
                    G.dealloc(p, layout);
                }
            })
        })
        .collect();

    barrier.wait();
    let stats = G.stats();
    assert!(stats.shard_count >= THREADS);
    assert_eq!(stats.largest_shard_spans, 3);
    barrier.wait();

    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn large_request_shard_grows_to_required_span_count() {
    static G: HostedLazyGlobalWfSpanAllocator<C> = HostedLazyGlobalWfSpanAllocator::new(1, 1);
    let layout = Layout::from_size_align(SPAN_SIZE * 3, 8).unwrap();

    // SAFETY: GlobalAlloc contract upheld manually in this test.
    unsafe {
        let p = G.alloc(layout);
        assert!(!p.is_null());
        G.dealloc(p, layout);
    }

    let stats = G.stats();
    assert!(stats.shard_count >= 2);
    assert!(stats.largest_shard_spans >= 4);
}

#[test]
fn beyond_wfspan_limit_uses_system_fallback_without_growing_shard_to_request() {
    static G: HostedLazyGlobalWfSpanAllocator<C, 1> = HostedLazyGlobalWfSpanAllocator::new(1, 8);
    let layout = Layout::from_size_align(SPAN_SIZE * 5, 8).unwrap();

    // SAFETY: GlobalAlloc contract upheld manually in this test. With a
    // one-span huge granule, five spans exceeds wfspan's largest huge class.
    unsafe {
        let p = G.alloc(layout);
        assert!(!p.is_null());
        G.dealloc(p, layout);
    }

    let stats = G.stats();
    assert_eq!(stats.system_allocations, 1);
    assert_eq!(stats.system_deallocations, 1);
    assert_eq!(stats.largest_shard_spans, 8);
}
