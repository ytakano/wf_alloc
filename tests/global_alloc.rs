//! GlobalAlloc wrapper tests (run with `--features global`).

#![cfg(feature = "global")]

use std::alloc::{GlobalAlloc, Layout};
#[cfg(debug_assertions)]
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use wf_alloc::SPAN_SIZE;
use wf_alloc::global::HostedLazyGlobalWfSpanAllocator;

const C: usize = 4;
#[cfg(debug_assertions)]
const GLOBAL_HEADER_MAGIC: usize = 0x5746_474C_4F42_3100;
#[cfg(debug_assertions)]
const BACKEND_SYSTEM: usize = 2;
#[cfg(debug_assertions)]
const GLOBAL_HEADER_USIZES: usize = 6;
#[cfg(debug_assertions)]
const GLOBAL_HEADER_SIZE: usize = GLOBAL_HEADER_USIZES * std::mem::size_of::<usize>();

#[cfg(debug_assertions)]
unsafe fn global_header_words(ptr: *mut u8) -> *mut usize {
    // SAFETY: test callers pass pointers returned by HostedLazyGlobalWfSpanAllocator.
    unsafe { ptr.sub(GLOBAL_HEADER_SIZE).cast::<usize>() }
}

static TLS_DROP_ALLOCATOR: HostedLazyGlobalWfSpanAllocator<C> =
    HostedLazyGlobalWfSpanAllocator::new(1, 4);
static TLS_DROP_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

struct AllocOnDrop;

impl Drop for AllocOnDrop {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(128, 8).unwrap();
        // SAFETY: GlobalAlloc contract upheld manually in this test TLS
        // destructor. The pointer is freed before Drop returns.
        unsafe {
            let p = TLS_DROP_ALLOCATOR.alloc(layout);
            if !p.is_null() {
                p.write_bytes(0xD3, layout.size());
                TLS_DROP_ALLOCATOR.dealloc(p, layout);
                TLS_DROP_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

std::thread_local! {
    static ALLOC_ON_DROP: AllocOnDrop = const { AllocOnDrop };
}

#[test]
fn hosted_lazy_global_alloc_roundtrip() {
    static G: HostedLazyGlobalWfSpanAllocator<C> = HostedLazyGlobalWfSpanAllocator::new(4, 16);

    let layout = Layout::from_size_align(64, 8).unwrap();
    // SAFETY: GlobalAlloc contract upheld manually in this test.
    unsafe {
        let p = G.alloc(layout);
        assert!(!p.is_null());
        assert_eq!((p as usize) % layout.align(), 0);
        p.write_bytes(0xAB, 64);
        G.dealloc(p, layout);

        // Requests beyond the small classes go through wfspan's large path.
        let big = Layout::from_size_align(SPAN_SIZE, 8).unwrap();
        let bp = G.alloc(big);
        assert!(!bp.is_null());
        assert_eq!((bp as usize) % big.align(), 0);
        G.dealloc(bp, big);
    }

    // Cross-thread: frees of another thread's pointer use either that shard's
    // current token or the reserved service token.
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
        // SAFETY: freed once, from the main thread.
        unsafe { G.dealloc(addr, layout) };
    }
    // SAFETY: quiescent for the first shard.
    unsafe { wf_alloc::verify::check_quiescent(G.inner().unwrap()) };
}

#[test]
fn hosted_lazy_allocates_during_tls_destructor() {
    let before = TLS_DROP_ALLOCATIONS.load(Ordering::Relaxed);

    std::thread::spawn(|| {
        ALLOC_ON_DROP.with(|_| {});

        let layout = Layout::from_size_align(64, 8).unwrap();
        // SAFETY: GlobalAlloc contract upheld manually in this test.
        unsafe {
            let p = TLS_DROP_ALLOCATOR.alloc(layout);
            assert!(!p.is_null());
            TLS_DROP_ALLOCATOR.dealloc(p, layout);
        }
    })
    .join()
    .unwrap();

    assert_eq!(TLS_DROP_ALLOCATIONS.load(Ordering::Relaxed), before + 1);
    assert_eq!(TLS_DROP_ALLOCATOR.user_tokens_in_use(), 0);
}

#[test]
fn hosted_lazy_realloc_grows_and_preserves_prefix() {
    static G: HostedLazyGlobalWfSpanAllocator<C> = HostedLazyGlobalWfSpanAllocator::new(2, 8);

    let old_layout = Layout::from_size_align(64, 16).unwrap();
    let new_layout = Layout::from_size_align(256, 16).unwrap();

    // SAFETY: GlobalAlloc contract upheld manually in this test. The old
    // pointer is not used after successful realloc, and the returned pointer is
    // freed with the new layout.
    unsafe {
        let p = G.alloc(old_layout);
        assert!(!p.is_null());
        assert_eq!((p as usize) % old_layout.align(), 0);
        for i in 0..old_layout.size() {
            p.add(i).write(i as u8);
        }

        let q = G.realloc(p, old_layout, new_layout.size());
        assert!(!q.is_null());
        assert_eq!((q as usize) % new_layout.align(), 0);
        for i in 0..old_layout.size() {
            assert_eq!(q.add(i).read(), i as u8, "byte {i} was not preserved");
        }

        q.add(old_layout.size())
            .write_bytes(0xE1, new_layout.size() - old_layout.size());
        G.dealloc(q, new_layout);
    }
}

#[test]
fn hosted_lazy_realloc_shrinks_and_preserves_retained_prefix() {
    static G: HostedLazyGlobalWfSpanAllocator<C> = HostedLazyGlobalWfSpanAllocator::new(2, 8);

    let old_layout = Layout::from_size_align(256, 32).unwrap();
    let new_layout = Layout::from_size_align(64, 32).unwrap();

    // SAFETY: GlobalAlloc contract upheld manually in this test. Only the
    // retained prefix is read after shrinking, and the result is freed with the
    // resized layout.
    unsafe {
        let p = G.alloc(old_layout);
        assert!(!p.is_null());
        assert_eq!((p as usize) % old_layout.align(), 0);
        for i in 0..old_layout.size() {
            p.add(i).write((255 - i) as u8);
        }

        let q = G.realloc(p, old_layout, new_layout.size());
        assert!(!q.is_null());
        assert_eq!((q as usize) % new_layout.align(), 0);
        for i in 0..new_layout.size() {
            assert_eq!(
                q.add(i).read(),
                (255 - i) as u8,
                "byte {i} was not preserved"
            );
        }

        G.dealloc(q, new_layout);
    }
}

#[test]
fn hosted_lazy_realloc_can_grow_into_system_fallback() {
    static G: HostedLazyGlobalWfSpanAllocator<C, 1> = HostedLazyGlobalWfSpanAllocator::new(1, 8);

    let old_layout = Layout::from_size_align(128, 64).unwrap();
    let new_layout = Layout::from_size_align(SPAN_SIZE * 5, 64).unwrap();

    // SAFETY: GlobalAlloc contract upheld manually in this test. The large
    // resized allocation exceeds wfspan's one-span huge class and exercises the
    // System fallback used by allocate-copy-free realloc.
    unsafe {
        let p = G.alloc(old_layout);
        assert!(!p.is_null());
        for i in 0..old_layout.size() {
            p.add(i).write((i ^ 0x5A) as u8);
        }

        let before_system = G.system_allocations();
        let q = G.realloc(p, old_layout, new_layout.size());
        assert!(!q.is_null());
        assert_eq!((q as usize) % new_layout.align(), 0);
        assert_eq!(G.system_allocations(), before_system + 1);
        for i in 0..old_layout.size() {
            assert_eq!(
                q.add(i).read(),
                (i ^ 0x5A) as u8,
                "byte {i} was not preserved"
            );
        }

        q.add(old_layout.size())
            .write_bytes(0x4B, new_layout.size() - old_layout.size());
        G.dealloc(q, new_layout);
    }
}

#[test]
fn hosted_lazy_reuses_tokens_after_thread_exit() {
    static G: HostedLazyGlobalWfSpanAllocator<C> = HostedLazyGlobalWfSpanAllocator::new(1, 4);
    let layout = Layout::from_size_align(64, 8).unwrap();

    for _ in 0..8 {
        std::thread::spawn(move || {
            // SAFETY: GlobalAlloc contract upheld manually in this test.
            unsafe {
                let p = G.alloc(layout);
                assert!(!p.is_null());
                G.dealloc(p, layout);
            }
        })
        .join()
        .unwrap();
    }

    assert_eq!(G.shard_count(), 1);
    assert_eq!(G.user_tokens_in_use(), 0);
}

#[test]
fn hosted_lazy_cross_thread_free_after_allocating_thread_exit_reuses_shard() {
    static G: HostedLazyGlobalWfSpanAllocator<C> = HostedLazyGlobalWfSpanAllocator::new(1, 4);
    let layout = Layout::from_size_align(64, 8).unwrap();

    let addr = std::thread::spawn(move || {
        // SAFETY: GlobalAlloc contract upheld manually in this test.
        unsafe {
            let p = G.alloc(layout);
            assert!(!p.is_null());
            p.write_bytes(0xA5, layout.size());
            p as usize
        }
    })
    .join()
    .unwrap();

    assert_eq!(G.shard_count(), 1);
    assert_eq!(G.user_tokens_in_use(), 0);

    // SAFETY: the allocating thread has exited and released its reusable token;
    // this free exercises the wrapper's cross-thread service-token path.
    unsafe { G.dealloc(addr as *mut u8, layout) };

    let addr2 = std::thread::spawn(move || {
        // SAFETY: GlobalAlloc contract upheld manually in this test.
        unsafe {
            let p = G.alloc(layout);
            assert!(!p.is_null());
            p as usize
        }
    })
    .join()
    .unwrap();

    assert_eq!(G.shard_count(), 1);
    assert_eq!(G.user_tokens_in_use(), 0);
    // SAFETY: freed once, from a different thread than the allocating thread.
    unsafe { G.dealloc(addr2 as *mut u8, layout) };
}

#[test]
fn hosted_lazy_service_token_counts_cross_shard_frees_after_thread_exit() {
    static G: HostedLazyGlobalWfSpanAllocator<C> = HostedLazyGlobalWfSpanAllocator::new(1, 4);
    const THREADS: usize = 6;

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
                    p.write_bytes(0xC7, layout.size());
                    barrier.wait();
                    p as usize
                }
            })
        })
        .collect();

    barrier.wait();
    assert!(G.shard_count() >= THREADS);
    assert_eq!(G.user_tokens_in_use(), THREADS);

    let ptrs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(G.user_tokens_in_use(), 0);

    let before = G.stats().service_token_deallocations;
    for addr in ptrs {
        // SAFETY: each pointer is freed exactly once after its allocating
        // thread exited, so the freeing thread must use each shard's reserved
        // service token.
        unsafe { G.dealloc(addr as *mut u8, layout) };
    }
    let after = G.stats().service_token_deallocations;
    assert!(after > before);
    assert!(after - before <= THREADS);
}

#[test]
fn hosted_lazy_adds_shards_for_concurrent_threads() {
    static G: HostedLazyGlobalWfSpanAllocator<C> = HostedLazyGlobalWfSpanAllocator::new(1, 4);
    let layout = Layout::from_size_align(64, 8).unwrap();
    let barrier = Arc::new(Barrier::new(5));

    let handles: Vec<_> = (0..4)
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
    assert!(G.shard_count() >= 4);
    assert_eq!(G.user_tokens_in_use(), 4);
    barrier.wait();

    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(G.user_tokens_in_use(), 0);
}

#[test]
fn hosted_lazy_grows_region_for_large_request() {
    static G: HostedLazyGlobalWfSpanAllocator<C> = HostedLazyGlobalWfSpanAllocator::new(1, 1);
    let layout = Layout::from_size_align(SPAN_SIZE * 3, 8).unwrap();

    // SAFETY: GlobalAlloc contract upheld manually in this test.
    unsafe {
        let p = G.alloc(layout);
        assert!(!p.is_null());
        p.write_bytes(0xCD, layout.size());
        G.dealloc(p, layout);
    }
    assert!(G.shard_count() >= 2);
}

#[test]
fn hosted_lazy_reports_system_fallback_allocations() {
    static G: HostedLazyGlobalWfSpanAllocator<C, 1> = HostedLazyGlobalWfSpanAllocator::new(1, 8);
    let layout = Layout::from_size_align(SPAN_SIZE * 5, 8).unwrap();

    assert_eq!(G.system_allocations(), 0);
    // SAFETY: GlobalAlloc contract upheld manually in this test. With a
    // one-span huge granule, five spans exceeds wfspan's largest huge class
    // and must fall back to System without requiring an enormous allocation.
    unsafe {
        let p = G.alloc(layout);
        assert!(!p.is_null());
        assert_eq!(G.system_allocations(), 1);
        G.dealloc(p, layout);
    }
}

#[test]
fn hosted_lazy_preserves_alignment_with_hidden_header() {
    static G: HostedLazyGlobalWfSpanAllocator<C> = HostedLazyGlobalWfSpanAllocator::new(2, 8);

    for (size, align) in [(1, 1), (17, 16), (128, 64), (513, 256), (4097, 4096)] {
        let layout = Layout::from_size_align(size, align).unwrap();
        // SAFETY: GlobalAlloc contract upheld manually in this test.
        unsafe {
            let p = G.alloc(layout);
            assert!(
                !p.is_null(),
                "allocation failed for size={size} align={align}"
            );
            assert_eq!(
                (p as usize) % align,
                0,
                "misaligned size={size} align={align}"
            );
            p.write_bytes(0x5A, size);
            G.dealloc(p, layout);
        }
    }
}

#[test]
#[cfg(debug_assertions)]
fn hosted_lazy_debug_asserts_on_bad_header_magic() {
    static G: HostedLazyGlobalWfSpanAllocator<C> = HostedLazyGlobalWfSpanAllocator::new(1, 4);
    let layout = Layout::from_size_align(64, 8).unwrap();

    // SAFETY: GlobalAlloc contract upheld manually except for deliberate,
    // debug-only header corruption. The header is repaired before final free.
    unsafe {
        let p = G.alloc(layout);
        assert!(!p.is_null());
        let header = global_header_words(p);
        let original_magic = *header.add(0);
        *header.add(0) = 0;

        let result = catch_unwind(AssertUnwindSafe(|| G.dealloc(p, layout)));
        assert!(result.is_err());

        *header.add(0) = original_magic;
        G.dealloc(p, layout);
    }
}

#[test]
#[cfg(debug_assertions)]
fn hosted_lazy_debug_asserts_on_unknown_header_backend() {
    static G: HostedLazyGlobalWfSpanAllocator<C, 1> = HostedLazyGlobalWfSpanAllocator::new(1, 8);
    let layout = Layout::from_size_align(SPAN_SIZE * 5, 8).unwrap();

    // SAFETY: GlobalAlloc contract upheld manually except for deliberate,
    // debug-only header corruption. Use the System fallback backend so repair
    // and final free do not depend on wfspan shard internals after unwinding.
    unsafe {
        let p = G.alloc(layout);
        assert!(!p.is_null());
        let header = global_header_words(p);
        assert_eq!(*header.add(0), GLOBAL_HEADER_MAGIC);
        assert_eq!(*header.add(1), BACKEND_SYSTEM);
        *header.add(1) = usize::MAX;

        let result = catch_unwind(AssertUnwindSafe(|| G.dealloc(p, layout)));
        assert!(result.is_err());

        *header.add(1) = BACKEND_SYSTEM;
        G.dealloc(p, layout);
    }
}
