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
