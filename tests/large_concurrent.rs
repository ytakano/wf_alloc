//! Concurrent integration tests for the large-object allocator path.
//!
//! Covers: concurrent alloc/free from N threads (same class), cross-thread
//! free (producer/consumer), and mixed small+large concurrent workloads.

#![cfg(not(miri))]

use std::alloc::Layout;
use std::sync::{Barrier, mpsc};

use wf_alloc::region::OwnedRegion;
use wf_alloc::{MIN_LARGE_SIZE, WfSpanAllocator};

const N: usize = 4;
const C: usize = 4;

fn setup(
    small_spans: usize,
    large_spans: usize,
) -> (
    &'static WfSpanAllocator<N, C>,
    &'static OwnedRegion,
    &'static OwnedRegion,
) {
    let small_region = Box::leak(Box::new(OwnedRegion::new(small_spans)));
    let large_region = Box::leak(Box::new(OwnedRegion::new(large_spans)));
    let alloc = Box::leak(Box::new(WfSpanAllocator::<N, C>::new()));
    // SAFETY: init once, before sharing; all three are leaked and never move.
    unsafe {
        alloc.init(small_region.ptr(), small_region.len());
        alloc.init_large(large_region.ptr(), large_region.len());
    }
    (alloc, small_region, large_region)
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// N threads concurrently alloc and free class-0 large blocks.
/// Each thread writes a unique tag immediately after allocation and verifies
/// it is intact before freeing — any double-allocation would corrupt the tag.
#[test]
fn concurrent_large_alloc_free_same_class() {
    // 32 spans = 2 MiB; peak concurrency is N × batch blocks × 32 KiB.
    let (alloc, _small, _large) = setup(4, 32);
    let barrier: &'static Barrier = Box::leak(Box::new(Barrier::new(N)));

    let handles: Vec<_> = (0..N)
        .map(|i| {
            std::thread::spawn(move || {
                let token = alloc.register_thread().unwrap();
                barrier.wait();
                let layout = Layout::from_size_align(MIN_LARGE_SIZE / 2, 8).unwrap();

                for round in 0..50u64 {
                    let mut batch = Vec::new();
                    for j in 0..4u64 {
                        // SAFETY: per-thread token.
                        let p = unsafe { alloc.alloc_with_token(layout, token) };
                        if p.is_null() {
                            // Pool may exhaust transiently; back off and retry.
                            std::thread::yield_now();
                            continue;
                        }
                        let tag = (i as u64) << 48 | round << 16 | j;
                        // SAFETY: block is at least 8 bytes.
                        unsafe { (p as *mut u64).write(tag) };
                        batch.push((p, tag));
                    }
                    for (p, tag) in batch {
                        // Pattern must be intact — no other thread may have
                        // received this block while we held it.
                        // SAFETY: we still hold the block.
                        unsafe { assert_eq!((p as *const u64).read(), tag) };
                        // SAFETY: freed once.
                        unsafe { alloc.dealloc_with_token(p, layout, token) };
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
    // SAFETY: all threads joined; quiescent.
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Producer thread allocates large blocks; consumer thread frees them.
/// Exercises the cross-thread (remote) free path for large objects.
#[test]
fn cross_thread_large_free() {
    // 32 spans (2 MiB) — channel backlog is bounded by recv speed; in
    // practice only a handful of blocks are live simultaneously.
    let (alloc, _small, _large) = setup(4, 32);
    let (tx, rx) = mpsc::channel::<usize>();

    let producer = std::thread::spawn(move || {
        let token = alloc.register_thread().unwrap();
        let layout = Layout::from_size_align(MIN_LARGE_SIZE / 2, 8).unwrap();
        let mut sent = 0u64;
        for j in 0..200u64 {
            // SAFETY: per-thread token.
            let p = unsafe { alloc.alloc_with_token(layout, token) };
            if p.is_null() {
                // Pool transiently full; yield and try next iteration.
                std::thread::yield_now();
                continue;
            }
            // SAFETY: block is at least 8 bytes.
            unsafe { (p as *mut u64).write(j) };
            tx.send(p as usize).unwrap();
            sent += 1;
        }
        sent
    });

    let consumer = std::thread::spawn(move || {
        let token = alloc.register_thread().unwrap();
        let layout = Layout::from_size_align(MIN_LARGE_SIZE / 2, 8).unwrap();
        let mut freed = 0u64;
        while let Ok(addr) = rx.recv() {
            let p = addr as *mut u8;
            // SAFETY: freed exactly once; remote free is safe across threads
            // for the large path (header-based, no owner check needed).
            unsafe { alloc.dealloc_with_token(p, layout, token) };
            freed += 1;
        }
        freed
    });

    let sent = producer.join().unwrap();
    let freed = consumer.join().unwrap();
    assert_eq!(sent, freed, "every sent block must be freed");
    assert!(freed > 0, "at least one block must have been transferred");
    // SAFETY: quiescent.
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Even-indexed threads do small allocations; odd-indexed threads do large.
/// Both must succeed and must not corrupt each other's data.
#[test]
fn mixed_small_large_concurrent() {
    // Generous regions to avoid spurious exhaustion.
    let (alloc, _small, _large) = setup(32, 32);
    let barrier: &'static Barrier = Box::leak(Box::new(Barrier::new(N)));

    let handles: Vec<_> = (0..N)
        .map(|i| {
            std::thread::spawn(move || {
                let token = alloc.register_thread().unwrap();
                barrier.wait();

                if i % 2 == 0 {
                    // Small allocation thread.
                    let layout = Layout::from_size_align(64, 8).unwrap();
                    for round in 0..200u64 {
                        // SAFETY: per-thread token.
                        let p = unsafe { alloc.alloc_with_token(layout, token) };
                        if p.is_null() {
                            continue; // span pool briefly exhausted; skip
                        }
                        let tag = (i as u64) << 48 | round;
                        // SAFETY: block is at least 8 bytes.
                        unsafe { (p as *mut u64).write(tag) };
                        assert_eq!(unsafe { (p as *const u64).read() }, tag);
                        // SAFETY: freed once.
                        unsafe { alloc.dealloc_with_token(p, layout, token) };
                    }
                } else {
                    // Large allocation thread.
                    let layout = Layout::from_size_align(MIN_LARGE_SIZE / 2, 8).unwrap();
                    for round in 0..50u64 {
                        // SAFETY: per-thread token.
                        let p = unsafe { alloc.alloc_with_token(layout, token) };
                        if p.is_null() {
                            std::thread::yield_now();
                            continue;
                        }
                        let tag = (i as u64) << 48 | round;
                        // SAFETY: block is at least 8 bytes.
                        unsafe { (p as *mut u64).write(tag) };
                        assert_eq!(unsafe { (p as *const u64).read() }, tag);
                        // SAFETY: freed once.
                        unsafe { alloc.dealloc_with_token(p, layout, token) };
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
    // SAFETY: quiescent.
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}
