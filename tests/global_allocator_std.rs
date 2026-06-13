//! Production-like std workload coverage with HostedLazyGlobalWfSpanAllocator
//! installed as the test binary's global allocator.

#![cfg(feature = "global")]

use std::collections::HashMap;
use std::sync::mpsc;

use wf_alloc::global::{GlobalAllocatorConfig, HostedLazyGlobalWfSpanAllocator};

#[global_allocator]
static ALLOC: HostedLazyGlobalWfSpanAllocator =
    HostedLazyGlobalWfSpanAllocator::with_config(GlobalAllocatorConfig::new(4, 64));

#[test]
fn global_allocator_handles_std_collection_thread_churn() {
    for wave in 0..12usize {
        let handles: Vec<_> = (0..8usize)
            .map(|worker| {
                std::thread::spawn(move || {
                    let mut values = Vec::with_capacity(256);
                    for i in 0..256usize {
                        values.push((wave, worker, i));
                    }

                    let mut map = HashMap::with_capacity(128);
                    for (idx, value) in values.iter().enumerate() {
                        map.insert(format!("{worker}:{idx}"), *value);
                    }

                    let boxed = Box::new([worker as u8; 1024]);
                    assert_eq!(boxed[0], worker as u8);
                    assert_eq!(map.len(), values.len());
                    map.values().map(|(_, _, i)| *i).sum::<usize>()
                })
            })
            .collect();

        let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert!(total > 0);
    }

    assert!(ALLOC.stats().shard_count >= 1);
}

#[test]
fn global_allocator_handles_channel_handoff_and_remote_drops() {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let producers: Vec<_> = (0..4usize)
        .map(|producer| {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for msg in 0..128usize {
                    let mut payload = Vec::with_capacity(2048);
                    payload.resize(2048, (producer ^ msg) as u8);
                    tx.send(payload).unwrap();
                }
            })
        })
        .collect();
    drop(tx);

    let consumer = std::thread::spawn(move || {
        let mut count = 0usize;
        let mut checksum = 0usize;
        while let Ok(payload) = rx.recv() {
            checksum = checksum.wrapping_add(payload[0] as usize);
            count += 1;
        }
        (count, checksum)
    });

    for p in producers {
        p.join().unwrap();
    }
    let (count, checksum) = consumer.join().unwrap();
    assert_eq!(count, 4 * 128);
    assert!(checksum > 0);
}

#[test]
fn global_allocator_survives_mixed_longer_workload() {
    let handles: Vec<_> = (0..6usize)
        .map(|worker| {
            std::thread::spawn(move || {
                let mut checksum = 0usize;
                for iter in 0..400usize {
                    let len = 16 + ((worker * 31 + iter * 17) % 4096);
                    let mut bytes = Vec::with_capacity(len);
                    bytes.resize(len, (iter & 0xFF) as u8);
                    bytes.reserve(len / 2 + 1);

                    let text = format!("worker={worker} iter={iter} len={len}");
                    let boxed = Box::new((text, bytes));
                    checksum = checksum.wrapping_add(boxed.0.len());
                    checksum = checksum.wrapping_add(boxed.1[0] as usize);
                }
                checksum
            })
        })
        .collect();

    let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(total > 0);
    assert!(ALLOC.stats().wfspan_allocations > 0);
}

#[test]
#[ignore = "longer global allocator soak; run explicitly before stabilization"]
fn global_allocator_ignored_soak_many_threads() {
    let handles: Vec<_> = (0..8usize)
        .map(|worker| {
            std::thread::spawn(move || {
                let mut checksum = 0usize;
                for iter in 0..2_000usize {
                    let len = 1 + ((worker * 131 + iter * 257) % 16_384);
                    let mut bytes = Vec::with_capacity(len);
                    bytes.resize(len, (worker ^ iter) as u8);
                    if iter % 3 == 0 {
                        bytes.reserve(len / 3 + 17);
                    }

                    let mut map = HashMap::with_capacity(16);
                    for j in 0..16usize {
                        map.insert(format!("{worker}:{iter}:{j}"), bytes[j % bytes.len()]);
                    }
                    checksum = checksum.wrapping_add(map.len());
                    checksum = checksum.wrapping_add(bytes[0] as usize);
                }
                checksum
            })
        })
        .collect();

    let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(total > 0);
    assert_eq!(ALLOC.stats().shard_creation_failures, 0);
}
