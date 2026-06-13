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
    /// Run classes examined by one large allocation (≤ MAX_LARGE_RUN_CLASSES).
    pub large_class_steps: usize,
    /// Directory slots examined by one huge allocation
    /// (≤ MAX_HUGE_RUN_CLASSES * MAX_HUGE_RUNS_PER_CLASS).
    pub huge_slot_scans: usize,
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
    /// const ACTIVE_THREADS: usize = 4;
    /// let region = OwnedRegion::new(16);
    /// let alloc = Box::leak(Box::new(WfSpanAllocator::<{ wf_alloc::MAX_SUPPORTED_CLASSES }>::new(ACTIVE_THREADS)));
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
    /// step.assert_bounds(ACTIVE_THREADS, HELP_BUDGET_H, ACTIVE_THREADS, bps, LOCAL_SPAN_LIMIT_K);
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

    /// Assert the per-operation bounds of the LARGE path (one
    /// `alloc_large`/`dealloc_large`). `n` = max threads, `h` = helping
    /// budget, `p` = query limit (= n), `r` = MAX_LARGE_RUN_CLASSES.
    ///
    /// One large allocation examines at most `r` run classes; each class
    /// runs at most one bounded helping acquisition (same per-call bounds as
    /// `assert_bounds`), and at most one raw carve (one FAA + one rollback
    /// CAS) happens per allocation. Deallocation is O(1).
    pub fn assert_large_bounds(&self, n: usize, h: usize, p: usize, r: usize) {
        assert!(
            self.large_class_steps <= r,
            "large_class_steps {} exceeds bound",
            self.large_class_steps
        );
        assert!(
            self.help_steps <= r * (h * n + p + 1),
            "help_steps {} exceeds large bound",
            self.help_steps
        );
        assert!(
            self.query_steps <= r * (p + n + 1),
            "query_steps {} exceeds large bound",
            self.query_steps
        );
        // One CAS2 per help/query step at most.
        assert!(
            self.cas2_attempts <= self.help_steps + self.query_steps + 2 * r,
            "cas2_attempts {} exceeds large bound",
            self.cas2_attempts
        );
        // CAS: one per help/query step (help record CAS), one clear per
        // acquisition, plus one carve rollback.
        assert!(
            self.cas_attempts <= self.help_steps + self.query_steps + 2 * r + 1,
            "cas_attempts {} exceeds large bound",
            self.cas_attempts
        );
        // FAA: one raw carve per allocation (+ slack for stats-free paths).
        assert!(
            self.faa_ops <= r + 2,
            "faa_ops {} exceeds large bound",
            self.faa_ops
        );
        // The large path never scans blocks.
        assert_eq!(self.blocks_scanned, 0, "large path scanned blocks");
    }

    /// Assert the per-operation bounds of the HUGE path (one
    /// `alloc_huge`/`dealloc_huge`). `r` = MAX_HUGE_RUN_CLASSES,
    /// `slots` = MAX_HUGE_RUNS_PER_CLASS.
    ///
    /// One huge operation scans at most `r * slots` directory slots. Per
    /// scanned slot: at most one claim CAS, and (for an EMPTY slot) at
    /// most one carve FAA plus one rollback CAS. It never touches the
    /// helping protocol, the span lists, or per-block free-lists.
    /// Deallocation is a scan plus one store — no CAS at all.
    pub fn assert_huge_bounds(&self, r: usize, slots: usize) {
        assert!(
            self.huge_slot_scans <= r * slots,
            "huge_slot_scans {} exceeds bound",
            self.huge_slot_scans
        );
        // One claim CAS per scanned slot + one rollback CAS per carve.
        assert!(
            self.cas_attempts <= 2 * self.huge_slot_scans,
            "cas_attempts {} exceeds huge bound",
            self.cas_attempts
        );
        // At most one carve FAA per EMPTY-claimed slot.
        assert!(
            self.faa_ops <= self.huge_slot_scans,
            "faa_ops {} exceeds huge bound",
            self.faa_ops
        );
        // The huge path never uses helping, queries, CAS2, or block scans.
        assert_eq!(self.help_steps, 0, "huge path used helping");
        assert_eq!(self.query_steps, 0, "huge path queried span lists");
        assert_eq!(self.cas2_attempts, 0, "huge path used CAS2");
        assert_eq!(self.blocks_scanned, 0, "huge path scanned blocks");
        assert_eq!(self.large_class_steps, 0, "huge path entered large path");
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
    /// Raw runs carved from the fixed pool (large path).
    pub allocated_runs: AtomicUsize,
    /// Freed runs published to a public SPMC run-list.
    pub published_runs: AtomicUsize,
    /// Runs acquired from public SPMC run-lists (incl. via help records).
    pub acquired_public_runs: AtomicUsize,
    /// Runs stashed in a run HelpRecord (one-request-two-runs case).
    pub run_help_record_runs: AtomicUsize,
    /// Runs reclaimed from a run HelpRecord on a later acquisition.
    pub run_help_record_reclaimed: AtomicUsize,
    /// Huge runs lazily carved from the fixed pool into directory slots.
    pub allocated_huge_runs: AtomicUsize,
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
            allocated_runs: AtomicUsize::new(0),
            published_runs: AtomicUsize::new(0),
            acquired_public_runs: AtomicUsize::new(0),
            run_help_record_runs: AtomicUsize::new(0),
            run_help_record_reclaimed: AtomicUsize::new(0),
            allocated_huge_runs: AtomicUsize::new(0),
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
/// `A_extra(a) = (a + (ceil(a / p) + a - 1) * (a - 1)) * c * s`.
///
/// # Examples
///
/// ```
/// use wf_alloc::theoretical_extra_bound;
/// use wf_alloc::SPAN_SIZE;
///
/// // Bound for 4 active threads, 8 size classes, with P = A = 4.
/// let bound = theoretical_extra_bound(4, 8, SPAN_SIZE, 4);
/// assert!(bound > 0);
/// ```
pub const fn theoretical_extra_bound(n: usize, c: usize, s: usize, p: usize) -> usize {
    let ceil_n_p = n.div_ceil(p);
    (n + (ceil_n_p + n - 1) * (n - 1)) * c * s
}
