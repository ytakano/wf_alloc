//! `WfSpanAllocator`: token-based wait-free allocation and deallocation.
//!
//! Const parameters: `N` = maximum participating threads, `C` = number of
//! size classes used (must be <= MAX_SUPPORTED_CLASSES).
//!
//! Allocator-core rules (see docs/progress.md): no unbounded loops — every
//! loop is bounded by N, C, P (= N), H, K, or blocks_per_span; no
//! Vec/Box/String/format!/println!; no Mutex/RwLock; no OS allocation on
//! the wait-free path.

use core::alloc::Layout;
use core::sync::atomic::Ordering;

use crate::acquire::spanlists_acquire_span;
use crate::atomic_backend::DefaultCas2Backend;
use crate::block::{Block, block_from_payload};
use crate::config::{LOCAL_SPAN_LIMIT_K, OWNER_NONE, OWNER_PUBLIC};
use crate::heap::ThreadHeap;
use crate::help_record::HelpTable;
use crate::pagemap::FixedSpanPool;
use crate::size_class::{class_to_size, size_to_class};
use crate::span::{
    SpanHeader, SpanState, alloc_from_local_span, dealloc_to_local_span, init_span, span_from_ptr,
};
use crate::remote_mpsc::append_remote_to_local_bounded;
use crate::stats::{AllocatorStats, StepCounter, theoretical_extra_bound};
use crate::thread::{ThreadRegistry, ThreadToken};

/// Token-based wait-free span allocator.
///
/// `N` is the maximum number of participating threads; `C` is the number of
/// supported power-of-two size classes (1 ≤ `C` ≤ [`MAX_SUPPORTED_CLASSES`]).
///
/// Call [`new`](Self::new) to construct, [`init`](Self::init) to wire up
/// internal state and install backing memory, then
/// [`register_thread`](Self::register_thread) once per thread before
/// calling [`alloc_with_token`](Self::alloc_with_token) or
/// [`dealloc_with_token`](Self::dealloc_with_token).
///
/// See the [crate-level documentation](crate) for a complete quick-start example.
pub struct WfSpanAllocator<const N: usize, const C: usize> {
    pub heaps: [ThreadHeap<C>; N],
    pub help: HelpTable<N, C>,
    pub pool: FixedSpanPool,
    pub registry: ThreadRegistry,
    pub stats: AllocatorStats,
}

impl<const N: usize, const C: usize> WfSpanAllocator<N, C> {
    /// Create an allocator with uninitialized (unlinked) SPMC lists.
    /// [`Self::init`] must be called before use.
    pub const fn new() -> Self {
        assert!(N >= 1);
        assert!(C >= 1 && C <= crate::config::MAX_SUPPORTED_CLASSES);
        Self {
            heaps: [const { ThreadHeap::new() }; N],
            help: HelpTable::new(),
            pool: FixedSpanPool::new(),
            registry: ThreadRegistry::new(N),
            stats: AllocatorStats::new(),
        }
    }

    /// Wire up the self-referential SPMC list dummies and install the
    /// backing memory region.
    ///
    /// # Safety
    /// Must be called exactly once, before the allocator is shared, and the
    /// allocator must not move afterwards (pin it: static, Box, or
    /// not-moved stack slot). `region`/`len`: see `FixedSpanPool::set_region`.
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(feature = "std")] {
    /// use wf_alloc::WfSpanAllocator;
    /// use wf_alloc::region::OwnedRegion;
    ///
    /// let region = OwnedRegion::new(16);
    /// let alloc = Box::leak(Box::new(WfSpanAllocator::<4, 8>::new()));
    /// // Must be called exactly once before sharing across threads.
    /// unsafe { alloc.init(region.ptr(), region.len()) };
    /// # }
    /// ```
    pub unsafe fn init(&self, region: *mut u8, len: usize) {
        // Bounded loops: N * C.
        for heap in &self.heaps {
            for list in &heap.public_spans {
                // SAFETY: pre-share, called once per contract.
                unsafe { list.init() };
            }
        }
        // SAFETY: forwarded contract.
        unsafe { self.pool.set_region(region, len) };
    }

    /// Register the calling thread; None after N registrations.
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(feature = "std")] {
    /// use wf_alloc::WfSpanAllocator;
    /// use wf_alloc::region::OwnedRegion;
    ///
    /// let region = OwnedRegion::new(16);
    /// let alloc = Box::leak(Box::new(WfSpanAllocator::<2, 8>::new()));
    /// unsafe { alloc.init(region.ptr(), region.len()) };
    ///
    /// let t0 = alloc.register_thread(); // first registration
    /// let t1 = alloc.register_thread(); // second registration
    /// let t2 = alloc.register_thread(); // exceeds N=2, returns None
    /// assert!(t0.is_some());
    /// assert!(t1.is_some());
    /// assert!(t2.is_none());
    /// # }
    /// ```
    pub fn register_thread(&self) -> Option<ThreadToken> {
        self.registry.register()
    }

    /// Paper's approximate bound on ADDITIONAL footprint (bytes) beyond
    /// what a fully linearizable allocator would hold, with P = N.
    ///
    /// # Examples
    ///
    /// ```
    /// use wf_alloc::WfSpanAllocator;
    ///
    /// let bound = WfSpanAllocator::<4, 8>::theoretical_extra_bound();
    /// assert!(bound > 0);
    /// ```
    pub const fn theoretical_extra_bound() -> usize {
        theoretical_extra_bound(N, C, crate::config::SPAN_SIZE, N)
    }

    /// Allocate; returns null on unsupported layout or pool exhaustion.
    ///
    /// # Safety
    /// `token` must come from this allocator's registry and be used by only
    /// one thread at a time.
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(feature = "std")] {
    /// use core::alloc::Layout;
    /// use wf_alloc::WfSpanAllocator;
    /// use wf_alloc::region::OwnedRegion;
    ///
    /// let region = OwnedRegion::new(16);
    /// let alloc = Box::leak(Box::new(WfSpanAllocator::<4, 8>::new()));
    /// unsafe { alloc.init(region.ptr(), region.len()) };
    /// let token = alloc.register_thread().unwrap();
    ///
    /// let layout = Layout::new::<u32>();
    /// let ptr = unsafe { alloc.alloc_with_token(layout, token) };
    /// assert!(!ptr.is_null());
    /// unsafe { alloc.dealloc_with_token(ptr, layout, token) };
    /// # }
    /// ```
    pub unsafe fn alloc_with_token(&self, layout: Layout, token: ThreadToken) -> *mut u8 {
        let mut step = StepCounter::new();
        // SAFETY: forwarded contract.
        unsafe { self.alloc_with_token_counted(layout, token, &mut step) }
    }

    /// Allocation with explicit step accounting (used by tests/benches to
    /// check wait-freedom bounds).
    ///
    /// # Safety
    /// See [`Self::alloc_with_token`].
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(feature = "std")] {
    /// use core::alloc::Layout;
    /// use wf_alloc::{WfSpanAllocator, StepCounter};
    /// use wf_alloc::region::OwnedRegion;
    ///
    /// let region = OwnedRegion::new(16);
    /// let alloc = Box::leak(Box::new(WfSpanAllocator::<4, 8>::new()));
    /// unsafe { alloc.init(region.ptr(), region.len()) };
    /// let token = alloc.register_thread().unwrap();
    ///
    /// let layout = Layout::new::<u64>();
    /// let mut step = StepCounter::new();
    /// let ptr = unsafe { alloc.alloc_with_token_counted(layout, token, &mut step) };
    /// assert!(!ptr.is_null());
    /// // step records exactly how many atomic operations this alloc took.
    /// unsafe { alloc.dealloc_with_token(ptr, layout, token) };
    /// # }
    /// ```
    pub unsafe fn alloc_with_token_counted(
        &self,
        layout: Layout,
        token: ThreadToken,
        step: &mut StepCounter,
    ) -> *mut u8 {
        let Some(class) = size_to_class(layout.size(), layout.align()) else {
            return core::ptr::null_mut();
        };
        if class >= C {
            return core::ptr::null_mut();
        }
        let tid = token.id;
        debug_assert!(tid < N);
        let list = &self.heaps[tid].local_spans[class];

        // 1. Rotate through privately held spans. Bounded: K + 1 iterations.
        for _ in 0..=LOCAL_SPAN_LIMIT_K {
            let span = list.front();
            if span.is_null() {
                break;
            }
            // SAFETY: front of our own local list => we own `span`.
            unsafe {
                let p = alloc_from_local_span(span, step);
                if !p.is_null() {
                    return p;
                }
                // Local list empty: absorb remote frees (bounded).
                if self.refill_from_remote(span, step) {
                    let p = alloc_from_local_span(span, step);
                    if !p.is_null() {
                        return p;
                    }
                }
                // Nothing reusable now: rotate it out.
                list.pop_front();
                if (*span).local.pending_remote.load(Ordering::Relaxed).is_null()
                    && self.try_discard(span, tid, step)
                {
                    AllocatorStats::bump(&self.stats.discarded_spans);
                } else {
                    // Blocked by a stalled remote producer or freshly
                    // re-owned: keep it, retry on a later rotation.
                    list.push_back(span);
                }
            }
        }

        // 2. Acquire a span from public SPMC span-lists (bounded helping).
        // SAFETY: tid is a valid registered id per token contract.
        let span =
            unsafe { spanlists_acquire_span::<DefaultCas2Backend, N, C>(self, tid, class, step) };
        if !span.is_null() {
            // SAFETY: acquire hands us exclusive ownership of `span`.
            unsafe {
                debug_assert_eq!((*span).owner.load(Ordering::Relaxed), OWNER_PUBLIC);
                debug_assert_eq!((*span).size_class.load(Ordering::Relaxed), class);
                (*span).owner.store(tid, Ordering::Release);
                (*span)
                    .state
                    .store(SpanState::NonEmptyLocal as usize, Ordering::Relaxed);
                list.push_front(span);
                let p = alloc_from_local_span(span, step);
                if !p.is_null() {
                    return p;
                }
                // All of its free blocks may sit in the remote list.
                if self.refill_from_remote(span, step) {
                    let p = alloc_from_local_span(span, step);
                    if !p.is_null() {
                        return p;
                    }
                }
            }
        }

        // 3. Take a raw span from the fixed pool (single FAA).
        let raw = self.pool.acquire_raw_span(step);
        if !raw.is_null() {
            // SAFETY: the pool hands out each span exactly once.
            unsafe {
                let span = init_span(raw, class, class_to_size(class), tid);
                AllocatorStats::bump(&self.stats.allocated_spans);
                list.push_front(span);
                return alloc_from_local_span(span, step);
            }
        }

        // 4. Exhaustion: fixed backend returns null (never OS allocation).
        core::ptr::null_mut()
    }

    /// Deallocate. O(1) bounded: local push, or remote SWAP + FAA (+ one
    /// claim CAS).
    ///
    /// # Safety
    /// `ptr` must have been returned by this allocator and not yet freed
    /// (double free is caller UB in release builds); `token` as in alloc.
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(feature = "std")] {
    /// use core::alloc::Layout;
    /// use wf_alloc::WfSpanAllocator;
    /// use wf_alloc::region::OwnedRegion;
    ///
    /// let region = OwnedRegion::new(16);
    /// let alloc = Box::leak(Box::new(WfSpanAllocator::<4, 8>::new()));
    /// unsafe { alloc.init(region.ptr(), region.len()) };
    /// let token = alloc.register_thread().unwrap();
    ///
    /// let layout = Layout::new::<u64>();
    /// let ptr = unsafe { alloc.alloc_with_token(layout, token) };
    /// assert!(!ptr.is_null());
    ///
    /// // Any registered thread may free a pointer (remote-free is O(1) bounded).
    /// unsafe { alloc.dealloc_with_token(ptr, layout, token) };
    /// # }
    /// ```
    pub unsafe fn dealloc_with_token(&self, ptr: *mut u8, layout: Layout, token: ThreadToken) {
        let mut step = StepCounter::new();
        // SAFETY: forwarded contract.
        unsafe { self.dealloc_with_token_counted(ptr, layout, token, &mut step) }
    }

    /// Deallocation with explicit step accounting.
    ///
    /// # Safety
    /// See [`Self::dealloc_with_token`].
    pub unsafe fn dealloc_with_token_counted(
        &self,
        ptr: *mut u8,
        _layout: Layout,
        token: ThreadToken,
        step: &mut StepCounter,
    ) {
        if ptr.is_null() {
            return;
        }
        let span = span_from_ptr(ptr);
        let block = block_from_payload(ptr);
        // SAFETY: `ptr` belongs to a live span of this allocator.
        let owner = unsafe { (*span).owner.load(Ordering::Acquire) };
        if owner == token.id {
            // SAFETY: we are the owner thread.
            unsafe { self.dealloc_local(span, block, token, step) };
        } else {
            // SAFETY: remote path never touches owner-local state.
            unsafe { self.dealloc_remote(span, block, token, step) };
        }
    }

    /// Owner-thread free: local push; publish the span if it became full
    /// and the heap holds more than K spans of this class.
    ///
    /// # Safety
    /// Caller thread must own `span`; `block` is an allocated block of it.
    unsafe fn dealloc_local(
        &self,
        span: *mut SpanHeader,
        block: *mut Block,
        token: ThreadToken,
        step: &mut StepCounter,
    ) {
        step.local_steps += 1;
        // SAFETY: owner-private list/counter.
        unsafe {
            dealloc_to_local_span(span, block);

            let q = (*span).local.free_count.load(Ordering::Relaxed) as isize;
            let g = (*span).remote.free_count.load(Ordering::Relaxed);
            let m = (*span).block_count.load(Ordering::Relaxed) as isize;
            if q + g == m {
                (*span)
                    .state
                    .store(SpanState::FullLocal as usize, Ordering::Relaxed);
                let class = (*span).size_class.load(Ordering::Relaxed);
                let list = &self.heaps[token.id].local_spans[class];
                if list.len() > LOCAL_SPAN_LIMIT_K {
                    // Publish the surplus full span to our public list.
                    // remove_bounded is bounded by the current list length.
                    if list.remove_bounded(span, list.len()) {
                        (*span).owner.store(OWNER_PUBLIC, Ordering::Release);
                        (*span)
                            .state
                            .store(SpanState::FullPublic as usize, Ordering::Relaxed);
                        self.heaps[token.id].public_spans[class].enqueue_by_owner(span, step);
                        AllocatorStats::bump(&self.stats.published_spans);
                    }
                }
            }
        }
    }

    /// Non-owner free: MPSC push + FAA; if the FAA moves g from 0 to 1 the
    /// span may have been discarded — try to claim it (one CAS).
    ///
    /// # Safety
    /// `block` is an allocated block of `span`; caller is not the owner.
    unsafe fn dealloc_remote(
        &self,
        span: *mut SpanHeader,
        block: *mut Block,
        token: ThreadToken,
        step: &mut StepCounter,
    ) {
        step.remote_steps += 1;
        step.swap_ops += 1;
        step.faa_ops += 1;
        // SAFETY: remote MPSC push of an exclusively held block.
        let old = unsafe {
            (*span).remote.free.push(block);
            (*span).remote.free_count.fetch_add(1, Ordering::AcqRel)
        };
        if old == 0 {
            step.cas_attempts += 1;
            // SAFETY: claim CAS only succeeds on a discarded span.
            unsafe {
                if (*span)
                    .owner
                    .compare_exchange(OWNER_NONE, token.id, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    (*span)
                        .state
                        .store(SpanState::NonEmptyLocal as usize, Ordering::Relaxed);
                    let class = (*span).size_class.load(Ordering::Relaxed);
                    self.heaps[token.id].local_spans[class].push_front(span);
                    AllocatorStats::bump(&self.stats.claimed_spans);
                }
            }
        }
    }

    /// Consume the pending chain and/or freshly reclaimed remote list into
    /// the local free-list. Returns whether any block was absorbed.
    /// Bounded by 2 * blocks_per_span.
    ///
    /// # Safety
    /// Caller thread must own `span`.
    unsafe fn refill_from_remote(&self, span: *mut SpanHeader, step: &mut StepCounter) -> bool {
        let mut got = 0usize;
        // SAFETY: owner-private pending chain and local list.
        unsafe {
            // (a) A previously stashed chain blocked at an UNLINKED link.
            let pending = (*span)
                .local
                .pending_remote
                .swap(core::ptr::null_mut(), Ordering::Relaxed);
            if !pending.is_null() {
                let (k, leftover) = append_remote_to_local_bounded(span, pending, step);
                (*span)
                    .local
                    .pending_remote
                    .store(leftover, Ordering::Relaxed);
                got += k;
                if !leftover.is_null() {
                    AllocatorStats::bump(&self.stats.remote_blocked_events);
                    // Do not also reclaim the live list while a chain is
                    // stashed: one pending chain per span, by invariant.
                    if k > 0 {
                        step.faa_ops += 1;
                        (*span)
                            .remote
                            .free_count
                            .fetch_sub(k as isize, Ordering::AcqRel);
                    }
                    return got > 0;
                }
            }
            // (b) The live remote list.
            step.swap_ops += 1;
            let chain = (*span).remote.free.reclaim_all();
            if !chain.is_null() {
                let (k, leftover) = append_remote_to_local_bounded(span, chain, step);
                (*span)
                    .local
                    .pending_remote
                    .store(leftover, Ordering::Relaxed);
                if !leftover.is_null() {
                    AllocatorStats::bump(&self.stats.remote_blocked_events);
                }
                got += k;
            }
            if got > 0 {
                step.faa_ops += 1;
                (*span)
                    .remote
                    .free_count
                    .fetch_sub(got as isize, Ordering::AcqRel);
            }
        }
        got > 0
    }

    /// Try to discard an exhausted span (no local, no pending, no visible
    /// remote blocks). Bounded: two loads, at most one CAS, no loop.
    ///
    /// Returns true if the span left this thread's ownership (discarded or
    /// claimed by a racing remote deallocator); false if it must be kept.
    ///
    /// # Safety
    /// Caller thread must own `span`; span must be unlinked from any list,
    /// with empty local free-list and no pending chain.
    unsafe fn try_discard(&self, span: *mut SpanHeader, tid: usize, step: &mut StepCounter) -> bool {
        // SAFETY: owner-side loads/stores on a span we exclusively own.
        unsafe {
            debug_assert!((*span).local.free.is_empty());
            if (*span).remote.free_count.load(Ordering::Acquire) != 0 {
                return false; // remote blocks exist; keep and reclaim later
            }
            (*span).owner.store(OWNER_NONE, Ordering::Release);
            (*span)
                .state
                .store(SpanState::Discarded as usize, Ordering::Relaxed);
            if (*span).remote.free_count.load(Ordering::Acquire) == 0 {
                return true; // discarded; a future g 0->1 freer claims it
            }
            // A remote free landed in the window and may have failed its
            // claim CAS. Try to take the span back (single CAS, no retry).
            step.cas_attempts += 1;
            if (*span)
                .owner
                .compare_exchange(OWNER_NONE, tid, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                (*span)
                    .state
                    .store(SpanState::NonEmptyLocal as usize, Ordering::Relaxed);
                return false; // re-owned; caller keeps it
            }
            true // the racing freer claimed it
        }
    }
}

impl<const N: usize, const C: usize> Default for WfSpanAllocator<N, C> {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: all shared state is atomics, SPMC/MPSC wait-free structures, or
// owner-only fields whose handover is release/acquire synchronized.
unsafe impl<const N: usize, const C: usize> Send for WfSpanAllocator<N, C> {}
unsafe impl<const N: usize, const C: usize> Sync for WfSpanAllocator<N, C> {}
