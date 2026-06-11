//! Remote-free-heavy workload: producers allocate, a consumer frees
//! (cross-thread), exercising the MPSC remote lists and claim path.
//! Run: `cargo bench --bench remote_free`.

use std::alloc::Layout;
use std::sync::mpsc;
use std::time::Instant;

use wf_alloc::WfSpanAllocator;
use wf_alloc::region::OwnedRegion;

const N: usize = 8;
const C: usize = 8;
const OPS: usize = 100_000;

fn main() {
    let region = OwnedRegion::new(1024);
    let alloc = Box::leak(Box::new(WfSpanAllocator::<N, C>::new()));
    // SAFETY: init once before sharing; leaked box never moves.
    unsafe { alloc.init(region.ptr(), region.len()) };
    let alloc_ref: &'static WfSpanAllocator<N, C> = alloc;
    let layout = Layout::from_size_align(64, 8).unwrap();

    let (tx, rx) = mpsc::sync_channel::<usize>(4096);
    let start = Instant::now();

    let producers: Vec<_> = (0..N - 1)
        .map(|_| {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let token = alloc_ref.register_thread().unwrap();
                for _ in 0..OPS / (N - 1) {
                    // SAFETY: per-thread token.
                    let p = unsafe { alloc_ref.alloc_with_token(layout, token) };
                    if p.is_null() {
                        break; // exhaustion is a valid bounded outcome
                    }
                    tx.send(p as usize).unwrap();
                }
            })
        })
        .collect();
    drop(tx);

    let consumer = std::thread::spawn(move || {
        let token = alloc_ref.register_thread().unwrap();
        let mut lat = Vec::new();
        while let Ok(addr) = rx.recv() {
            let t0 = Instant::now();
            // SAFETY: pointer produced by this allocator, freed once.
            unsafe { alloc_ref.dealloc_with_token(addr as *mut u8, layout, token) };
            lat.push(t0.elapsed().as_nanos() as u64);
        }
        lat
    });

    for p in producers {
        p.join().unwrap();
    }
    let mut lat = consumer.join().unwrap();
    let secs = start.elapsed().as_secs_f64();
    lat.sort_unstable();
    let p = |q: f64| lat[((lat.len() - 1) as f64 * q) as usize];
    println!(
        "remote-free: {:.2} Mops/s p50={}ns p99={}ns p99.99={}ns max={}ns",
        (lat.len() as f64 / secs) / 1_000_000.0,
        p(0.5),
        p(0.99),
        p(0.9999),
        lat[lat.len() - 1]
    );
    println!(
        "stats: claimed={} discarded={} blocked={}",
        alloc_ref
            .stats
            .claimed_spans
            .load(std::sync::atomic::Ordering::Relaxed),
        alloc_ref
            .stats
            .discarded_spans
            .load(std::sync::atomic::Ordering::Relaxed),
        alloc_ref
            .stats
            .remote_blocked_events
            .load(std::sync::atomic::Ordering::Relaxed),
    );
    std::mem::forget(region);
}
