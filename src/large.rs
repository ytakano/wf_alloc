//! Wait-free large-object path: whole runs of contiguous spans (guide
//! Appendix A, Policy 1: whole-larger-run, no splitting, no coalescing).
//!
//! A request that does not fit a small size class is served by a **large
//! run**: `2^r` contiguous, `SPAN_SIZE`-aligned spans carved from the SAME
//! `FixedSpanPool` region as small spans (single FAA). The run's base span
//! carries a `SpanHeader` (see `span::init_run`), so free runs circulate
//! through the same wait-free machinery as small spans: per-thread private
//! `local_runs` lists, public SPMC `public_runs` lists, and the bounded
//! helping protocol (`runlists_acquire_run`).
//!
//! Policy 1: if class `k` has no free run, a whole class-`j > k` run may be
//! used as-is. The run keeps the class it was carved at forever; freeing it
//! returns it to that class's list, so capacity never degrades.
//!
//! Deallocation is O(1): the DEALLOCATING thread becomes the run's owner
//! and keeps (or publishes) the whole run. No remote free-list is needed —
//! the previous owner retains no reference to a freed run.
//!
//! ## Memory layout of one large allocation (run class r)
//!
//! ```text
//! run_base (SPAN_SIZE-aligned, 2^r spans)
//!   ├─ SpanHeader (run header; within SPAN_HEADER_RESERVE bytes)
//!   ├─ [padding to align the payload]
//!   ├─ LargeAllocHeader { magic, run, run_class, span_count }
//!   └─ payload (layout.align()-aligned)                ← returned to caller
//! ```
//!
//! Dispatch between the small, large, and huge paths is a pure function of
//! the `Layout`, identical in alloc and dealloc. Hence `span_from_ptr`
//! (SPAN_SIZE masking) is never applied to a large payload, whose masked
//! address could be a headerless interior span of the run. Requests with
//! `size >= WfSpanAllocator::HUGE_THRESHOLD` (one huge granule; default
//! 1 GiB) never reach this path — they use the bounded huge-slot directory
//! (`huge.rs`), which avoids GiB-scale runs lingering in help records and
//! per-thread caches.

use core::alloc::Layout;
use core::sync::atomic::Ordering;

use crate::acquire::runlists_acquire_run;
use crate::align::round_up;
use crate::allocator::WfSpanAllocator;
use crate::atomic_backend::DefaultCas2Backend;
use crate::config::{
    LARGE_LOCAL_RUN_LIMIT_K, MAX_LARGE_RUN_CLASSES, MAX_LARGE_SPANS, OWNER_PUBLIC,
    SPAN_HEADER_RESERVE, SPAN_SIZE,
};
use crate::span::{SpanHeader, SpanState, init_run};
use crate::stats::{AllocatorStats, StepCounter};
use crate::thread::ThreadToken;

/// Marker validating that a freed pointer carries a large-run header
/// (debug builds only). ASCII "WFLRUN1\0" as a usize.
pub const LARGE_MAGIC: usize = 0x5746_4C52_554E_3100;

/// Hidden header placed immediately before every large payload.
#[repr(C)]
pub struct LargeAllocHeader {
    pub magic: usize,
    /// Base run header (the run's first span).
    pub run: *mut SpanHeader,
    /// Class the run was CARVED at (Policy 1: never changes).
    pub run_class: usize,
    /// `1 << run_class`; redundant, kept for debug cross-checks.
    pub span_count: usize,
}

const HDR_SIZE: usize = core::mem::size_of::<LargeAllocHeader>();

/// Spans in a run of `class`.
pub const fn run_class_spans(class: usize) -> usize {
    1 << class
}

/// Bytes in a run of `class`.
pub const fn run_class_bytes(class: usize) -> usize {
    SPAN_SIZE << class
}

/// Smallest run class whose run is guaranteed to hold the run header
/// reserve, a `LargeAllocHeader`, and a `layout`-aligned payload.
/// `None` if the request needs more than `MAX_LARGE_SPANS` spans (or
/// overflows). Alignments larger than `SPAN_SIZE` are honored via slack.
pub fn run_class_for_layout(layout: Layout) -> Option<usize> {
    let align = layout.align().max(core::mem::align_of::<LargeAllocHeader>());
    let needed = SPAN_HEADER_RESERVE
        .checked_add(HDR_SIZE)?
        .checked_add(align - 1)?
        .checked_add(layout.size())?;
    let needed_spans = needed.div_ceil(SPAN_SIZE);
    let spans = needed_spans.checked_next_power_of_two()?;
    if spans > MAX_LARGE_SPANS {
        return None;
    }
    Some(spans.trailing_zeros() as usize)
}

/// Write the `LargeAllocHeader` and return the aligned payload pointer.
///
/// # Safety
/// `run` must be the initialized base header of a run of `run_class`
/// exclusively owned by the caller, and `run_class` must be at least
/// `run_class_for_layout(layout)`.
unsafe fn place_large_payload(
    run: *mut SpanHeader,
    run_class: usize,
    layout: Layout,
) -> *mut u8 {
    let align = layout.align().max(core::mem::align_of::<LargeAllocHeader>());
    // Pointer arithmetic (not int casts) keeps the run's provenance.
    let payload_off =
        round_up(run as usize + SPAN_HEADER_RESERVE + HDR_SIZE, align) - run as usize;
    // SAFETY: payload_off + layout.size() fits the run (see debug_asserts;
    // guaranteed by run_class_for_layout).
    let payload = unsafe { (run as *mut u8).add(payload_off) };
    let header = unsafe { payload.sub(HDR_SIZE) } as *mut LargeAllocHeader;
    debug_assert!(header as usize >= run as usize + SPAN_HEADER_RESERVE);
    debug_assert!(payload_off + layout.size() <= run_class_bytes(run_class));
    // SAFETY: header lies past the reserve area, inside the exclusively
    // owned run (asserted above; guaranteed by run_class_for_layout).
    unsafe {
        core::ptr::write(
            header,
            LargeAllocHeader {
                magic: LARGE_MAGIC,
                run,
                run_class,
                span_count: run_class_spans(run_class),
            },
        );
    }
    payload
}

impl<const N: usize, const C: usize, const HG: usize> WfSpanAllocator<N, C, HG> {
    /// Large allocation (guide A.7, Policy 1). Bounded: at most
    /// `MAX_LARGE_RUN_CLASSES` class steps, each one local pop (O(1)) plus
    /// one helping acquisition (O(H + P)), plus one raw carve (one FAA, at
    /// most one rollback CAS) — O(R · N) total, no retry loops.
    ///
    /// # Safety
    /// As for [`Self::alloc_with_token`].
    pub(crate) unsafe fn alloc_large_with_token_counted(
        &self,
        layout: Layout,
        token: ThreadToken,
        step: &mut StepCounter,
    ) -> *mut u8 {
        let Some(min_class) = run_class_for_layout(layout) else {
            return core::ptr::null_mut();
        };
        let tid = token.id;
        debug_assert!(tid < N);

        // Class search order per class (Policy 1):
        //   (a) own local free runs — exact-class reuse, zero waste, O(1);
        //   (b) public run-lists via bounded helping;
        //   (c) fresh carve, at the EXACT class only: if carving 2^min_class
        //       spans fails, any larger carve must fail too, and escalated
        //       reuse (a)/(b) only wastes space temporarily — the run goes
        //       back to its own class on free.
        // Bounded loop: at most MAX_LARGE_RUN_CLASSES iterations.
        for class in min_class..MAX_LARGE_RUN_CLASSES {
            step.large_class_steps += 1;

            // SAFETY: local_runs is owner-private; we are thread `tid`.
            let run = unsafe { self.heaps[tid].local_runs[class].pop_front() };
            if !run.is_null() {
                // SAFETY: popped from our own list => exclusively ours.
                return unsafe { self.finish_large(run, class, layout) };
            }

            // SAFETY: tid is a valid registered id per token contract.
            let run = unsafe {
                runlists_acquire_run::<DefaultCas2Backend, N, C, HG>(self, tid, class, step)
            };
            if !run.is_null() {
                // SAFETY: acquire hands us exclusive ownership of `run`.
                unsafe {
                    debug_assert_eq!((*run).owner.load(Ordering::Relaxed), OWNER_PUBLIC);
                    (*run).owner.store(tid, Ordering::Release);
                    return self.finish_large(run, class, layout);
                }
            }

            if class == min_class {
                let raw = self.pool.acquire_raw_run(run_class_spans(class), step);
                if !raw.is_null() {
                    // SAFETY: the pool hands out each span range exactly once.
                    unsafe {
                        let run = init_run(raw, class, run_class_spans(class), tid);
                        AllocatorStats::bump(&self.stats.allocated_runs);
                        return self.finish_large(run, class, layout);
                    }
                }
            }
        }

        // Exhaustion: fixed backend returns null (never OS allocation).
        core::ptr::null_mut()
    }

    /// Stamp an exclusively owned free run as allocated and lay out the
    /// hidden header + payload.
    ///
    /// # Safety
    /// `run` must be an initialized run header of class `class`, exclusively
    /// owned by the calling thread and in no list.
    unsafe fn finish_large(
        &self,
        run: *mut SpanHeader,
        class: usize,
        layout: Layout,
    ) -> *mut u8 {
        // SAFETY: exclusive ownership per contract.
        unsafe {
            debug_assert_eq!((*run).size_class.load(Ordering::Relaxed), class);
            debug_assert_eq!(
                (*run).block_count.load(Ordering::Relaxed),
                run_class_spans(class)
            );
            (*run)
                .state
                .store(SpanState::RunAllocated as usize, Ordering::Relaxed);
            place_large_payload(run, class, layout)
        }
    }

    /// Large deallocation (guide A.10). O(1) bounded: header recovery, one
    /// owner store, then one local push OR one publish — no loops, no CAS.
    /// The deallocating thread becomes the run's new owner.
    ///
    /// # Safety
    /// `ptr` must have been returned by the large path of this allocator and
    /// not yet freed; `token` as in [`Self::dealloc_with_token`].
    pub(crate) unsafe fn dealloc_large_with_token_counted(
        &self,
        ptr: *mut u8,
        _layout: Layout,
        token: ThreadToken,
        step: &mut StepCounter,
    ) {
        step.local_steps += 1;
        // Pointer arithmetic (not an int cast) keeps the run's provenance.
        // SAFETY: the large path placed the header immediately before `ptr`.
        let header = unsafe { ptr.sub(HDR_SIZE) } as *mut LargeAllocHeader;
        // SAFETY: the large path wrote this header immediately before the
        // payload; the allocation is exclusively ours to retire.
        unsafe {
            debug_assert_eq!(
                (*header).magic,
                LARGE_MAGIC,
                "dealloc of a non-large or corrupted pointer on the large path"
            );
            let run = (*header).run;
            let class = (*header).run_class;
            debug_assert!(class < MAX_LARGE_RUN_CLASSES);
            debug_assert_eq!((*header).span_count, run_class_spans(class));
            debug_assert_eq!(
                (*run).state.load(Ordering::Relaxed),
                SpanState::RunAllocated as usize,
                "double free of a large run"
            );

            let list = &self.heaps[token.id].local_runs[class];
            if list.len() >= LARGE_LOCAL_RUN_LIMIT_K {
                // Bounded trim: publish the freed run to our own public
                // run-list (owner-only enqueue, one release store).
                (*run).owner.store(OWNER_PUBLIC, Ordering::Release);
                (*run)
                    .state
                    .store(SpanState::RunFreePublic as usize, Ordering::Relaxed);
                self.heaps[token.id].public_runs[class].enqueue_by_owner(run, step);
                AllocatorStats::bump(&self.stats.published_runs);
            } else {
                (*run).owner.store(token.id, Ordering::Release);
                (*run)
                    .state
                    .store(SpanState::RunFreeLocal as usize, Ordering::Relaxed);
                list.push_front(run);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MAX_BLOCK_SIZE;

    #[test]
    fn run_class_boundaries() {
        // A small-ish oversized request fits the 1-span class 0
        // (64 KiB - reserve - header is plenty for MAX_BLOCK_SIZE + 1).
        let l = Layout::from_size_align(MAX_BLOCK_SIZE + 1, 16).unwrap();
        assert_eq!(run_class_for_layout(l), Some(0));

        // The largest payload still fitting one span.
        let max0 = SPAN_SIZE - SPAN_HEADER_RESERVE - HDR_SIZE - 15;
        let l = Layout::from_size_align(max0, 16).unwrap();
        assert_eq!(run_class_for_layout(l), Some(0));
        // One byte more needs 2 spans => class 1.
        let l = Layout::from_size_align(max0 + 1, 16).unwrap();
        assert_eq!(run_class_for_layout(l), Some(1));

        // 3 spans round up to class 2 (4 spans).
        let l = Layout::from_size_align(SPAN_SIZE * 2 + 1, 16).unwrap();
        assert_eq!(run_class_for_layout(l), Some(2));

        // align > SPAN_SIZE is honored via slack.
        let l = Layout::from_size_align(64, 2 * SPAN_SIZE).unwrap();
        assert_eq!(run_class_for_layout(l), Some(2));

        // Top of the range: needs > MAX_LARGE_SPANS spans => None.
        let l = Layout::from_size_align(MAX_LARGE_SPANS * SPAN_SIZE, 16).unwrap();
        assert_eq!(run_class_for_layout(l), None);
        // Largest representable request.
        let max_req = MAX_LARGE_SPANS * SPAN_SIZE - SPAN_HEADER_RESERVE - HDR_SIZE - 15;
        let l = Layout::from_size_align(max_req, 16).unwrap();
        assert_eq!(run_class_for_layout(l), Some(MAX_LARGE_RUN_CLASSES - 1));
    }
}
