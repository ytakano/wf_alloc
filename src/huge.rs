//! Wait-free huge-object path: GiB-scale allocations from a fixed slot
//! directory (guide Appendix B).
//!
//! Requests of at least one huge granule (`HUGE_GRANULE_SPANS` spans;
//! default 1 GiB) bypass the large-run path entirely. The large path's
//! helping protocol and per-thread caches are bounded, but at GiB scale
//! those bounds are unacceptable: a HelpRecord may strand one multi-GiB
//! run per thread per class, and `local_runs` may retain up to K of them
//! (guide B.13: with N = 64 that bound is already ~192 GiB). The huge path
//! therefore uses a **fixed directory of `HugeRunSlot`s** — no SPMC lists,
//! no helping, no per-thread caches:
//!
//! - Allocation scans at most `MAX_HUGE_RUN_CLASSES *
//!   MAX_HUGE_RUNS_PER_CLASS` slots, with AT MOST ONE claim CAS per slot
//!   and no retry loop (B.7/B.8).
//! - Slot memory is carved lazily from the SAME `FixedSpanPool` region as
//!   everything else (one FAA via `acquire_raw_run`), preserving the
//!   single-region init. A carved slot keeps its memory forever,
//!   alternating FREE ↔ ALLOCATED; it never returns to the pool.
//! - Deallocation recovers the slot by a bounded directory scan over the
//!   same ≤ R × SLOTS entries (address-range reverse lookup) and stores
//!   FREE. No CAS, no unbounded loops.
//!
//! ## No hidden header (deviation from guide B.6, chosen deliberately)
//!
//! All metadata lives in the directory slot; the payload is the bare run.
//! With B.6's hidden header, every huge request (size ≥ one granule by the
//! dispatch rule) would need `size + header > granule` bytes, so class 0
//! could never serve any request and an exactly-1-granule allocation would
//! consume a 2-granule run. Header-less placement lets requests of exactly
//! 1/2/4 granules occupy runs of exactly that size. B.4 explicitly allows
//! dispatch metadata other than a hidden header; double-free detection is
//! preserved through the slot state.
//!
//! Huge runs carry NO `SpanHeader` and never touch the small-span pagemap
//! (B.11). Like the rest of the crate, the huge path returns a contiguous
//! range of the caller-provided region — virtually contiguous; physical
//! contiguity is the region provider's responsibility (B.12).
//!
//! ## Memory layout of one huge allocation (run class r)
//!
//! ```text
//! slot.base (SPAN_SIZE-aligned, 2^r * HUGE_GRANULE_SPANS spans)
//!   ├─ [padding only when layout.align() > SPAN_SIZE]
//!   └─ payload (layout.align()-aligned)             ← returned to caller
//! ```

use core::alloc::Layout;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::align::round_up;
use crate::allocator::WfSpanAllocator;
use crate::config::{MAX_HUGE_GRANULES, MAX_HUGE_RUN_CLASSES, MAX_HUGE_RUNS_PER_CLASS, SPAN_SIZE};
use crate::stats::{AllocatorStats, StepCounter};
use crate::thread::ThreadToken;

/// Slot has no memory yet (lazy carve pending).
pub const HUGE_SLOT_EMPTY: usize = 0;
/// Slot owns a carved run that is currently free.
pub const HUGE_SLOT_FREE: usize = 1;
/// Slot's run is handed to a live user allocation.
pub const HUGE_SLOT_ALLOCATED: usize = 2;

/// One directory entry. `state` is the only synchronization point: a slot
/// is claimed with a single EMPTY/FREE → ALLOCATED CAS; `base` is written
/// once under the EMPTY→ALLOCATED claim and never changes afterwards, so
/// the FREE ↔ ALLOCATED cycle is ABA-free without versioning. The
/// remaining fields are written only under an exclusive ALLOCATED claim.
#[repr(C, align(64))]
pub struct HugeRunSlot {
    pub state: AtomicUsize,
    /// Base address of the carved run; 0 while EMPTY. Immutable once set.
    pub base: AtomicUsize,
    /// Last allocating or freeing thread (stats/debugging only).
    pub owner: AtomicUsize,
    /// Layout of the live allocation, asserted on dealloc (guide B.10).
    pub requested_size: AtomicUsize,
    pub requested_align: AtomicUsize,
}

impl HugeRunSlot {
    pub const fn new() -> Self {
        Self {
            state: AtomicUsize::new(HUGE_SLOT_EMPTY),
            base: AtomicUsize::new(0),
            owner: AtomicUsize::new(usize::MAX),
            requested_size: AtomicUsize::new(0),
            requested_align: AtomicUsize::new(0),
        }
    }
}

impl Default for HugeRunSlot {
    fn default() -> Self {
        Self::new()
    }
}

/// Fixed huge-run directory (guide B.7): one slot array per run class.
/// Class `r` slots hold runs of `2^r` huge granules.
pub struct HugeArena {
    pub slots: [[HugeRunSlot; MAX_HUGE_RUNS_PER_CLASS]; MAX_HUGE_RUN_CLASSES],
}

impl HugeArena {
    pub const fn new() -> Self {
        Self {
            slots: [const { [const { HugeRunSlot::new() }; MAX_HUGE_RUNS_PER_CLASS] };
                MAX_HUGE_RUN_CLASSES],
        }
    }
}

impl Default for HugeArena {
    fn default() -> Self {
        Self::new()
    }
}

/// Granules in a huge run of `class`.
pub const fn huge_class_granules(class: usize) -> usize {
    1 << class
}

impl<const N: usize, const C: usize, const HG: usize> WfSpanAllocator<N, C, HG> {
    /// Bytes in one huge granule (= the huge dispatch threshold).
    pub const HUGE_GRANULE_BYTES: usize = HG * SPAN_SIZE;

    /// Bytes in a huge run of `class`.
    pub const fn huge_class_bytes(class: usize) -> usize {
        (HG * SPAN_SIZE) << class
    }

    /// Smallest huge run class whose run holds a `layout`-aligned payload.
    /// Header-less: a request of exactly `2^r` granules fits class `r`.
    /// `None` if more than `MAX_HUGE_GRANULES` granules are needed (or the
    /// size overflows). Alignments above `SPAN_SIZE` are honored via slack
    /// (run bases are only SPAN_SIZE-aligned).
    pub fn huge_class_for_layout(layout: Layout) -> Option<usize> {
        let slack = layout.align().saturating_sub(SPAN_SIZE);
        let needed = layout.size().checked_add(slack)?;
        let granules = needed.div_ceil(Self::HUGE_GRANULE_BYTES);
        let g = granules.max(1).checked_next_power_of_two()?;
        if g > MAX_HUGE_GRANULES {
            return None;
        }
        Some(g.trailing_zeros() as usize)
    }

    /// Huge allocation (guide B.8 + lazy carve). Bounded: at most
    /// `MAX_HUGE_RUN_CLASSES * MAX_HUGE_RUNS_PER_CLASS` slot scans with one
    /// claim CAS each, plus one raw carve (one FAA, at most one rollback
    /// CAS). No helping, no retry loops; null when no free slot is
    /// observed within the scan (B.7).
    ///
    /// # Safety
    /// As for [`Self::alloc_with_token`].
    pub(crate) unsafe fn alloc_huge_with_token_counted(
        &self,
        layout: Layout,
        token: ThreadToken,
        step: &mut StepCounter,
    ) -> *mut u8 {
        let Some(min_class) = Self::huge_class_for_layout(layout) else {
            return core::ptr::null_mut();
        };

        // Bounded scan: classes × slots, at most one CAS attempt per slot.
        for class in min_class..MAX_HUGE_RUN_CLASSES {
            for slot in &self.huge.slots[class] {
                step.huge_slot_scans += 1;
                match slot.state.load(Ordering::Acquire) {
                    HUGE_SLOT_FREE => {
                        step.cas_attempts += 1;
                        if slot
                            .state
                            .compare_exchange(
                                HUGE_SLOT_FREE,
                                HUGE_SLOT_ALLOCATED,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_err()
                        {
                            continue; // lost the race; next slot, no retry
                        }
                        let base = slot.base.load(Ordering::Relaxed) as *mut u8;
                        debug_assert!(!base.is_null());
                        return Self::finish_huge(slot, base, class, layout, token);
                    }
                    HUGE_SLOT_EMPTY => {
                        step.cas_attempts += 1;
                        if slot
                            .state
                            .compare_exchange(
                                HUGE_SLOT_EMPTY,
                                HUGE_SLOT_ALLOCATED,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_err()
                        {
                            continue;
                        }
                        // Lazy carve from the shared pool: one FAA (+ at
                        // most one rollback CAS inside acquire_raw_run).
                        let span_count = huge_class_granules(class) * HG;
                        let base = self.pool.acquire_raw_run(span_count, step);
                        if base.is_null() {
                            // Pool exhausted: hand the slot back (one
                            // store) and keep scanning. No retry.
                            slot.state.store(HUGE_SLOT_EMPTY, Ordering::Release);
                            continue;
                        }
                        slot.base.store(base as usize, Ordering::Relaxed);
                        AllocatorStats::bump(&self.stats.allocated_huge_runs);
                        return Self::finish_huge(slot, base, class, layout, token);
                    }
                    _ => {} // ALLOCATED: next slot
                }
            }
        }

        // Exhaustion: fixed directory/backed memory full (never OS alloc).
        core::ptr::null_mut()
    }

    /// Record the claimed allocation in the slot and return the aligned
    /// payload. The caller holds the exclusive ALLOCATED claim on `slot`.
    fn finish_huge(
        slot: &HugeRunSlot,
        base: *mut u8,
        class: usize,
        layout: Layout,
        token: ThreadToken,
    ) -> *mut u8 {
        slot.owner.store(token.id, Ordering::Relaxed);
        slot.requested_size.store(layout.size(), Ordering::Relaxed);
        slot.requested_align.store(layout.align(), Ordering::Relaxed);
        // Pointer arithmetic (not int casts) keeps the run's provenance.
        let payload_off = round_up(base as usize, layout.align()) - base as usize;
        debug_assert!(
            payload_off + layout.size() <= Self::huge_class_bytes(class),
            "huge payload exceeds its run"
        );
        // SAFETY: payload_off + layout.size() fits the run (guaranteed by
        // huge_class_for_layout's slack accounting; asserted above).
        unsafe { base.add(payload_off) }
    }

    /// Huge deallocation (guide B.10, slot reverse lookup instead of a
    /// hidden header). Bounded: scans the fixed directory (≤ R × SLOTS
    /// entries) for the slot whose run contains `ptr`, then one release
    /// store makes the slot claimable again. No CAS, no unbounded loops.
    /// Any registered thread may free; the directory is global.
    ///
    /// # Safety
    /// `ptr` must have been returned by the huge path of this allocator
    /// with the same `layout` and not yet freed; `token` as in
    /// [`Self::dealloc_with_token`].
    pub(crate) unsafe fn dealloc_huge_with_token_counted(
        &self,
        ptr: *mut u8,
        layout: Layout,
        token: ThreadToken,
        step: &mut StepCounter,
    ) {
        let addr = ptr as usize;
        // Bounded reverse lookup: R × SLOTS address-range checks.
        for class in 0..MAX_HUGE_RUN_CLASSES {
            for slot in &self.huge.slots[class] {
                step.huge_slot_scans += 1;
                let base = slot.base.load(Ordering::Relaxed);
                if base == 0 || addr < base || addr >= base + Self::huge_class_bytes(class) {
                    continue;
                }
                debug_assert_eq!(
                    slot.state.load(Ordering::Relaxed),
                    HUGE_SLOT_ALLOCATED,
                    "double free of a huge run"
                );
                debug_assert_eq!(slot.requested_size.load(Ordering::Relaxed), layout.size());
                debug_assert_eq!(slot.requested_align.load(Ordering::Relaxed), layout.align());
                slot.owner.store(token.id, Ordering::Relaxed);
                // One atomic release makes the slot claimable again (B.14).
                slot.state.store(HUGE_SLOT_FREE, Ordering::Release);
                return;
            }
        }
        debug_assert!(false, "huge dealloc of a pointer not owned by any slot");
        let _ = layout;
    }
}
