//! Throughput + latency percentiles for local alloc/free, single- and
//! multi-threaded. Run: `cargo bench --bench alloc_free`.

use std::alloc::Layout;
use std::time::Instant;

use wf_alloc::WfSpanAllocator;
use wf_alloc::region::OwnedRegion;

const N: usize = 8;
const C: usize = 8;
const OPS: usize = 200_000;

fn percentiles(mut ns: Vec<u64>, label: &str) {
    ns.sort_unstable();
    let p = |q: f64| ns[((ns.len() - 1) as f64 * q) as usize];
    println!(
        "{label}: ops={} p50={}ns p90={}ns p99={}ns p99.9={}ns p99.99={}ns max={}ns",
        ns.len(),
        p(0.50),
        p(0.90),
        p(0.99),
        p(0.999),
        p(0.9999),
        ns[ns.len() - 1]
    );
}

fn main() {
    let region = OwnedRegion::new(512);
    let alloc = Box::leak(Box::new(WfSpanAllocator::<N, C>::new()));
    // SAFETY: init once, before sharing; leaked box never moves.
    unsafe { alloc.init(region.ptr(), region.len()) };
    let layout = Layout::from_size_align(64, 8).unwrap();

    // Single-thread alloc/free pairs.
    let token = alloc.register_thread().unwrap();
    let mut lat = Vec::with_capacity(OPS);
    let start = Instant::now();
    for _ in 0..OPS {
        let t0 = Instant::now();
        // SAFETY: registered token, single thread.
        let p = unsafe { alloc.alloc_with_token(layout, token) };
        assert!(!p.is_null());
        // SAFETY: p was just allocated.
        unsafe { alloc.dealloc_with_token(p, layout, token) };
        lat.push(t0.elapsed().as_nanos() as u64);
    }
    let secs = start.elapsed().as_secs_f64();
    println!(
        "single-thread: {:.1} Mops/s",
        (OPS as f64 / secs) / 1_000_000.0
    );
    percentiles(lat, "single-thread alloc+free");

    // N-1 additional threads, local alloc/free.
    let alloc_ref: &'static WfSpanAllocator<N, C> = alloc;
    let handles: Vec<_> = (1..N)
        .map(|_| {
            std::thread::spawn(move || {
                let token = alloc_ref.register_thread().unwrap();
                let mut lat = Vec::with_capacity(OPS / 4);
                for _ in 0..OPS / 4 {
                    let t0 = Instant::now();
                    // SAFETY: per-thread token.
                    let p = unsafe { alloc_ref.alloc_with_token(layout, token) };
                    assert!(!p.is_null());
                    // SAFETY: just allocated.
                    unsafe { alloc_ref.dealloc_with_token(p, layout, token) };
                    lat.push(t0.elapsed().as_nanos() as u64);
                }
                lat
            })
        })
        .collect();
    let mut all = Vec::new();
    for h in handles {
        all.extend(h.join().unwrap());
    }
    percentiles(all, "multi-thread local alloc+free");
    std::mem::forget(region); // leaked allocator still references it
}
