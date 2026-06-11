//! Helping-protocol tests (Milestone 6 acceptance): assisted completion,
//! reclaim, the one-request-two-spans case, H/P bounds, and the natural
//! publish path (K overflow).

use std::alloc::Layout;
use std::sync::atomic::Ordering;

use wf_alloc::DefaultCas2Backend;
use wf_alloc::WfSpanAllocator;
use wf_alloc::acquire::spanlists_acquire_span;
use wf_alloc::config::{HELP_BUDGET_H, LOCAL_SPAN_LIMIT_K, OWNER_PUBLIC};
use wf_alloc::help_record::{EncodedReq, help_finishing_req, reclaim_request};
use wf_alloc::region::OwnedRegion;
use wf_alloc::size_class::blocks_per_span;
use wf_alloc::span::{SpanHeader, init_span};
use wf_alloc::stats::StepCounter;
use wf_alloc::class_to_size;

const N: usize = 2;
const C: usize = 4;

fn setup(spans: usize) -> (&'static WfSpanAllocator<N, C>, OwnedRegion) {
    let region = OwnedRegion::new(spans);
    let alloc = Box::leak(Box::new(WfSpanAllocator::<N, C>::new()));
    // SAFETY: init once before sharing; leaked box never moves.
    unsafe { alloc.init(region.ptr(), region.len()) };
    (alloc, region)
}

/// Make one PUBLIC span without putting it in any list (models a span a
/// helper has already popped and holds exclusively).
fn make_public_span(alloc: &WfSpanAllocator<N, C>) -> *mut SpanHeader {
    let mut step = StepCounter::new();
    let raw = alloc.pool.acquire_raw_span(&mut step);
    assert!(!raw.is_null());
    // SAFETY: fresh raw span exclusively owned by this test.
    unsafe { init_span(raw, 0, class_to_size(0), OWNER_PUBLIC) }
}

/// Make one PUBLIC span and enqueue it on heap 0's class-0 public list.
fn publish_one(alloc: &WfSpanAllocator<N, C>) -> *mut SpanHeader {
    let span = make_public_span(alloc);
    let mut step = StepCounter::new();
    // SAFETY: enqueue as the list's unique producer (single-threaded setup).
    unsafe { alloc.heaps[0].public_spans[0].enqueue_by_owner(span, &mut step) };
    span
}

#[test]
fn helper_completes_pending_request() {
    let (alloc, _region) = setup(4);
    let span = publish_one(alloc);
    let mut step = StepCounter::new();

    // "Thread A" (record 1) publishes a pending request.
    let req = &alloc.help.records[1][0];
    req.phase_pending_or_span
        .store(EncodedReq::pending(7).0, Ordering::Release);

    // "Thread B" helps: pops from the public list, completes the request.
    let mut held: *mut SpanHeader = std::ptr::null_mut();
    let mut empty = false;
    // SAFETY: initialized list; held is exclusively ours.
    unsafe {
        help_finishing_req::<DefaultCas2Backend>(
            &alloc.heaps[0].public_spans[0],
            req,
            &mut held,
            &mut empty,
            &mut step,
        )
    };
    assert!(held.is_null(), "span must transfer into the record");
    assert!(!empty);
    let enc = EncodedReq(req.phase_pending_or_span.load(Ordering::Acquire));
    assert!(!enc.is_pending() && !enc.is_empty());
    assert_eq!(enc.span(), span);

    // Helping the same (now completed) record again is safe and a no-op.
    // SAFETY: as above.
    unsafe {
        help_finishing_req::<DefaultCas2Backend>(
            &alloc.heaps[0].public_spans[0],
            req,
            &mut held,
            &mut empty,
            &mut step,
        )
    };
    assert!(held.is_null() && !empty);

    // "Thread A" reclaims the completed span.
    assert_eq!(reclaim_request(req, &mut step), span);
    assert!(EncodedReq(req.phase_pending_or_span.load(Ordering::Acquire)).is_empty());
    // Reclaiming again returns null (no double ownership).
    assert!(reclaim_request(req, &mut step).is_null());
}

#[test]
fn empty_list_sets_list_is_null() {
    let (alloc, _region) = setup(1);
    let req = &alloc.help.records[0][0];
    req.phase_pending_or_span
        .store(EncodedReq::pending(1).0, Ordering::Release);
    let mut held: *mut SpanHeader = std::ptr::null_mut();
    let mut empty = false;
    let mut step = StepCounter::new();
    // SAFETY: initialized (empty) list.
    unsafe {
        help_finishing_req::<DefaultCas2Backend>(
            &alloc.heaps[1].public_spans[0],
            req,
            &mut held,
            &mut empty,
            &mut step,
        )
    };
    assert!(empty, "empty list must be reported");
    assert!(
        EncodedReq(req.phase_pending_or_span.load(Ordering::Acquire)).is_pending(),
        "request must stay pending when no span is available"
    );
}

#[test]
fn acquire_via_own_pop_and_via_helper() {
    let (alloc, _region) = setup(4);
    let span = publish_one(alloc);

    // Own pop path: acquire finds the span on heap 0's public list.
    let mut step = StepCounter::new();
    // SAFETY: tid 1 used only by this thread; allocator initialized.
    let got = unsafe { spanlists_acquire_span::<DefaultCas2Backend, N, C>(alloc, 1, 0, &mut step) };
    assert_eq!(got, span);
    step.assert_bounds(N, HELP_BUDGET_H, N, blocks_per_span(class_to_size(0)), LOCAL_SPAN_LIMIT_K);

    // Helper-completed path: pre-complete the record (as a helper would),
    // then acquire must reclaim it without touching any list.
    let span2 = make_public_span(alloc); // a helper's exclusively held span
    let req = &alloc.help.records[1][0];
    let mut held = span2;
    let mut empty = false;
    req.phase_pending_or_span
        .store(EncodedReq::pending(3).0, Ordering::Release);
    // SAFETY: held span is exclusively ours (popped equivalent).
    unsafe {
        // Complete using the held span without popping (list untouched).
        help_finishing_req::<DefaultCas2Backend>(
            &alloc.heaps[1].public_spans[0], // empty list: held short-circuits the pop
            req,
            &mut held,
            &mut empty,
            &mut step,
        )
    };
    assert!(held.is_null());
    let mut step2 = StepCounter::new();
    // SAFETY: as above.
    let got2 =
        unsafe { spanlists_acquire_span::<DefaultCas2Backend, N, C>(alloc, 1, 0, &mut step2) };
    assert_eq!(got2, span2, "completed record must be reclaimed first");
    assert_eq!(
        alloc.stats.help_record_reclaimed.load(Ordering::Relaxed),
        1
    );
}

#[test]
fn one_request_two_spans_is_recoverable() {
    let (alloc, _region) = setup(8);
    let span_a = make_public_span(alloc); // already "popped" by a helper
    let span_b = publish_one(alloc); // still on the public list

    // Simulate: thread 1's request was completed by a helper with span_a,
    // but thread 1 also popped span_b itself before noticing. The acquire
    // call models this by the record being pre-completed; the direct pop
    // happens inside acquire (list non-empty), so acquire returns span_a
    // (reclaim-first) and span_b stays in the list — OR, in the in-flight
    // variant below, the extra span is stashed in the record.
    let req = &alloc.help.records[1][0];
    req.phase_pending_or_span
        .store(EncodedReq::done_with_span(span_a).0, Ordering::Release);

    let mut step = StepCounter::new();
    // SAFETY: tid 1 single-threaded here.
    let got = unsafe { spanlists_acquire_span::<DefaultCas2Backend, N, C>(alloc, 1, 0, &mut step) };
    assert_eq!(got, span_a, "reclaim-before-publish must win");

    // Next acquire still finds span_b via the normal pop path: no span lost.
    let mut step = StepCounter::new();
    // SAFETY: as above.
    let got = unsafe { spanlists_acquire_span::<DefaultCas2Backend, N, C>(alloc, 1, 0, &mut step) };
    assert_eq!(got, span_b);

    // In-flight variant: a helper completes the request while the requester
    // already holds a popped span. help_finishing_req keeps the held span
    // (CAS fails), and acquire stashes it back into the record on return —
    // verified at the unit level here.
    let span_c = make_public_span(alloc); // held by the helper
    let span_d = make_public_span(alloc); // held by the requester
    req.phase_pending_or_span
        .store(EncodedReq::pending(9).0, Ordering::Release);
    // Helper completes with span_c.
    let mut held_helper = span_c;
    let mut empty = false;
    // SAFETY: held span exclusively ours.
    unsafe {
        help_finishing_req::<DefaultCas2Backend>(
            &alloc.heaps[1].public_spans[0],
            req,
            &mut held_helper,
            &mut empty,
            &mut step,
        )
    };
    assert!(held_helper.is_null());
    // Requester (holding span_d) tries to finish: CAS fails, held kept.
    let mut held_req = span_d;
    // SAFETY: as above.
    unsafe {
        help_finishing_req::<DefaultCas2Backend>(
            &alloc.heaps[1].public_spans[0],
            req,
            &mut held_req,
            &mut empty,
            &mut step,
        )
    };
    assert_eq!(held_req, span_d, "requester must keep its extra span");
    // Record still owns span_c; both spans remain recoverable.
    assert_eq!(reclaim_request(req, &mut step), span_c);
}

#[test]
fn surplus_full_span_published_after_k_overflow() {
    const SPANS: usize = LOCAL_SPAN_LIMIT_K + 8;
    let (alloc, _region) = setup(SPANS);
    let t0 = alloc.register_thread().unwrap();
    let t1 = alloc.register_thread().unwrap();
    // Use the largest class in C to keep block counts small.
    let class = C - 1;
    let bs = class_to_size(class);
    let layout = Layout::from_size_align(bs, 8).unwrap();
    let m = blocks_per_span(bs);
    let spans_needed = LOCAL_SPAN_LIMIT_K + 1;

    // Drain K+1 spans...
    let ptrs: Vec<_> = (0..spans_needed * m)
        // SAFETY: registered token, single thread.
        .map(|_| unsafe { alloc.alloc_with_token(layout, t0) })
        .collect();
    assert!(ptrs.iter().all(|p| !p.is_null()));
    // ...then free everything: spans refill to full while len > K, so the
    // owner must publish surplus full spans to its public SPMC list.
    for &p in &ptrs {
        // SAFETY: freed once by owner.
        unsafe { alloc.dealloc_with_token(p, layout, t0) };
    }
    let published = alloc.stats.published_spans.load(Ordering::Relaxed);
    assert!(published >= 1, "K overflow must publish surplus spans");

    // Another thread acquires the published span through the real path.
    // SAFETY: registered token.
    let p = unsafe { alloc.alloc_with_token(layout, t1) };
    assert!(!p.is_null());
    assert_eq!(
        alloc.stats.acquired_public_spans.load(Ordering::Relaxed),
        1,
        "t1 must obtain the published span, not a raw one"
    );
    // SAFETY: freed once.
    unsafe { alloc.dealloc_with_token(p, layout, t1) };
    // SAFETY: quiescent.
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}
