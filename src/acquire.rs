//! `spanlists_acquire_span`: bounded, helping-based span acquisition from
//! public SPMC span-lists (paper Algorithms 3 and 4).
//!
//! Flow (all loops statically bounded by H and P = N):
//! 1. Reclaim a span already completed into this thread's HelpRecord.
//! 2. Publish a new pending request (fresh phase).
//! 3. Help at most H other pending requests.
//! 4. Traverse at most P public span-lists finishing our own request.
//! 5. On exit, clear/reclaim the pending request; save positions.
//!
//! Non-linearizable by design: one request may end up with TWO spans (one
//! returned directly, one completed into the record by a helper). The extra
//! span stays in the HelpRecord and is reclaimed by step 1 of a later call.
//! It is never dropped.

use core::sync::atomic::Ordering;

use crate::allocator::WfSpanAllocator;
use crate::atomic_backend::Cas2Backend;
use crate::help_record::{EncodedReq, HelpRecord, help_finishing_req, reclaim_request};
use crate::span::SpanHeader;
use crate::stats::{AllocatorStats, StepCounter};

/// Acquire a span of `size_class` for thread `tid`, or null within the
/// configured query budget. Bounded: O(H + P) iterations, one CAS2 each.
///
/// Returned spans (and spans left in the HelpRecord) have owner
/// `OWNER_PUBLIC`; the caller claims ownership.
///
/// # Safety
/// `tid` must be a valid registered thread id (`< N`) used only by the
/// calling thread; the allocator must be initialized.
pub unsafe fn spanlists_acquire_span<B: Cas2Backend, const N: usize, const C: usize>(
    alloc: &WfSpanAllocator<N, C>,
    tid: usize,
    size_class: usize,
    step: &mut StepCounter,
) -> *mut SpanHeader {
    let heap = &alloc.heaps[tid];
    let mut query_pos = heap.cur_query[size_class].load(Ordering::Relaxed) % N;
    let mut helping_pos = heap.helping_pos[size_class].load(Ordering::Relaxed) % N;
    let my_req = &alloc.help.records[tid][size_class];

    let save = |query_pos: usize, helping_pos: usize| {
        heap.cur_query[size_class].store(query_pos, Ordering::Relaxed);
        heap.helping_pos[size_class].store(helping_pos, Ordering::Relaxed);
    };

    // 1. Reclaim a span a helper completed for us earlier.
    let reclaimed = reclaim_request(my_req, step);
    if !reclaimed.is_null() {
        AllocatorStats::bump(&alloc.stats.help_record_reclaimed);
        save(query_pos, helping_pos);
        return reclaimed;
    }

    // 2. Publish a new pending request.
    let phase = my_req
        .last_phase
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    my_req
        .phase_pending_or_span
        .store(EncodedReq::pending(phase).0, Ordering::Release);

    let mut held_span: *mut SpanHeader = core::ptr::null_mut();
    let mut list_is_null = false;

    // 3. Help at most H pending requests, sharing the P query budget.
    let mut help_count = 0usize;
    let mut help_query = 0usize;
    while help_count < crate::config::HELP_BUDGET_H && help_query < N {
        step.help_steps += 1;
        let req = &alloc.help.records[helping_pos % N][size_class];
        let target_list = &alloc.heaps[query_pos % N].public_spans[size_class];

        let req_state = EncodedReq(req.phase_pending_or_span.load(Ordering::Acquire));
        if req_state.is_pending() {
            // SAFETY: initialized list; held_span is exclusively ours.
            unsafe {
                help_finishing_req::<B>(target_list, req, &mut held_span, &mut list_is_null, step)
            };
            if list_is_null {
                help_query += 1;
                query_pos = (query_pos + 1) % N;
                list_is_null = false;
                continue;
            }
        }
        help_count += 1;
        helping_pos = (helping_pos + 1) % N;
    }

    // 4. Finish our own request, traversing at most P (= N) lists.
    while help_query < N {
        step.query_steps += 1;

        let state = EncodedReq(my_req.phase_pending_or_span.load(Ordering::Acquire));
        if !state.is_pending() {
            // A helper completed our request.
            let span = reclaim_request(my_req, step);
            AllocatorStats::bump(&alloc.stats.acquired_public_spans);
            save(query_pos, helping_pos);
            return finish_with(my_req, alloc, span, held_span);
        }

        let target_list = &alloc.heaps[query_pos % N].public_spans[size_class];
        // SAFETY: initialized list; held_span is exclusively ours.
        unsafe {
            help_finishing_req::<B>(target_list, my_req, &mut held_span, &mut list_is_null, step)
        };

        if list_is_null {
            help_query += 1;
            query_pos = (query_pos + 1) % N;
            list_is_null = false;
            continue;
        }

        let span = reclaim_request(my_req, step);
        if !span.is_null() {
            AllocatorStats::bump(&alloc.stats.acquired_public_spans);
            save(query_pos, helping_pos);
            return finish_with(my_req, alloc, span, held_span);
        }

        // We popped a span but could not place it in our (already cleared
        // or re-completed) record: return it directly. This is the
        // non-linearizable one-request-two-spans path.
        if !held_span.is_null() {
            let span = held_span;
            AllocatorStats::bump(&alloc.stats.acquired_public_spans);
            save(query_pos, helping_pos);
            return span;
        }

        // No progress on this list (pop Failed); spend query budget.
        help_query += 1;
        query_pos = (query_pos + 1) % N;
    }

    // 5. Budget exhausted: clear our pending request if still pending.
    step.cas_attempts += 1;
    let _ = my_req.phase_pending_or_span.compare_exchange(
        EncodedReq::pending(phase).0,
        EncodedReq::empty().0,
        Ordering::AcqRel,
        Ordering::Acquire,
    );
    // If a helper completed it concurrently, reclaim that span now.
    let span = reclaim_request(my_req, step);
    save(query_pos, helping_pos);
    if !span.is_null() {
        AllocatorStats::bump(&alloc.stats.acquired_public_spans);
        return finish_with(my_req, alloc, span, held_span);
    }
    if !held_span.is_null() {
        AllocatorStats::bump(&alloc.stats.acquired_public_spans);
        return held_span;
    }
    core::ptr::null_mut()
}

/// Return `ret`; if we still hold a second span, stash it back into our
/// (now empty) record as a completed request so it is reclaimed by the next
/// acquisition instead of being leaked.
fn finish_with<const N: usize, const C: usize>(
    my_req: &HelpRecord,
    alloc: &WfSpanAllocator<N, C>,
    ret: *mut SpanHeader,
    held: *mut SpanHeader,
) -> *mut SpanHeader {
    if !held.is_null() {
        debug_assert!(held != ret);
        my_req
            .phase_pending_or_span
            .store(EncodedReq::done_with_span(held).0, Ordering::Release);
        AllocatorStats::bump(&alloc.stats.help_record_spans);
    }
    ret
}
