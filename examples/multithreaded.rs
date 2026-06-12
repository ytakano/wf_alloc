//! Standard-library example: multi-threaded wait-free allocation.
//!
//! Demonstrates:
//!  1. Allocator setup with `OwnedRegion` and `Box::leak`
//!  2. Per-thread local alloc / dealloc with step-counter verification
//!  3. Producer-consumer remote-free pattern (N-1 producers, 1 consumer)
//!  4. Quiescent-state invariant check after all threads exit
//!
//! Run with: `cargo run --example multithreaded`

use std::alloc::Layout;
use std::sync::mpsc;

use wf_alloc::region::OwnedRegion;
use wf_alloc::size_class::{blocks_per_span, class_to_size};
use wf_alloc::{
    HELP_BUDGET_H, LOCAL_SPAN_LIMIT_K, MAX_SUPPORTED_CLASSES, StepCounter, WfSpanAllocator,
};

// 4 threads; the size classes (16 B – 16 KiB) and the huge granule use
// their defaults, so only N needs to be specified.
const N: usize = 4;
const C: usize = MAX_SUPPORTED_CLASSES;

fn main() {
    local_alloc_free();
    remote_free();
}

// ── Part 1: each thread independently allocates and frees its own memory ──────

fn local_alloc_free() {
    // Pin the allocator in a leaked Box so it lives for the duration of the
    // program and its address never changes after init().
    let region = Box::leak(Box::new(OwnedRegion::new(64)));
    let alloc: &'static WfSpanAllocator<N> =
        Box::leak(Box::new(WfSpanAllocator::new()));
    // Safety: called once before any thread touches the allocator;
    // the leaked allocation guarantees the allocator never moves.
    unsafe { alloc.init(region.ptr(), region.len()) };

    // blocks_per_span for class 0 (16-byte blocks) gives the tightest bound.
    let bps = blocks_per_span(class_to_size(0));

    let handles: Vec<_> = (0..N)
        .map(|i| {
            std::thread::spawn(move || {
                // Each thread registers once to obtain an exclusive token.
                let token = alloc.register_thread().unwrap();
                let layout =
                    Layout::from_size_align(class_to_size(i % C), 8).unwrap();

                for round in 0..500u64 {
                    let mut ptrs: Vec<(*mut u8, u64)> = Vec::with_capacity(32);

                    // Allocate a batch and write a canary value into each block.
                    for j in 0..32u64 {
                        let mut step = StepCounter::new();
                        // Safety: token is used exclusively by this thread.
                        let p = unsafe {
                            alloc.alloc_with_token_counted(layout, token, &mut step)
                        };
                        // Assert that this single operation stayed within the
                        // wait-freedom step bounds from the paper.
                        step.assert_bounds(N, HELP_BUDGET_H, N, bps, LOCAL_SPAN_LIMIT_K);
                        assert!(!p.is_null(), "pool exhausted unexpectedly");

                        // Encode thread id, round, and index into a 64-bit tag.
                        let tag = (token.id as u64) << 48 | round << 16 | j;
                        // Safety: every block is at least MIN_BLOCK_SIZE (16) bytes.
                        unsafe { (p as *mut u64).write(tag) };
                        ptrs.push((p, tag));
                    }

                    // Verify each tag is still intact, then free the block.
                    // A corrupted tag would indicate a double-allocation bug.
                    for (p, tag) in ptrs {
                        // Safety: block is still exclusively owned by this thread.
                        unsafe { assert_eq!((p as *const u64).read(), tag) };
                        // Safety: freed exactly once.
                        unsafe { alloc.dealloc_with_token(p, layout, token) };
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    // Walk every list and verify all invariants hold at quiescence.
    // Safety: all threads have joined; no concurrent operations are in flight.
    unsafe { wf_alloc::verify::check_quiescent(alloc) };

    println!("local_alloc_free: ok");
}

// ── Part 2: producers allocate; a single consumer frees remotely ──────────────

fn remote_free() {
    let region = Box::leak(Box::new(OwnedRegion::new(128)));
    let alloc: &'static WfSpanAllocator<N> =
        Box::leak(Box::new(WfSpanAllocator::new()));
    // Safety: as above.
    unsafe { alloc.init(region.ptr(), region.len()) };

    let layout = Layout::from_size_align(64, 8).unwrap();

    // N-1 producer threads allocate blocks and ship raw pointers to the
    // consumer via an mpsc channel.  The consumer frees every block using its
    // own token — this exercises the O(1) remote-free path (SWAP + FAA).
    let (tx, rx) = mpsc::channel::<usize>();

    let producers: Vec<_> = (0..N - 1)
        .map(|_| {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let token = alloc.register_thread().unwrap();
                for _ in 0..2_000 {
                    // Safety: per-thread token.
                    let p = unsafe { alloc.alloc_with_token(layout, token) };
                    if p.is_null() {
                        // Pool temporarily exhausted (consumer is lagging);
                        // yield and retry on the next iteration.
                        std::thread::yield_now();
                        continue;
                    }
                    tx.send(p as usize).unwrap();
                }
            })
        })
        .collect();
    drop(tx); // close the sender side so the consumer loop terminates

    let consumer = std::thread::spawn(move || {
        let token = alloc.register_thread().unwrap();
        let mut count = 0usize;
        while let Ok(addr) = rx.recv() {
            // Safety: freed exactly once; any registered thread may free any
            // pointer from this allocator — remote frees are always O(1) safe.
            unsafe { alloc.dealloc_with_token(addr as *mut u8, layout, token) };
            count += 1;
        }
        count
    });

    for h in producers {
        h.join().unwrap();
    }
    let freed = consumer.join().unwrap();
    assert!(freed > 0);

    // Safety: all threads have joined.
    unsafe { wf_alloc::verify::check_quiescent(alloc) };

    println!("remote_free: freed {freed} blocks remotely, ok");
}
