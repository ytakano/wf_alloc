//! WCET-style measurement: maximum observed step counts (not just time)
//! under forced SPMC contention, plus footprint statistics.
//! Run: `cargo bench --bench wcet_like`.

use std::alloc::Layout;
use std::sync::atomic::Ordering;

use wf_alloc::stats::StepCounter;
use wf_alloc::WfSpanAllocator;
use wf_alloc::region::OwnedRegion;
use wf_alloc::size_class::blocks_per_span;
use wf_alloc::{HELP_BUDGET_H, LOCAL_SPAN_LIMIT_K, class_to_size};

const N: usize = 8;
const C: usize = 4;
const ROUNDS: usize = 20_000;

fn main() {
    let region = OwnedRegion::new(256);
    let alloc = Box::leak(Box::new(WfSpanAllocator::<N, C>::new()));
    // SAFETY: init once before sharing; leaked box never moves.
    unsafe { alloc.init(region.ptr(), region.len()) };
    let alloc_ref: &'static WfSpanAllocator<N, C> = alloc;
    let layout = Layout::from_size_align(class_to_size(0), 8).unwrap();
    let bps = blocks_per_span(class_to_size(0));

    // Threads allocate whole spans worth of blocks and free them remotely
    // crosswise, forcing span churn through SPMC lists and helping.
    let handles: Vec<_> = (0..N)
        .map(|_| {
            std::thread::spawn(move || {
                let token = alloc_ref.register_thread().unwrap();
                let mut max = StepCounter::new();
                let mut held: Vec<*mut u8> = Vec::with_capacity(bps);
                for _ in 0..ROUNDS {
                    let mut step = StepCounter::new();
                    // SAFETY: per-thread token.
                    let p = unsafe { alloc_ref.alloc_with_token_counted(layout, token, &mut step) };
                    step.assert_bounds(N, HELP_BUDGET_H, N, bps, LOCAL_SPAN_LIMIT_K);
                    max.help_steps = max.help_steps.max(step.help_steps);
                    max.query_steps = max.query_steps.max(step.query_steps);
                    max.cas2_attempts = max.cas2_attempts.max(step.cas2_attempts);
                    max.blocks_scanned = max.blocks_scanned.max(step.blocks_scanned);
                    if p.is_null() {
                        for q in held.drain(..) {
                            // SAFETY: allocated above, freed once.
                            unsafe { alloc_ref.dealloc_with_token(q, layout, token) };
                        }
                        continue;
                    }
                    held.push(p);
                    if held.len() >= bps {
                        for q in held.drain(..) {
                            // SAFETY: allocated above, freed once.
                            unsafe { alloc_ref.dealloc_with_token(q, layout, token) };
                        }
                    }
                }
                for q in held.drain(..) {
                    // SAFETY: allocated above, freed once.
                    unsafe { alloc_ref.dealloc_with_token(q, layout, token) };
                }
                max
            })
        })
        .collect();

    let mut max = StepCounter::new();
    for h in handles {
        let m = h.join().unwrap();
        max.help_steps = max.help_steps.max(m.help_steps);
        max.query_steps = max.query_steps.max(m.query_steps);
        max.cas2_attempts = max.cas2_attempts.max(m.cas2_attempts);
        max.blocks_scanned = max.blocks_scanned.max(m.blocks_scanned);
    }
    println!(
        "max steps per alloc: help={} query={} cas2={} blocks_scanned={}",
        max.help_steps, max.query_steps, max.cas2_attempts, max.blocks_scanned
    );
    println!(
        "footprint: pool_used={}/{} spans, published={} acquired={} helped_stash={} blocked={}",
        alloc_ref.pool.spans_used(),
        alloc_ref.pool.spans_total(),
        alloc_ref.stats.published_spans.load(Ordering::Relaxed),
        alloc_ref
            .stats
            .acquired_public_spans
            .load(Ordering::Relaxed),
        alloc_ref.stats.help_record_spans.load(Ordering::Relaxed),
        alloc_ref
            .stats
            .remote_blocked_events
            .load(Ordering::Relaxed),
    );
    println!(
        "theoretical extra bound: {} bytes",
        WfSpanAllocator::<N, C>::theoretical_extra_bound()
    );
    std::mem::forget(region);
}
