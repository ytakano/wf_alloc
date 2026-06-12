//! Bounded, helping-based acquisition from public SPMC lists (paper
//! Algorithms 3 and 4), shared by the small-span and large-run paths.
//!
//! Flow (all loops statically bounded by H and P = N):
//! 1. Reclaim a span already completed into this thread's HelpRecord.
//! 2. Publish a new pending request (fresh phase).
//! 3. Help at most H other pending requests.
//! 4. Traverse at most P public lists finishing our own request.
//! 5. On exit, clear/reclaim the pending request; save positions.
//!
//! Non-linearizable by design: one request may end up with TWO spans (one
//! returned directly, one completed into the record by a helper). The extra
//! span stays in the HelpRecord and is reclaimed by step 1 of a later call.
//! It is never dropped.
//!
//! The same protocol runs over two index spaces ("lanes"):
//! - small spans: per-size-class `public_spans` lists and `help.records`;
//! - large runs: per-run-class `public_runs` lists and `help.run_records`
//!   (a run's base span carries a `SpanHeader`, so the machinery is shared).

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::allocator::WfSpanAllocator;
use crate::atomic_backend::Cas2Backend;
use crate::help_record::{EncodedReq, HelpRecord, help_finishing_req, reclaim_request};
use crate::span::SpanHeader;
use crate::spmc_span_list::SpmcSpanList;
use crate::stats::{AllocatorStats, StepCounter};

/// Stat sinks so the span lane and the run lane bump distinct counters.
struct AcquireStats<'a> {
    /// Bumped when a previously completed HelpRecord span is reclaimed.
    reclaimed: &'a AtomicUsize,
    /// Bumped when a span/run is acquired from a public list.
    acquired: &'a AtomicUsize,
    /// Bumped when a surplus span/run is stashed into our HelpRecord.
    stashed: &'a AtomicUsize,
}

/// Acquire a span of `size_class` for thread `tid`, or null within the
/// configured query budget. Bounded: O(H + P) iterations, one CAS2 each.
///
/// Returned spans (and spans left in the HelpRecord) have owner
/// `OWNER_PUBLIC`; the caller claims ownership.
///
/// # Safety
/// `tid` must be a valid registered thread id (`< N`) used only by the
/// calling thread; the allocator must be initialized.
pub unsafe fn spanlists_acquire_span<
    B: Cas2Backend,
    const N: usize,
    const C: usize,
    const HG: usize,
>(
    alloc: &WfSpanAllocator<N, C, HG>,
    tid: usize,
    size_class: usize,
    step: &mut StepCounter,
) -> *mut SpanHeader {
    let heap = &alloc.heaps[tid];
    // SAFETY: forwarded contract; the accessors index initialized lists and
    // records of registered heaps (`i < N` by construction in the core).
    unsafe {
        acquire_from_lists::<B, N>(
            &alloc.help.records[tid][size_class],
            &heap.cur_query[size_class],
            &heap.helping_pos[size_class],
            |i| &alloc.help.records[i][size_class],
            |i| &alloc.heaps[i].public_spans[size_class],
            &AcquireStats {
                reclaimed: &alloc.stats.help_record_reclaimed,
                acquired: &alloc.stats.acquired_public_spans,
                stashed: &alloc.stats.help_record_spans,
            },
            step,
        )
    }
}

/// Acquire a large run of `run_class` for thread `tid`, or null within the
/// configured query budget. Same bounded protocol as
/// [`spanlists_acquire_span`], over the run lanes.
///
/// Returned runs (and runs left in the run HelpRecord) have owner
/// `OWNER_PUBLIC`; the caller claims ownership.
///
/// # Safety
/// As for [`spanlists_acquire_span`]; `run_class < MAX_LARGE_RUN_CLASSES`.
pub unsafe fn runlists_acquire_run<
    B: Cas2Backend,
    const N: usize,
    const C: usize,
    const HG: usize,
>(
    alloc: &WfSpanAllocator<N, C, HG>,
    tid: usize,
    run_class: usize,
    step: &mut StepCounter,
) -> *mut SpanHeader {
    let heap = &alloc.heaps[tid];
    // SAFETY: forwarded contract (see spanlists_acquire_span).
    unsafe {
        acquire_from_lists::<B, N>(
            &alloc.help.run_records[tid][run_class],
            &heap.cur_query_runs[run_class],
            &heap.helping_pos_runs[run_class],
            |i| &alloc.help.run_records[i][run_class],
            |i| &alloc.heaps[i].public_runs[run_class],
            &AcquireStats {
                reclaimed: &alloc.stats.run_help_record_reclaimed,
                acquired: &alloc.stats.acquired_public_runs,
                stashed: &alloc.stats.run_help_record_runs,
            },
            step,
        )
    }
}

/// The bounded helping protocol shared by both lanes. `record_at(i)` /
/// `list_at(i)` give thread `i`'s help record / public list for the class
/// being acquired; `my_req`, `cur_query`, `helping_pos` belong to the
/// calling thread for that class.
///
/// # Safety
/// `my_req == record_at(tid)` for the calling thread; all lists returned by
/// `list_at` must be initialized; the calling thread must be the only user
/// of `my_req`'s owner side.
unsafe fn acquire_from_lists<'a, B: Cas2Backend, const N: usize>(
    my_req: &'a HelpRecord,
    cur_query: &'a AtomicUsize,
    helping_pos: &'a AtomicUsize,
    record_at: impl Fn(usize) -> &'a HelpRecord,
    list_at: impl Fn(usize) -> &'a SpmcSpanList,
    stats: &AcquireStats<'a>,
    step: &mut StepCounter,
) -> *mut SpanHeader {
    let mut query_pos = cur_query.load(Ordering::Relaxed) % N;
    let mut helping = helping_pos.load(Ordering::Relaxed) % N;

    let save = |query_pos: usize, helping: usize| {
        cur_query.store(query_pos, Ordering::Relaxed);
        helping_pos.store(helping, Ordering::Relaxed);
    };

    // 1. Reclaim a span a helper completed for us earlier.
    let reclaimed = reclaim_request(my_req, step);
    if !reclaimed.is_null() {
        AllocatorStats::bump(stats.reclaimed);
        save(query_pos, helping);
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
        let req = record_at(helping % N);
        let target_list = list_at(query_pos % N);

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
        helping = (helping + 1) % N;
    }

    // 4. Finish our own request, traversing at most P (= N) lists.
    while help_query < N {
        step.query_steps += 1;

        let state = EncodedReq(my_req.phase_pending_or_span.load(Ordering::Acquire));
        if !state.is_pending() {
            // A helper completed our request.
            let span = reclaim_request(my_req, step);
            AllocatorStats::bump(stats.acquired);
            save(query_pos, helping);
            return finish_with(my_req, stats, span, held_span);
        }

        let target_list = list_at(query_pos % N);
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
            AllocatorStats::bump(stats.acquired);
            save(query_pos, helping);
            return finish_with(my_req, stats, span, held_span);
        }

        // We popped a span but could not place it in our (already cleared
        // or re-completed) record: return it directly. This is the
        // non-linearizable one-request-two-spans path.
        if !held_span.is_null() {
            let span = held_span;
            AllocatorStats::bump(stats.acquired);
            save(query_pos, helping);
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
    save(query_pos, helping);
    if !span.is_null() {
        AllocatorStats::bump(stats.acquired);
        return finish_with(my_req, stats, span, held_span);
    }
    if !held_span.is_null() {
        AllocatorStats::bump(stats.acquired);
        return held_span;
    }
    core::ptr::null_mut()
}

/// Return `ret`; if we still hold a second span, stash it back into our
/// (now empty) record as a completed request so it is reclaimed by the next
/// acquisition instead of being leaked.
fn finish_with(
    my_req: &HelpRecord,
    stats: &AcquireStats<'_>,
    ret: *mut SpanHeader,
    held: *mut SpanHeader,
) -> *mut SpanHeader {
    if !held.is_null() {
        debug_assert!(held != ret);
        my_req
            .phase_pending_or_span
            .store(EncodedReq::done_with_span(held).0, Ordering::Release);
        AllocatorStats::bump(stats.stashed);
    }
    ret
}
