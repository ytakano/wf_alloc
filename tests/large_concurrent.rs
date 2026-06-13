//! Concurrent integration tests for the wait-free large-run path.
//!
//! Covers: concurrent alloc/free from N threads (same class), cross-thread
//! free (the deallocating thread becomes the run owner), mixed small+large
//! workloads from ONE region, and step-count bounds under contention (the
//! old Treiber-stack path would spin here; the run path must stay bounded).

#![cfg(not(miri))]

use std::alloc::Layout;
use std::sync::{Barrier, mpsc};

use wf_alloc::region::OwnedRegion;
use wf_alloc::{HELP_BUDGET_H, MAX_LARGE_RUN_CLASSES, StepCounter, WfSpanAllocator};

const N: usize = 4;
const C: usize = 4;

/// One large allocation per class-0 run: an oversized-for-C=4 payload.
/// class_to_size(3) = 128 bytes, so 4 KiB always dispatches large.
const LARGE_SIZE: usize = 4096;

fn setup(spans: usize) -> (&'static WfSpanAllocator<C>, &'static OwnedRegion) {
    let region = Box::leak(Box::new(OwnedRegion::new(spans)));
    let alloc = Box::leak(Box::new(WfSpanAllocator::<C>::new(N)));
    // SAFETY: init once, before sharing; both are leaked and never move.
    unsafe { alloc.init(region.ptr(), region.len()) };
    (alloc, region)
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// N threads concurrently alloc and free class-0 runs from one region.
/// Each thread writes a unique tag immediately after allocation and verifies
/// it is intact before freeing — any double-allocation would corrupt it.
#[test]
fn concurrent_large_alloc_free_same_class() {
    // 32 spans; peak demand is N threads × 4 runs = 16 runs.
    let (alloc, _region) = setup(32);
    let barrier: &'static Barrier = Box::leak(Box::new(Barrier::new(N)));

    let handles: Vec<_> = (0..N)
        .map(|i| {
            std::thread::spawn(move || {
                let token = alloc.register_thread().unwrap();
                barrier.wait();
                let layout = Layout::from_size_align(LARGE_SIZE, 8).unwrap();

                for round in 0..50u64 {
                    let mut batch = Vec::new();
                    for j in 0..4u64 {
                        // SAFETY: per-thread token.
                        let p = unsafe { alloc.alloc_with_token(layout, token) };
                        if p.is_null() {
                            // Pool may exhaust transiently; back off.
                            std::thread::yield_now();
                            continue;
                        }
                        let tag = (i as u64) << 48 | round << 16 | j;
                        // SAFETY: payload is at least 8 bytes.
                        unsafe { (p as *mut u64).write(tag) };
                        batch.push((p, tag));
                    }
                    for (p, tag) in batch {
                        // Pattern must be intact — no other thread may have
                        // received this run while we held it.
                        // SAFETY: we still hold the run.
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

/// Producer thread allocates runs; consumer thread frees them. Exercises
/// guide A.10: the DEALLOCATING thread becomes the run's owner, so the runs
/// end up in the consumer's heap (or its public list) — verified by
/// check_quiescent after joining.
#[test]
fn cross_thread_large_free() {
    let (alloc, _region) = setup(32);
    let (tx, rx) = mpsc::channel::<usize>();

    let producer = std::thread::spawn(move || {
        let token = alloc.register_thread().unwrap();
        let layout = Layout::from_size_align(LARGE_SIZE, 8).unwrap();
        let mut sent = 0u64;
        for j in 0..200u64 {
            // SAFETY: per-thread token.
            let p = unsafe { alloc.alloc_with_token(layout, token) };
            if p.is_null() {
                // Pool transiently full; yield and try next iteration.
                std::thread::yield_now();
                continue;
            }
            // SAFETY: payload is at least 8 bytes.
            unsafe { (p as *mut u64).write(j) };
            tx.send(p as usize).unwrap();
            sent += 1;
        }
        sent
    });

    let consumer = std::thread::spawn(move || {
        let token = alloc.register_thread().unwrap();
        let layout = Layout::from_size_align(LARGE_SIZE, 8).unwrap();
        let mut freed = 0u64;
        while let Ok(addr) = rx.recv() {
            let p = addr as *mut u8;
            // SAFETY: freed exactly once; the large path transfers the whole
            // run to the freeing thread (no owner check, O(1)).
            unsafe { alloc.dealloc_with_token(p, layout, token) };
            freed += 1;
        }
        freed
    });

    let sent = producer.join().unwrap();
    let freed = consumer.join().unwrap();
    assert_eq!(sent, freed, "every sent run must be freed");
    assert!(freed > 0, "at least one run must have been transferred");
    // SAFETY: quiescent.
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Even-indexed threads do small allocations; odd-indexed threads do large.
/// Both paths draw from the SAME region and must not corrupt each other.
#[test]
fn mixed_small_large_concurrent() {
    // Generous region; threads tolerate transient nulls (a large carve may
    // also transiently strand a tail at exhaustion — documented waste).
    let (alloc, _region) = setup(64);
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
                    let layout = Layout::from_size_align(LARGE_SIZE, 8).unwrap();
                    for round in 0..50u64 {
                        // SAFETY: per-thread token.
                        let p = unsafe { alloc.alloc_with_token(layout, token) };
                        if p.is_null() {
                            std::thread::yield_now();
                            continue;
                        }
                        let tag = (i as u64) << 48 | round;
                        // SAFETY: payload is at least 8 bytes.
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

/// Under full contention, EVERY large operation must stay within the
/// wait-freedom step bounds. The old Treiber-stack implementation had
/// unbounded CAS retries exactly here.
#[test]
fn step_counts_bounded_under_contention() {
    // Deliberately small pool so threads constantly contend over the public
    // run-lists and the helping protocol.
    let (alloc, _region) = setup(8);
    let barrier: &'static Barrier = Box::leak(Box::new(Barrier::new(N)));

    let handles: Vec<_> = (0..N)
        .map(|_| {
            std::thread::spawn(move || {
                let token = alloc.register_thread().unwrap();
                barrier.wait();
                let layout = Layout::from_size_align(LARGE_SIZE, 8).unwrap();

                for _ in 0..200 {
                    let mut step = StepCounter::new();
                    // SAFETY: per-thread token.
                    let p = unsafe { alloc.alloc_with_token_counted(layout, token, &mut step) };
                    step.assert_large_bounds(N, HELP_BUDGET_H, N, MAX_LARGE_RUN_CLASSES);
                    if p.is_null() {
                        std::thread::yield_now();
                        continue;
                    }
                    let mut step = StepCounter::new();
                    // SAFETY: freed once.
                    unsafe { alloc.dealloc_with_token_counted(p, layout, token, &mut step) };
                    step.assert_large_bounds(N, HELP_BUDGET_H, N, MAX_LARGE_RUN_CLASSES);
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
