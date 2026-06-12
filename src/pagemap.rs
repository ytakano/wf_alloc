//! Fixed pre-provisioned span pool.
//!
//! Because spans are `SPAN_SIZE`-aligned, `span_from_ptr` is a mask — no
//! pagemap lookup structure is needed; this module instead provides the
//! upper-layer span source. The pool is a caller-provided memory region
//! handed out one raw span at a time with a single FAA (wait-free, O(1)).
//! No OS allocation ever happens on the wait-free path; exhaustion returns
//! null. Raw spans are never returned to the pool in this prototype — freed
//! spans keep circulating through the SPMC span-lists.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::align::round_up;
use crate::config::{SPAN_ALIGN, SPAN_SIZE};
use crate::stats::StepCounter;

pub struct FixedSpanPool {
    base: AtomicUsize,
    count: AtomicUsize,
    next: AtomicUsize,
}

impl FixedSpanPool {
    pub const fn new() -> Self {
        Self {
            base: AtomicUsize::new(0),
            count: AtomicUsize::new(0),
            next: AtomicUsize::new(0),
        }
    }

    /// Install the backing region. The usable part is the largest
    /// `SPAN_ALIGN`-aligned array of spans inside `[ptr, ptr + len)`.
    ///
    /// # Safety
    /// `ptr..ptr+len` must be valid, writable, unused memory that outlives
    /// the allocator. Must be called once, before any allocation.
    pub unsafe fn set_region(&self, ptr: *mut u8, len: usize) {
        let start = round_up(ptr as usize, SPAN_ALIGN);
        let end = (ptr as usize).saturating_add(len);
        let count = end.saturating_sub(start) / SPAN_SIZE;
        self.base.store(start, Ordering::Relaxed);
        self.count.store(count, Ordering::Relaxed);
        self.next.store(0, Ordering::Release);
    }

    /// Take one raw span. Wait-free: a single FAA. Null on exhaustion.
    pub fn acquire_raw_span(&self, step: &mut StepCounter) -> *mut u8 {
        step.faa_ops += 1;
        let i = self.next.fetch_add(1, Ordering::Relaxed);
        if i >= self.count.load(Ordering::Relaxed) {
            return core::ptr::null_mut();
        }
        (self.base.load(Ordering::Relaxed) + i * SPAN_SIZE) as *mut u8
    }

    /// Take `span_count` contiguous raw spans (for a large run). Wait-free:
    /// one FAA plus AT MOST ONE rollback CAS attempt (never retried). Null
    /// on exhaustion.
    ///
    /// Exhaustion semantics: a failed multi-span FAA overshoots `next`; the
    /// single `compare_exchange` tries to hand the tail back. If that CAS
    /// loses (another thread carved meanwhile), the remaining tail — fewer
    /// than `span_count` spans — is permanently skipped. This waste is
    /// bounded by one run of the largest requested class per exhaustion
    /// race; freed spans and runs still recirculate through the span/run
    /// lists, so no already-carved memory is ever lost.
    pub fn acquire_raw_run(&self, span_count: usize, step: &mut StepCounter) -> *mut u8 {
        debug_assert!(span_count >= 1);
        // Cheap pre-check: avoid pointlessly poisoning `next` when the
        // request can never fit (or the pool is uninitialized).
        if span_count > self.count.load(Ordering::Relaxed) {
            return core::ptr::null_mut();
        }
        step.faa_ops += 1;
        let i = self.next.fetch_add(span_count, Ordering::Relaxed);
        let count = self.count.load(Ordering::Relaxed);
        if i >= count || count - i < span_count {
            // Single rollback attempt; losing it is acceptable bounded waste.
            step.cas_attempts += 1;
            let _ = self.next.compare_exchange(
                i + span_count,
                i,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
            return core::ptr::null_mut();
        }
        (self.base.load(Ordering::Relaxed) + i * SPAN_SIZE) as *mut u8
    }

    /// Total spans in the region.
    pub fn spans_total(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    /// Raw spans handed out so far (saturates at total on exhaustion).
    pub fn spans_used(&self) -> usize {
        self.next.load(Ordering::Relaxed).min(self.spans_total())
    }

    pub fn base_addr(&self) -> usize {
        self.base.load(Ordering::Relaxed)
    }
}

impl Default for FixedSpanPool {
    fn default() -> Self {
        Self::new()
    }
}
