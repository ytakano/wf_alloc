//! Hosted lazy `GlobalAlloc` example.
//!
//! This example installs `HostedLazyGlobalWfSpanAllocator` as the process
//! global allocator. The wrapper is std/hosted-only: it bootstraps itself on
//! first allocation by calling `std::alloc::System` directly for wf_alloc's
//! metadata and backing span regions. It creates additional wfspan shards on
//! demand and falls back to `System` for requests outside wfspan's range.
//!
//! Run with: `cargo run --features global --example global_wrapper`

#[cfg(feature = "global")]
use wf_alloc::global::HostedLazyGlobalWfSpanAllocator;

#[cfg(feature = "global")]
#[global_allocator]
static ALLOC: HostedLazyGlobalWfSpanAllocator = HostedLazyGlobalWfSpanAllocator::new(16, 1024);

#[cfg(feature = "global")]
fn main() {
    let mut values = Vec::with_capacity(256);
    for i in 0..256u64 {
        values.push(i * i);
    }

    let boxed = Box::new([0xA5u8; 4096]);
    assert_eq!(boxed[0], 0xA5);

    let handles: Vec<_> = (0..4)
        .map(|worker| {
            std::thread::spawn(move || {
                let mut local = Vec::with_capacity(128);
                for i in 0..128usize {
                    local.push(worker * 1000 + i);
                }
                local.iter().sum::<usize>()
            })
        })
        .collect();

    let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(total > 0);

    drop(boxed);
    drop(values);

    let inner = ALLOC.inner().expect("allocator initialized");
    println!(
        "Hosted lazy global allocator ok: shards={}, active_threads_per_first_shard={}, first_shard_spans_used={}, tokens_in_use={}",
        ALLOC.shard_count(),
        inner.active_threads(),
        inner.pool.spans_used(),
        ALLOC.user_tokens_in_use()
    );
}

#[cfg(not(feature = "global"))]
fn main() {
    eprintln!("enable the `global` feature: cargo run --features global --example global_wrapper");
}
