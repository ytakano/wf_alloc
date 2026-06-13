//! Concurrent integration tests for the wait-free huge-slot path.
//!
//! Tiny granule (HUGE_GRANULE_SPANS = 1) so many threads contend over a
//! handful of directory slots (guide B.16): no two successful allocations
//! may overlap, losers must receive null in bounded steps, and
//! cross-thread free must return slots to FREE.

#![cfg(not(miri))]

use std::alloc::Layout;
use std::sync::{Barrier, mpsc};

use wf_alloc::region::OwnedRegion;
use wf_alloc::{
    MAX_HUGE_RUN_CLASSES, MAX_HUGE_RUNS_PER_CLASS, SPAN_SIZE, StepCounter, WfSpanAllocator,
};

const N: usize = 4;
const C: usize = 4;
const HG: usize = 1;
type HugeAlloc = WfSpanAllocator<C, HG>;

const GRANULE: usize = HG * SPAN_SIZE;

fn setup(spans: usize) -> (&'static HugeAlloc, &'static OwnedRegion) {
    let region = Box::leak(Box::new(OwnedRegion::new(spans)));
    let alloc = Box::leak(Box::new(HugeAlloc::new(N)));
    // SAFETY: init once, before sharing; both are leaked and never move.
    unsafe { alloc.init(region.ptr(), region.len()) };
    (alloc, region)
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// N threads hammer class-0 huge allocations over few slots. Tag integrity
/// proves no slot is ever handed to two threads; nulls are expected and
/// tolerated. Every operation asserts the huge step bounds (the structural
/// no-retry-loop proof, B.16 #8/#9).
#[test]
fn concurrent_huge_no_duplicate_slots() {
    // Directory capacity (28 spans) fits; threads still contend per class.
    let (alloc, _region) = setup(32);
    let barrier: &'static Barrier = Box::leak(Box::new(Barrier::new(N)));

    let handles: Vec<_> = (0..N)
        .map(|i| {
            std::thread::spawn(move || {
                let token = alloc.register_thread().unwrap();
                barrier.wait();
                let layout = Layout::from_size_align(GRANULE, 8).unwrap();
                let mut successes = 0u64;

                for round in 0..200u64 {
                    let mut step = StepCounter::new();
                    // SAFETY: per-thread token.
                    let p = unsafe { alloc.alloc_with_token_counted(layout, token, &mut step) };
                    step.assert_huge_bounds(MAX_HUGE_RUN_CLASSES, MAX_HUGE_RUNS_PER_CLASS);
                    if p.is_null() {
                        // All slots busy: bounded failure, try next round.
                        std::thread::yield_now();
                        continue;
                    }
                    successes += 1;
                    let tag = (i as u64) << 48 | round;
                    // SAFETY: payload is GRANULE bytes; we hold it
                    // exclusively until the free below.
                    unsafe {
                        (p as *mut u64).write(tag);
                        p.add(GRANULE - 1).write(i as u8);
                        assert_eq!((p as *const u64).read(), tag, "slot double-handed");
                        assert_eq!(p.add(GRANULE - 1).read(), i as u8);
                    }
                    let mut step = StepCounter::new();
                    // SAFETY: freed once.
                    unsafe { alloc.dealloc_with_token_counted(p, layout, token, &mut step) };
                    step.assert_huge_bounds(MAX_HUGE_RUN_CLASSES, MAX_HUGE_RUNS_PER_CLASS);
                }
                successes
            })
        })
        .collect();

    let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(total > 0, "at least some huge allocations must succeed");
    // SAFETY: all threads joined; quiescent.
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// Producer allocates huge runs; a different thread frees them (the
/// directory is global — no per-thread ownership), returning slots to
/// FREE for further allocation.
#[test]
fn cross_thread_huge_free() {
    let (alloc, _region) = setup(32);
    let (tx, rx) = mpsc::channel::<usize>();

    let producer = std::thread::spawn(move || {
        let token = alloc.register_thread().unwrap();
        let layout = Layout::from_size_align(GRANULE, 8).unwrap();
        let mut sent = 0u64;
        for j in 0..200u64 {
            // SAFETY: per-thread token.
            let p = unsafe { alloc.alloc_with_token(layout, token) };
            if p.is_null() {
                // All slots live in the channel backlog; wait for frees.
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
        let layout = Layout::from_size_align(GRANULE, 8).unwrap();
        let mut freed = 0u64;
        while let Ok(addr) = rx.recv() {
            let p = addr as *mut u8;
            // SAFETY: freed exactly once; any registered thread may free a
            // huge run (global directory).
            unsafe { alloc.dealloc_with_token(p, layout, token) };
            freed += 1;
        }
        freed
    });

    let sent = producer.join().unwrap();
    let freed = consumer.join().unwrap();
    assert_eq!(sent, freed, "every sent run must be freed");
    assert!(freed > 0);
    // SAFETY: quiescent.
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

/// All three paths under concurrency from one region: small, large, and
/// huge threads run simultaneously without corrupting each other.
#[test]
fn mixed_three_paths_concurrent() {
    let (alloc, _region) = setup(64);
    let barrier: &'static Barrier = Box::leak(Box::new(Barrier::new(N)));

    let handles: Vec<_> = (0..N)
        .map(|i| {
            std::thread::spawn(move || {
                let token = alloc.register_thread().unwrap();
                barrier.wait();
                let layout = match i % 3 {
                    0 => Layout::from_size_align(64, 8).unwrap(),   // small
                    1 => Layout::from_size_align(4096, 8).unwrap(), // large
                    _ => Layout::from_size_align(GRANULE, 8).unwrap(), // huge
                };
                for round in 0..200u64 {
                    // SAFETY: per-thread token.
                    let p = unsafe { alloc.alloc_with_token(layout, token) };
                    if p.is_null() {
                        std::thread::yield_now();
                        continue;
                    }
                    let tag = (i as u64) << 48 | round;
                    // SAFETY: every payload is at least 8 bytes.
                    unsafe { (p as *mut u64).write(tag) };
                    assert_eq!(unsafe { (p as *const u64).read() }, tag);
                    // SAFETY: freed once.
                    unsafe { alloc.dealloc_with_token(p, layout, token) };
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
