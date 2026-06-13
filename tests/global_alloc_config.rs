//! Configuration API coverage for the hosted wrapper.

#![cfg(feature = "global")]

use std::alloc::{GlobalAlloc, Layout};

use wf_alloc::global::{GlobalAllocatorConfig, HostedLazyGlobalWfSpanAllocator};

const C: usize = 4;

#[test]
fn hosted_lazy_with_config_uses_named_parameters() {
    static G: HostedLazyGlobalWfSpanAllocator<C> =
        HostedLazyGlobalWfSpanAllocator::with_config(GlobalAllocatorConfig::new(2, 4));
    let layout = Layout::from_size_align(64, 8).unwrap();

    // SAFETY: GlobalAlloc contract upheld manually in this test.
    unsafe {
        let p = G.alloc(layout);
        assert!(!p.is_null());
        G.dealloc(p, layout);
    }

    assert_eq!(G.shard_count(), 1);
    assert_eq!(G.inner().unwrap().active_threads(), 3);
}

#[test]
fn hosted_lazy_default_hosted_allocates() {
    static G: HostedLazyGlobalWfSpanAllocator<C> =
        HostedLazyGlobalWfSpanAllocator::default_hosted();
    let layout = Layout::from_size_align(64, 8).unwrap();

    // SAFETY: GlobalAlloc contract upheld manually in this test.
    unsafe {
        let p = G.alloc(layout);
        assert!(!p.is_null());
        G.dealloc(p, layout);
    }

    assert_eq!(G.shard_count(), 1);
}

#[test]
fn hosted_lazy_zero_config_uses_runtime_defaults() {
    static G: HostedLazyGlobalWfSpanAllocator<C> =
        HostedLazyGlobalWfSpanAllocator::with_config(GlobalAllocatorConfig::new(0, 0));
    let layout = Layout::from_size_align(64, 8).unwrap();

    // SAFETY: GlobalAlloc contract upheld manually in this test.
    unsafe {
        let p = G.alloc(layout);
        assert!(!p.is_null());
        G.dealloc(p, layout);
    }

    assert_eq!(G.shard_count(), 1);
}
