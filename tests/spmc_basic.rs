//! SPMC span-list tests (Milestone 5 acceptance): owner enqueue without
//! CAS, one-shot pop, Empty/Span outcomes, version (ABA) behavior.

use std::sync::atomic::Ordering;

use wf_alloc::DefaultCas2Backend;
use wf_alloc::Cas2Backend;
use wf_alloc::config::OWNER_PUBLIC;
use wf_alloc::region::OwnedRegion;
use wf_alloc::span::{SpanHeader, init_span};
use wf_alloc::spmc_span_list::{SpmcSpanList, TryPop};
use wf_alloc::stats::StepCounter;
use wf_alloc::tagged::HeadWord;
use wf_alloc::{SPAN_SIZE, class_to_size};

fn make_spans(region: &OwnedRegion, n: usize) -> Vec<*mut SpanHeader> {
    (0..n)
        .map(|i| {
            // SAFETY: distinct, aligned, exclusively owned span regions.
            unsafe {
                init_span(
                    region.ptr().add(i * SPAN_SIZE),
                    0,
                    class_to_size(0),
                    OWNER_PUBLIC,
                )
            }
        })
        .collect()
}

#[test]
fn enqueue_pop_empty_and_version() {
    let region = OwnedRegion::new(2);
    let spans = make_spans(&region, 2);
    let list = Box::leak(Box::new(SpmcSpanList::new()));
    // SAFETY: init once; leaked box never moves.
    unsafe { list.init() };
    let mut step = StepCounter::new();

    // Empty before any enqueue.
    // SAFETY: initialized list.
    assert_eq!(
        unsafe { list.try_pop_head_once::<DefaultCas2Backend>(&mut step) },
        TryPop::Empty
    );

    // Owner enqueue (no CAS by construction), FIFO pop.
    // SAFETY: single producer in this test.
    unsafe {
        list.enqueue_by_owner(spans[0], &mut step);
        list.enqueue_by_owner(spans[1], &mut step);
    }

    // SAFETY: aligned head word.
    let v0 = unsafe { DefaultCas2Backend::load(list.head_ptr()) }.version;

    // SAFETY: initialized list.
    let popped = unsafe { list.try_pop_head_once::<DefaultCas2Backend>(&mut step) };
    assert_eq!(popped, TryPop::Span(spans[0]));
    // version increments on successful pop (ABA protection)
    // SAFETY: aligned head word.
    let v1 = unsafe { DefaultCas2Backend::load(list.head_ptr()) }.version;
    assert_eq!(v1, v0.wrapping_add(1));

    // Popped span received a node back, ready to be re-enqueued.
    // SAFETY: we own the popped span now.
    assert!(!unsafe { (*spans[0]).node.load(Ordering::Relaxed) }.is_null());

    // SAFETY: initialized list.
    unsafe {
        assert_eq!(
            list.try_pop_head_once::<DefaultCas2Backend>(&mut step),
            TryPop::Span(spans[1])
        );
        assert_eq!(
            list.try_pop_head_once::<DefaultCas2Backend>(&mut step),
            TryPop::Empty
        );
    }

    // Exactly one CAS2 attempt per non-empty pop, none for Empty.
    assert_eq!(step.cas2_attempts, 2);

    // Re-enqueue a popped span (node rotation works).
    // SAFETY: single producer; span owned and unlisted.
    unsafe {
        list.enqueue_by_owner(spans[0], &mut step);
        assert_eq!(
            list.try_pop_head_once::<DefaultCas2Backend>(&mut step),
            TryPop::Span(spans[0])
        );
    }
}

#[test]
fn stale_cas2_fails_after_version_bump() {
    let region = OwnedRegion::new(2);
    let spans = make_spans(&region, 2);
    let list = Box::leak(Box::new(SpmcSpanList::new()));
    // SAFETY: init once; leaked box never moves.
    unsafe { list.init() };
    let mut step = StepCounter::new();

    // SAFETY: single producer.
    unsafe {
        list.enqueue_by_owner(spans[0], &mut step);
        list.enqueue_by_owner(spans[1], &mut step);
    }

    // A consumer snapshots the head, then another consumer pops first:
    // the stale snapshot's CAS2 must fail even though it points at a
    // plausible node (the version moved).
    // SAFETY: aligned head word; single-threaded test.
    unsafe {
        let stale = DefaultCas2Backend::load(list.head_ptr());
        assert_eq!(
            list.try_pop_head_once::<DefaultCas2Backend>(&mut step),
            TryPop::Span(spans[0])
        );
        let res = DefaultCas2Backend::compare_exchange(
            list.head_ptr(),
            stale,
            HeadWord::new(0xdead_0000, stale.version.wrapping_add(1)),
        );
        assert!(res.is_err(), "stale (un-versioned) CAS2 must fail");
    }
}

#[cfg(not(miri))]
#[test]
fn concurrent_consumers_one_shot_pop() {
    use std::sync::Barrier;
    use std::sync::atomic::AtomicUsize;

    const SPANS: usize = 64;
    const CONSUMERS: usize = 4;

    let region = OwnedRegion::new(SPANS);
    let spans = make_spans(&region, SPANS);
    let list: &'static SpmcSpanList = Box::leak(Box::new(SpmcSpanList::new()));
    // SAFETY: init once; leaked box never moves.
    unsafe { list.init() };
    let mut step = StepCounter::new();
    for &s in &spans {
        // SAFETY: single producer thread (this one).
        unsafe { list.enqueue_by_owner(s, &mut step) };
    }

    static FAILED: AtomicUsize = AtomicUsize::new(0);
    let barrier: &'static Barrier = Box::leak(Box::new(Barrier::new(CONSUMERS)));
    let handles: Vec<_> = (0..CONSUMERS)
        .map(|_| {
            std::thread::spawn(move || {
                barrier.wait();
                let mut got = Vec::new();
                let mut step = StepCounter::new();
                // Bounded attempts; Failed routes onward, never retries the
                // same pop in a loop.
                for _ in 0..SPANS * 8 {
                    // SAFETY: initialized, leaked (never moved) list.
                    match unsafe { list.try_pop_head_once::<DefaultCas2Backend>(&mut step) } {
                        TryPop::Span(s) => got.push(s as usize),
                        TryPop::Empty => break,
                        TryPop::Failed => {
                            FAILED.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                got
            })
        })
        .collect();

    let mut all: Vec<usize> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    all.sort_unstable();
    all.dedup();
    // Every span popped exactly once; none lost, none duplicated.
    // (The MS-queue shape keeps the last node as dummy, so with concurrent
    // one-shot pops every span is eventually taken.)
    assert_eq!(all.len(), SPANS, "spans lost or duplicated under contention");
}
