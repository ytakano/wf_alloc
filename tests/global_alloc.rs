//! GlobalAlloc wrapper tests (run with `--features global`).

#![cfg(feature = "global")]

use std::alloc::{GlobalAlloc, Layout};

use wf_alloc::SPAN_SIZE;
use wf_alloc::global::GlobalWfSpanAllocator;
use wf_alloc::region::OwnedRegion;

const N: usize = 4;
const C: usize = 4;

#[test]
fn global_alloc_roundtrip_and_limits() {
    let region = Box::leak(Box::new(OwnedRegion::new(16)));
    let g: &'static GlobalWfSpanAllocator<C> = Box::leak(Box::new(GlobalWfSpanAllocator::new(N)));
    // SAFETY: init once before sharing; leaked box never moves.
    unsafe { g.init(region.ptr(), region.len()) };

    let layout = Layout::from_size_align(64, 8).unwrap();
    // SAFETY: GlobalAlloc contract upheld manually in this test.
    unsafe {
        let p = g.alloc(layout);
        assert!(!p.is_null());
        p.write_bytes(0xAB, 64);
        g.dealloc(p, layout);

        // Requests beyond the small classes go through the large-run path.
        let big = Layout::from_size_align(SPAN_SIZE, 8).unwrap();
        let bp = g.alloc(big);
        assert!(!bp.is_null());
        g.dealloc(bp, big);

        // A request beyond the largest huge run class (> 4 GiB) is
        // unsupported: null, never a panic.
        let too_big = Layout::from_size_align(5usize << 30, 8).unwrap();
        assert!(g.alloc(too_big).is_null());
    }

    // Cross-thread: each thread registers lazily via TLS; frees of another
    // thread's pointer take the remote path.
    let handles: Vec<_> = (0..2)
        .map(|_| {
            std::thread::spawn(move || {
                // SAFETY: as above.
                unsafe {
                    let p = g.alloc(layout);
                    assert!(!p.is_null());
                    p as usize
                }
            })
        })
        .collect();
    for h in handles {
        let addr = h.join().unwrap() as *mut u8;
        // SAFETY: freed once, from the main thread (remote free).
        unsafe { g.dealloc(addr, layout) };
    }
    // SAFETY: quiescent.
    unsafe { wf_alloc::verify::check_quiescent(&g.inner) };
}

#[test]
fn registration_exhaustion_returns_null() {
    let region = Box::leak(Box::new(OwnedRegion::new(4)));
    let g: &'static GlobalWfSpanAllocator<C> = Box::leak(Box::new(GlobalWfSpanAllocator::new(1)));
    // SAFETY: init once before sharing.
    unsafe { g.init(region.ptr(), region.len()) };
    let layout = Layout::from_size_align(64, 8).unwrap();

    // First thread takes the only slot.
    std::thread::spawn(move || {
        // SAFETY: as above.
        unsafe { assert!(!g.alloc(layout).is_null()) };
    })
    .join()
    .unwrap();

    // This (second) thread cannot register: alloc returns null, no panic.
    // SAFETY: as above.
    unsafe { assert!(g.alloc(layout).is_null()) };
}
