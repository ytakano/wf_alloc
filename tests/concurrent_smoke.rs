//! Concurrent smoke tests: N threads with local, cross-thread (remote-free),
//! and mixed-size workloads, with pattern checks against double allocation
//! and a quiescent invariant sweep at the end. Step bounds asserted per op.

#![cfg(not(miri))]

use std::alloc::Layout;
use std::sync::Barrier;
use std::sync::mpsc;

use wf_alloc::WfSpanAllocator;
use wf_alloc::region::OwnedRegion;
use wf_alloc::size_class::blocks_per_span;
use wf_alloc::stats::StepCounter;
use wf_alloc::{HELP_BUDGET_H, LOCAL_SPAN_LIMIT_K, class_to_size};

const N: usize = 4;
const C: usize = 4;

fn setup(spans: usize) -> (&'static WfSpanAllocator<N, C>, &'static OwnedRegion) {
    let region = Box::leak(Box::new(OwnedRegion::new(spans)));
    let alloc = Box::leak(Box::new(WfSpanAllocator::<N, C>::new()));
    // SAFETY: init once before sharing; leaked allocations never move.
    unsafe { alloc.init(region.ptr(), region.len()) };
    (alloc, region)
}

#[test]
fn local_alloc_free_all_threads() {
    let (alloc, _region) = setup(64);
    let barrier: &'static Barrier = Box::leak(Box::new(Barrier::new(N)));
    let bps = blocks_per_span(class_to_size(0));

    let handles: Vec<_> = (0..N)
        .map(|i| {
            std::thread::spawn(move || {
                let token = alloc.register_thread().unwrap();
                barrier.wait();
                let layout = Layout::from_size_align(class_to_size(i % C), 8).unwrap();
                for round in 0..200u64 {
                    let mut ptrs = Vec::new();
                    for j in 0..64u64 {
                        let mut step = StepCounter::new();
                        // SAFETY: per-thread token.
                        let p = unsafe {
                            alloc.alloc_with_token_counted(layout, token, &mut step)
                        };
                        step.assert_bounds(N, HELP_BUDGET_H, N, bps, LOCAL_SPAN_LIMIT_K);
                        assert!(!p.is_null());
                        let tag = (token.id as u64) << 48 | round << 16 | j;
                        // SAFETY: block is at least 16 bytes.
                        unsafe { (p as *mut u64).write(tag) };
                        ptrs.push((p, tag));
                    }
                    for (p, tag) in ptrs {
                        // SAFETY: pattern must be intact (no double alloc).
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

#[test]
fn producer_consumer_remote_free_heavy() {
    let (alloc, _region) = setup(128);
    let layout = Layout::from_size_align(64, 8).unwrap();
    let bps = blocks_per_span(64);

    // N-1 producers send allocations to 1 consumer, which frees them all
    // remotely. Exercises MPSC lists, discard/claim, and SPMC recycling.
    let (tx, rx) = mpsc::channel::<(usize, u64)>();
    let producers: Vec<_> = (0..N - 1)
        .map(|_| {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let token = alloc.register_thread().unwrap();
                for j in 0..20_000u64 {
                    let mut step = StepCounter::new();
                    // SAFETY: per-thread token.
                    let p =
                        unsafe { alloc.alloc_with_token_counted(layout, token, &mut step) };
                    step.assert_bounds(N, HELP_BUDGET_H, N, bps, LOCAL_SPAN_LIMIT_K);
                    if p.is_null() {
                        // Bounded pool + consumer lag can exhaust; that is
                        // a valid outcome, just back off.
                        std::thread::yield_now();
                        continue;
                    }
                    let tag = (token.id as u64) << 48 | j;
                    // SAFETY: 64-byte block.
                    unsafe { (p as *mut u64).write(tag) };
                    tx.send((p as usize, tag)).unwrap();
                }
            })
        })
        .collect();
    drop(tx);

    let consumer = std::thread::spawn(move || {
        let token = alloc.register_thread().unwrap();
        let mut freed = 0u64;
        while let Ok((addr, tag)) = rx.recv() {
            let p = addr as *mut u8;
            // SAFETY: producer wrote this tag; intact pattern means no
            // double allocation happened.
            unsafe { assert_eq!((p as *const u64).read(), tag) };
            let mut step = StepCounter::new();
            // SAFETY: freed once (remote path).
            unsafe { alloc.dealloc_with_token_counted(p, layout, token, &mut step) };
            assert!(step.swap_ops <= 2 && step.faa_ops <= 1 && step.cas_attempts <= 1);
            freed += 1;
        }
        freed
    });

    for h in producers {
        h.join().unwrap();
    }
    let freed = consumer.join().unwrap();
    assert!(freed > 0);
    // SAFETY: all threads joined; quiescent.
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

#[test]
fn mixed_sizes_with_crossing_frees() {
    let (alloc, _region) = setup(128);
    let barrier: &'static Barrier = Box::leak(Box::new(Barrier::new(N)));

    // Ring of channels: thread i frees what thread i-1 allocated.
    let mut txs = Vec::new();
    let mut rxs = Vec::new();
    for _ in 0..N {
        let (tx, rx) = mpsc::channel::<(usize, Layout)>();
        txs.push(tx);
        rxs.push(rx);
    }
    let txs: &'static [mpsc::Sender<(usize, Layout)>] = Box::leak(txs.into_boxed_slice());

    let handles: Vec<_> = rxs
        .into_iter()
        .enumerate()
        .map(|(i, rx)| {
            std::thread::spawn(move || {
                let token = alloc.register_thread().unwrap();
                barrier.wait();
                let next = &txs[(i + 1) % N];
                for j in 0..10_000usize {
                    let class = j % C;
                    let layout =
                        Layout::from_size_align(class_to_size(class), 8).unwrap();
                    // SAFETY: per-thread token.
                    let p = unsafe { alloc.alloc_with_token(layout, token) };
                    if !p.is_null() {
                        next.send((p as usize, layout)).unwrap();
                    }
                    // Drain whatever arrived for us.
                    while let Ok((addr, l)) = rx.try_recv() {
                        // SAFETY: freed once by the ring neighbor.
                        unsafe { alloc.dealloc_with_token(addr as *mut u8, l, token) };
                    }
                }
                // Final drain after all sends complete is done post-join.
                rx
            })
        })
        .collect();

    let leftovers: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    // Free the remainders single-threadedly with a fresh borrow of tokens
    // not possible (registry exhausted) — reuse thread 0's token id 0..N-1
    // is unsafe across threads, but we are single-threaded now.
    // SAFETY: quiescent; raw token reuse is safe single-threaded.
    let token = unsafe { alloc.registry.token_from_raw(0) };
    for rx in leftovers {
        while let Ok((addr, l)) = rx.try_recv() {
            // SAFETY: freed once.
            unsafe { alloc.dealloc_with_token(addr as *mut u8, l, token) };
        }
    }
    // SAFETY: quiescent.
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}
