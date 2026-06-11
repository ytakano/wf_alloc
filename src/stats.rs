//! Step accounting (wait-freedom guardrail) and allocator-wide statistics.

use core::sync::atomic::{AtomicUsize, Ordering};

/// Per-operation step counter. Every public alloc/dealloc path updates this.
/// It is a guardrail that catches accidental unbounded retry loops; it is not
/// a formal proof of wait-freedom.
#[derive(Default, Clone, Copy, Debug)]
pub struct StepCounter {
    pub local_steps: usize,
    pub remote_steps: usize,
    pub cas_attempts: usize,
    pub cas2_attempts: usize,
    pub swap_ops: usize,
    pub faa_ops: usize,
    pub help_steps: usize,
    pub query_steps: usize,
    pub blocks_scanned: usize,
}

impl StepCounter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Assert the per-operation bounds implied by the configuration.
    /// `n` = max threads, `h` = helping budget, `p` = query limit.
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(feature = "std")] {
    /// use core::alloc::Layout;
    /// use wf_alloc::{WfSpanAllocator, StepCounter, HELP_BUDGET_H, LOCAL_SPAN_LIMIT_K};
    /// use wf_alloc::region::OwnedRegion;
    /// use wf_alloc::size_class::{blocks_per_span, class_to_size};
    ///
    /// const N: usize = 4;
    /// const C: usize = 8;
    /// let region = OwnedRegion::new(16);
    /// let alloc = Box::leak(Box::new(WfSpanAllocator::<N, C>::new()));
    /// unsafe { alloc.init(region.ptr(), region.len()) };
    /// let token = alloc.register_thread().unwrap();
    ///
    /// let layout = Layout::new::<u32>();
    /// let mut step = StepCounter::new();
    /// let ptr = unsafe { alloc.alloc_with_token_counted(layout, token, &mut step) };
    /// assert!(!ptr.is_null());
    ///
    /// // Verify this single allocation stayed within the wait-freedom bounds.
    /// let bps = blocks_per_span(class_to_size(0)); // class 0 = 16-byte blocks
    /// step.assert_bounds(N, HELP_BUDGET_H, N, bps, LOCAL_SPAN_LIMIT_K);
    ///
    /// unsafe { alloc.dealloc_with_token(ptr, layout, token) };
    /// # }
    /// ```
    pub fn assert_bounds(&self, n: usize, h: usize, p: usize, blocks_per_span: usize, k: usize) {
        // Helping loop: <= H helped requests plus <= P empty-list skips.
        assert!(
            self.help_steps <= h * n + p + 1,
            "help_steps {} exceeds bound",
            self.help_steps
        );
        // Query loop: <= P traversed lists.
        assert!(
            self.query_steps <= p + n + 1,
            "query_steps {} exceeds bound",
            self.query_steps
        );
        // One CAS2 per help/query step at most.
        assert!(
            self.cas2_attempts <= self.help_steps + self.query_steps + 2,
            "cas2_attempts {} exceeds bound",
            self.cas2_attempts
        );
        // Remote list consumption is bounded by blocks_per_span per rotated
        // span, and at most K+1 spans are rotated per allocation.
        assert!(
            self.blocks_scanned <= blocks_per_span * (k + 2),
            "blocks_scanned {} exceeds bound",
            self.blocks_scanned
        );
    }
}

/// Allocator-wide monotonic event counters (Relaxed; advisory).
pub struct AllocatorStats {
    /// Raw spans taken from the fixed pool.
    pub allocated_spans: AtomicUsize,
    /// Spans discarded (made ownerless with no visible free blocks).
    pub discarded_spans: AtomicUsize,
    /// Discarded spans claimed back by a remote deallocator.
    pub claimed_spans: AtomicUsize,
    /// Full spans published to a public SPMC span-list.
    pub published_spans: AtomicUsize,
    /// Spans acquired from public SPMC span-lists (incl. via help records).
    pub acquired_public_spans: AtomicUsize,
    /// Spans stashed in a HelpRecord (the one-request-two-spans case).
    pub help_record_spans: AtomicUsize,
    /// Spans reclaimed from a HelpRecord on a later acquisition.
    pub help_record_reclaimed: AtomicUsize,
    /// Times remote-list consumption stopped at an UNLINKED link
    /// (span temporarily blocked by a stalled producer).
    pub remote_blocked_events: AtomicUsize,
}

impl AllocatorStats {
    pub const fn new() -> Self {
        Self {
            allocated_spans: AtomicUsize::new(0),
            discarded_spans: AtomicUsize::new(0),
            claimed_spans: AtomicUsize::new(0),
            published_spans: AtomicUsize::new(0),
            acquired_public_spans: AtomicUsize::new(0),
            help_record_spans: AtomicUsize::new(0),
            help_record_reclaimed: AtomicUsize::new(0),
            remote_blocked_events: AtomicUsize::new(0),
        }
    }

    pub fn bump(counter: &AtomicUsize) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

impl Default for AllocatorStats {
    fn default() -> Self {
        Self::new()
    }
}

/// Paper's approximate additional-footprint bound:
/// `A(N) = (N + (ceil(N / P) + N - 1) * (N - 1)) * C * S`.
///
/// # Examples
///
/// ```
/// use wf_alloc::theoretical_extra_bound;
/// use wf_alloc::SPAN_SIZE;
///
/// // Bound for 4 threads, 8 size classes, with P = N = 4.
/// let bound = theoretical_extra_bound(4, 8, SPAN_SIZE, 4);
/// assert!(bound > 0);
/// ```
pub const fn theoretical_extra_bound(n: usize, c: usize, s: usize, p: usize) -> usize {
    let ceil_n_p = n.div_ceil(p);
    (n + (ceil_n_p + n - 1) * (n - 1)) * c * s
}
