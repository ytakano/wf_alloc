//! Help records and the bounded helping primitive (paper Algorithms 2–4).
//!
//! A `HelpRecord` is a single `AtomicUsize` encoding one of:
//! - empty (0),
//! - pending request `(phase << 1) | 1` (low bit set),
//! - completed request: an aligned span pointer (low bit clear).
//!
//! A completed record OWNS its span. It must never be overwritten without
//! reclaiming the span first (`reclaim_request`).

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::atomic_backend::Cas2Backend;
use crate::span::SpanHeader;
use crate::spmc_span_list::{SpmcSpanList, TryPop};
use crate::stats::StepCounter;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct EncodedReq(pub usize);

impl EncodedReq {
    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn pending(phase: usize) -> Self {
        Self((phase << 1) | 1)
    }

    pub fn done_with_span(span: *mut SpanHeader) -> Self {
        debug_assert_eq!((span as usize) & 1, 0);
        Self(span as usize)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn is_pending(self) -> bool {
        (self.0 & 1) == 1
    }

    pub const fn phase(self) -> usize {
        self.0 >> 1
    }

    pub const fn span(self) -> *mut SpanHeader {
        (self.0 & !1) as *mut SpanHeader
    }
}

pub struct HelpRecord {
    pub phase_pending_or_span: AtomicUsize,
    pub last_phase: AtomicUsize,
}

impl HelpRecord {
    pub const fn new() -> Self {
        Self {
            phase_pending_or_span: AtomicUsize::new(0),
            last_phase: AtomicUsize::new(0),
        }
    }
}

impl Default for HelpRecord {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-thread, per-size-class request table. Fixed arrays; no dynamic
/// allocation in the allocator core.
pub struct HelpTable<const N: usize, const C: usize> {
    pub records: [[HelpRecord; C]; N],
}

impl<const N: usize, const C: usize> HelpTable<N, C> {
    pub const fn new() -> Self {
        Self {
            records: [const { [const { HelpRecord::new() }; C] }; N],
        }
    }
}

impl<const N: usize, const C: usize> Default for HelpTable<N, C> {
    fn default() -> Self {
        Self::new()
    }
}

/// Atomically take a completed span out of `req` (clearing it). Returns
/// null if the record was empty or still pending.
///
/// Only the record OWNER may call this. The SWAP clears a pending phase as
/// a side effect; that is safe because only the owner publishes phases, and
/// a helper's completion CAS against the cleared phase simply fails.
pub fn reclaim_request(req: &HelpRecord, step: &mut StepCounter) -> *mut SpanHeader {
    step.swap_ops += 1;
    let old = req
        .phase_pending_or_span
        .swap(EncodedReq::empty().0, Ordering::AcqRel);
    let enc = EncodedReq(old);
    if enc.is_empty() || enc.is_pending() {
        core::ptr::null_mut()
    } else {
        enc.span()
    }
}

/// Try once to complete the pending request `req` using a span popped from
/// `list`. Strictly bounded: at most one pop attempt and one CAS.
///
/// `held_span`: a span popped earlier that has not been placed anywhere yet;
/// on CAS success it is transferred into the record. `list_is_null` is set
/// when `list` is observed empty so the caller advances to the next list.
///
/// # Safety
/// `list` must be an initialized SPMC list; `held_span` (if non-null) must
/// be an unowned (`OWNER_PUBLIC`) span held exclusively by the caller.
pub unsafe fn help_finishing_req<B: Cas2Backend>(
    list: &SpmcSpanList,
    req: &HelpRecord,
    held_span: &mut *mut SpanHeader,
    list_is_null: &mut bool,
    step: &mut StepCounter,
) {
    let start = EncodedReq(req.phase_pending_or_span.load(Ordering::Acquire));
    if !start.is_pending() {
        return;
    }
    let phase = start.phase();

    // Re-read to avoid helping a stale phase.
    let now = EncodedReq(req.phase_pending_or_span.load(Ordering::Acquire));
    if !now.is_pending() || now.phase() != phase {
        return;
    }

    if (*held_span).is_null() {
        // SAFETY: forwarded contract; one-shot pop.
        match unsafe { list.try_pop_head_once::<B>(step) } {
            TryPop::Span(span) => *held_span = span,
            TryPop::Empty => {
                *list_is_null = true;
                return;
            }
            TryPop::Failed => return, // someone else made progress
        }
    }

    let expected = EncodedReq::pending(phase).0;
    let desired = EncodedReq::done_with_span(*held_span).0;
    step.cas_attempts += 1;
    if req
        .phase_pending_or_span
        .compare_exchange(expected, desired, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        // Span ownership transferred into the request record.
        *held_span = core::ptr::null_mut();
    }
    // On failure the phase changed or someone else completed the request;
    // we keep holding `held_span` for the caller. No retry.
}
