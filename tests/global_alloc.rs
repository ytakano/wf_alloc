//! GlobalAlloc wrapper tests (run with `--features global`).

#![cfg(feature = "global")]

use std::alloc::{GlobalAlloc, Layout};

use wf_alloc::SPAN_SIZE;
use wf_alloc::global::HostedLazyGlobalWfSpanAllocator;

const N: usize = 4;
const C: usize = 4;

#[test]
fn hosted_lazy_global_alloc_roundtrip_and_limits() {
    static G: HostedLazyGlobalWfSpanAllocator<C> = HostedLazyGlobalWfSpanAllocator::new(N, 16);

    let layout = Layout::from_size_align(64, 8).unwrap();
    // SAFETY: GlobalAlloc contract upheld manually in this test.
    unsafe {
        let p = G.alloc(layout);
        assert!(!p.is_null());
        p.write_bytes(0xAB, 64);
        G.dealloc(p, layout);

        // Requests beyond the small classes go through the large-run path.
        let big = Layout::from_size_align(SPAN_SIZE, 8).unwrap();
        let bp = G.alloc(big);
        assert!(!bp.is_null());
        G.dealloc(bp, big);

        // A request beyond the largest huge run class (> 4 GiB) is
        // unsupported: null, never a panic.
        let too_big = Layout::from_size_align(5usize << 30, 8).unwrap();
        assert!(G.alloc(too_big).is_null());
    }

    // Cross-thread: each thread registers lazily via TLS; frees of another
    // thread's pointer take the remote path.
    let handles: Vec<_> = (0..2)
        .map(|_| {
            std::thread::spawn(move || {
                // SAFETY: as above.
                unsafe {
                    let p = G.alloc(layout);
                    assert!(!p.is_null());
                    p as usize
                }
            })
        })
        .collect();
    for h in handles {
        let addr = h.join().unwrap() as *mut u8;
        // SAFETY: freed once, from the main thread (remote free).
        unsafe { G.dealloc(addr, layout) };
    }
    // SAFETY: quiescent.
    unsafe { wf_alloc::verify::check_quiescent(G.inner().unwrap()) };
}

#[test]
fn hosted_lazy_registration_exhaustion_returns_null() {
    static G: HostedLazyGlobalWfSpanAllocator<C> = HostedLazyGlobalWfSpanAllocator::new(1, 4);
    let layout = Layout::from_size_align(64, 8).unwrap();

    // First thread takes the only slot.
    std::thread::spawn(move || {
        // SAFETY: as above. The pointer is intentionally leaked so that the
        // thread keeps its token for this allocator instance.
        unsafe { assert!(!G.alloc(layout).is_null()) };
    })
    .join()
    .unwrap();

    // This (second) thread cannot register: alloc returns null, no panic.
    // SAFETY: as above.
    unsafe { assert!(G.alloc(layout).is_null()) };
}
