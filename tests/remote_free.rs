//! MPSC remote free-list tests (Milestone 4 acceptance), including the
//! producer-stopped-after-SWAP (UNLINKED) scenario. Miri-compatible.

use std::alloc::Layout;
use std::sync::atomic::Ordering;

use wf_alloc::WfSpanAllocator;
use wf_alloc::block::{UNLINKED, block_from_payload};
use wf_alloc::class_to_size;
use wf_alloc::region::OwnedRegion;
use wf_alloc::remote_mpsc::RemoteMpscFreeList;
use wf_alloc::size_class::blocks_per_span;
use wf_alloc::span::span_from_ptr;
use wf_alloc::stats::StepCounter;

const N: usize = 4;
const C: usize = 4;

fn setup(spans: usize) -> (&'static WfSpanAllocator<C>, OwnedRegion) {
    let region = OwnedRegion::new(spans);
    let alloc = Box::leak(Box::new(WfSpanAllocator::<C>::new(N)));
    // SAFETY: init once before sharing; leaked box never moves.
    unsafe { alloc.init(region.ptr(), region.len()) };
    (alloc, region)
}

#[test]
fn remote_free_then_owner_reuses() {
    let (alloc, _region) = setup(2);
    let t0 = alloc.register_thread().unwrap();
    let t1 = alloc.register_thread().unwrap();
    let layout = Layout::from_size_align(class_to_size(0), 8).unwrap();
    let m = blocks_per_span(class_to_size(0));

    // Owner t0 drains one span.
    let ptrs: Vec<_> = (0..m)
        // SAFETY: valid token.
        .map(|_| unsafe { alloc.alloc_with_token(layout, t0) })
        .collect();
    assert!(ptrs.iter().all(|p| !p.is_null()));

    // t1 frees half of them remotely (push + FAA, no loop).
    for &p in ptrs.iter().take(m / 2) {
        let mut step = StepCounter::new();
        // SAFETY: freed once, by a non-owner thread.
        unsafe { alloc.dealloc_with_token_counted(p, layout, t1, &mut step) };
        assert!(step.swap_ops <= 1 && step.faa_ops <= 1 && step.cas_attempts <= 1);
        assert_eq!(step.local_steps, 0, "remote free must not touch local path");
    }

    // Owner reclaims them on its next allocations.
    for _ in 0..m / 2 {
        // SAFETY: valid token.
        let p = unsafe { alloc.alloc_with_token(layout, t0) };
        assert!(!p.is_null(), "remotely freed blocks must be reusable");
    }
    for &p in ptrs.iter().skip(m / 2) {
        // SAFETY: freed once.
        unsafe { alloc.dealloc_with_token(p, layout, t0) };
    }
    for _ in 0..m / 2 {
        // SAFETY: valid token.
        let p = unsafe { alloc.alloc_with_token(layout, t0) };
        // SAFETY: freed once.
        unsafe { alloc.dealloc_with_token(p, layout, t0) };
        assert!(!p.is_null());
    }
    // SAFETY: quiescent.
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}

#[test]
fn producer_stalled_after_swap_blocks_then_recovers() {
    let (alloc, _region) = setup(2);
    let t0 = alloc.register_thread().unwrap();
    let layout = Layout::from_size_align(class_to_size(0), 8).unwrap();
    let m = blocks_per_span(class_to_size(0));

    // Drain the first span completely.
    let ptrs: Vec<_> = (0..m)
        // SAFETY: valid token.
        .map(|_| unsafe { alloc.alloc_with_token(layout, t0) })
        .collect();
    let span = span_from_ptr(ptrs[0]);

    // Simulate a remote producer halted between SWAP and link for b0,
    // then a completed push of b1 on top of it.
    let b0 = block_from_payload(ptrs[0]);
    let b1 = block_from_payload(ptrs[1]);
    // SAFETY: blocks belong to `span`; we model dealloc_remote manually.
    let (old0, g) = unsafe {
        let remote = &(*span).remote;
        let old0 = remote.free.push_publish(b0); // SWAP done, link missing
        remote.free_count.fetch_add(1, Ordering::AcqRel);
        remote.free.push(b1); // complete push on top
        let g = remote.free_count.fetch_add(1, Ordering::AcqRel) + 1;
        (old0, g)
    };
    assert_eq!(g, 2);

    // Owner allocation: must consume b1, stop at b0's UNLINKED link
    // without spinning, and must NOT lose b0.
    let mut step = StepCounter::new();
    // SAFETY: valid token.
    let p = unsafe { alloc.alloc_with_token_counted(layout, t0, &mut step) };
    assert_eq!(p, ptrs[1], "the linked block (b1) must be reusable");
    assert!(step.blocks_scanned <= m, "consumption must stay bounded");

    // The blocked suffix is stashed, not dropped.
    // SAFETY: owner-side read; quiescent.
    let pending = unsafe { (*span).local.pending_remote.load(Ordering::Relaxed) };
    assert_eq!(
        pending, b0,
        "blocked chain must be stashed in pending_remote"
    );
    // SAFETY: b0.next is UNLINKED right now.
    unsafe {
        assert_eq!((*b0).next.load(Ordering::Relaxed), UNLINKED);
    }

    // Producer resumes: completes the link. Owner then recovers b0.
    // SAFETY: completing the half-done push exactly once.
    unsafe { RemoteMpscFreeList::push_link(b0, old0) };
    // SAFETY: valid token.
    let p = unsafe { alloc.alloc_with_token(layout, t0) };
    assert_eq!(p, ptrs[0], "block behind UNLINKED must be recovered");

    assert!(
        alloc.stats.remote_blocked_events.load(Ordering::Relaxed) >= 1,
        "blocked event must be counted"
    );
}

#[test]
fn discarded_span_claimed_by_remote_freer() {
    let (alloc, _region) = setup(1);
    let t0 = alloc.register_thread().unwrap();
    let t1 = alloc.register_thread().unwrap();
    let layout = Layout::from_size_align(class_to_size(0), 8).unwrap();
    let m = blocks_per_span(class_to_size(0));

    // t0 drains the only span; next alloc discards it (g == 0) and returns
    // null (pool exhausted).
    let ptrs: Vec<_> = (0..m)
        // SAFETY: valid token.
        .map(|_| unsafe { alloc.alloc_with_token(layout, t0) })
        .collect();
    // SAFETY: valid token.
    assert!(unsafe { alloc.alloc_with_token(layout, t0) }.is_null());
    assert_eq!(alloc.stats.discarded_spans.load(Ordering::Relaxed), 1);

    // t1 remote-frees one block: FAA 0 -> 1 must claim the discarded span.
    let mut step = StepCounter::new();
    // SAFETY: freed once by non-owner.
    unsafe { alloc.dealloc_with_token_counted(ptrs[0], layout, t1, &mut step) };
    assert_eq!(alloc.stats.claimed_spans.load(Ordering::Relaxed), 1);

    // t1 (the claimer/new owner) can now allocate from it.
    // SAFETY: valid token.
    let p = unsafe { alloc.alloc_with_token(layout, t1) };
    assert_eq!(p, ptrs[0], "claimed span must serve the freed block");

    // Cleanup so the verifier sees consistent counts.
    // SAFETY: each pointer freed once (t1 frees remotely into the span).
    unsafe {
        alloc.dealloc_with_token(p, layout, t1);
        for &q in ptrs.iter().skip(1) {
            alloc.dealloc_with_token(q, layout, t0);
        }
    }
}
