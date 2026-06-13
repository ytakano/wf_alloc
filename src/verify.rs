//! std-only quiescent invariant checker (debug/test harness; NOT part of
//! the wait-free core — it may allocate and walks lists non-atomically).
//!
//! Call only while no other thread is using the allocator.
//!
//! Only LISTS are walked, never the raw region: the interior spans of a
//! large run carry no `SpanHeader`, and spans handed to live user
//! allocations are reachable only through their owners.

use std::collections::HashSet;
use std::sync::atomic::Ordering;

use crate::allocator::WfSpanAllocator;
use crate::atomic_backend::{Cas2Backend, DefaultCas2Backend};
use crate::block::UNLINKED;
use crate::config::{OWNER_PUBLIC, SPAN_SIZE};
use crate::help_record::EncodedReq;
use crate::huge::{HUGE_SLOT_ALLOCATED, HUGE_SLOT_EMPTY, HUGE_SLOT_FREE};
use crate::span::{SpanHeader, SpanState};
use crate::spmc_span_list::SpanNode;

/// Tracks which pool span indices are claimed by walked spans/runs, so a
/// free run can never overlap a small span or another run (guide A.13).
struct PageOccupancy {
    base: usize,
    pages: HashSet<usize>,
}

impl PageOccupancy {
    fn claim(&mut self, hdr: *mut SpanHeader, span_count: usize) {
        let addr = hdr as usize;
        if self.base == 0 || addr < self.base {
            return; // pool not installed or header outside it (tests' own spans)
        }
        let first = (addr - self.base) / SPAN_SIZE;
        for i in 0..span_count {
            assert!(
                self.pages.insert(first + i),
                "span index {} claimed twice (overlapping span/run)",
                first + i
            );
        }
    }
}

/// Panics on any violated invariant. Returns the number of spans AND runs
/// seen in local lists, public lists, and help records (discarded spans are
/// in no list and are not counted; neither are runs/blocks held by live
/// user allocations, nor huge directory slots).
///
/// # Safety
/// The allocator must be initialized and quiescent (no concurrent ops).
pub unsafe fn check_quiescent<const C: usize, const HG: usize>(
    alloc: &WfSpanAllocator<C, HG>,
) -> (usize, usize, usize) {
    let mut seen: HashSet<usize> = HashSet::new();
    let mut pages = PageOccupancy {
        base: alloc.pool.base_addr(),
        pages: HashSet::new(),
    };
    let span_limit = alloc.pool.spans_total() + 1;
    let mut local = 0usize;
    let mut public = 0usize;
    let mut in_help = 0usize;

    // SAFETY (whole fn): quiescent per contract; pointers come from the
    // allocator's own lists and the fixed pool, hence valid span headers.
    unsafe {
        for (tid, heap) in alloc.heaps.iter().enumerate() {
            for (class, list) in heap.local_spans.iter().enumerate() {
                let mut cur = list.front();
                let mut walked = 0usize;
                while !cur.is_null() {
                    walked += 1;
                    assert!(walked <= span_limit, "local list cycle/overflow");
                    assert!(
                        seen.insert(cur as usize),
                        "span {cur:p} appears in two lists"
                    );
                    check_span(cur, Some(tid), class);
                    pages.claim(cur, 1);
                    local += 1;
                    cur = (*cur).local.next_local.load(Ordering::Relaxed);
                }
                assert_eq!(walked, list.len(), "local list length mismatch");
            }

            for (class, list) in heap.public_spans.iter().enumerate() {
                public += walk_public_list(list, span_limit, &mut seen, &mut |span| {
                    check_span(span, None, class);
                    pages.claim(span, 1);
                });
            }

            for (class, list) in heap.local_runs.iter().enumerate() {
                let mut cur = list.front();
                let mut walked = 0usize;
                while !cur.is_null() {
                    walked += 1;
                    assert!(walked <= span_limit, "local run list cycle/overflow");
                    assert!(
                        seen.insert(cur as usize),
                        "run {cur:p} appears in two lists"
                    );
                    check_run(cur, Some(tid), class);
                    pages.claim(cur, 1usize << class);
                    local += 1;
                    cur = (*cur).local.next_local.load(Ordering::Relaxed);
                }
                assert_eq!(walked, list.len(), "local run list length mismatch");
            }

            for (class, list) in heap.public_runs.iter().enumerate() {
                public += walk_public_list(list, span_limit, &mut seen, &mut |run| {
                    check_run(run, None, class);
                    pages.claim(run, 1usize << class);
                });
            }
        }

        for row in alloc.help.records.iter() {
            for (class, rec) in row.iter().enumerate() {
                let enc = EncodedReq(rec.phase_pending_or_span.load(Ordering::Relaxed));
                if !enc.is_empty() && !enc.is_pending() {
                    let span = enc.span();
                    assert!(
                        seen.insert(span as usize),
                        "help-record span {span:p} also in a list"
                    );
                    check_span(span, None, class);
                    pages.claim(span, 1);
                    assert_eq!(
                        (*span).owner.load(Ordering::Relaxed),
                        OWNER_PUBLIC,
                        "help-record span must be PUBLIC"
                    );
                    in_help += 1;
                }
            }
        }

        for row in alloc.help.run_records.iter() {
            for (class, rec) in row.iter().enumerate() {
                let enc = EncodedReq(rec.phase_pending_or_span.load(Ordering::Relaxed));
                if !enc.is_empty() && !enc.is_pending() {
                    let run = enc.span();
                    assert!(
                        seen.insert(run as usize),
                        "run-help-record run {run:p} also in a list"
                    );
                    check_run(run, None, class);
                    pages.claim(run, 1usize << class);
                    assert_eq!(
                        (*run).owner.load(Ordering::Relaxed),
                        OWNER_PUBLIC,
                        "run-help-record run must be PUBLIC"
                    );
                    in_help += 1;
                }
            }
        }

        // Huge directory slots: a non-EMPTY slot owns its carved granules
        // forever (FREE and ALLOCATED alike) — claim them so no small span
        // or large run may overlap a huge run (guide B.14).
        for (class, pool) in alloc.huge.slots.iter().enumerate() {
            for slot in pool {
                let state = slot.state.load(Ordering::Relaxed);
                let base = slot.base.load(Ordering::Relaxed);
                match state {
                    HUGE_SLOT_EMPTY => {
                        assert_eq!(base, 0, "EMPTY huge slot with carved memory");
                    }
                    HUGE_SLOT_FREE | HUGE_SLOT_ALLOCATED => {
                        assert_ne!(base, 0, "carved huge slot without memory");
                        pages.claim(base as *mut SpanHeader, (1usize << class) * HG);
                    }
                    other => panic!("invalid huge slot state {other}"),
                }
            }
        }
    }
    (local, public, in_help)
}

/// Walk one SPMC list from its dummy head, asserting each entry is fresh,
/// PUBLIC-owned, and passing it to `check`. Returns the entry count.
///
/// # Safety
/// Quiescent; `list` initialized.
unsafe fn walk_public_list(
    list: &crate::spmc_span_list::SpmcSpanList,
    span_limit: usize,
    seen: &mut HashSet<usize>,
    check: &mut dyn FnMut(*mut SpanHeader),
) -> usize {
    let mut count = 0usize;
    // SAFETY: contract.
    unsafe {
        let head = DefaultCas2Backend::load(list.head_ptr());
        let mut node = head.ptr as *const SpanNode;
        assert!(!node.is_null(), "uninitialized SPMC list");
        // Skip the dummy: entries live in nodes after it.
        let mut walked = 0usize;
        loop {
            let next = (*node).next.load(Ordering::Acquire);
            if next.is_null() {
                break;
            }
            walked += 1;
            assert!(walked <= span_limit, "public list cycle/overflow");
            let span = (*next).span.load(Ordering::Relaxed);
            assert!(
                seen.insert(span as usize),
                "span {span:p} appears in two lists (public)"
            );
            check(span);
            assert_eq!(
                (*span).owner.load(Ordering::Relaxed),
                OWNER_PUBLIC,
                "public span must be ownerless (PUBLIC)"
            );
            count += 1;
            node = next;
        }
    }
    count
}

/// Per-span checks: counts in range, local free-list well formed and of
/// length q, no duplicate blocks, all blocks inside the span.
///
/// # Safety
/// `span` must be a valid initialized span header; quiescent.
unsafe fn check_span(span: *mut SpanHeader, owner: Option<usize>, class: usize) {
    // SAFETY: contract.
    unsafe {
        let q = (*span).local.free_count.load(Ordering::Relaxed);
        let g = (*span).remote.free_count.load(Ordering::Relaxed);
        let m = (*span).block_count.load(Ordering::Relaxed);
        assert_eq!(
            (*span).size_class.load(Ordering::Relaxed),
            class,
            "span filed under wrong size class"
        );
        if let Some(tid) = owner {
            assert_eq!((*span).owner.load(Ordering::Relaxed), tid, "owner mismatch");
        }
        assert!(q <= m, "q={q} exceeds m={m}");
        assert!(g >= 0, "negative g at quiescence: {g}");
        assert!(q as isize + g <= m as isize, "q+g exceeds m");

        // Walk the local free-list: exactly q blocks, all distinct, all in
        // this span's payload.
        let base = span as usize;
        let mut blocks: HashSet<usize> = HashSet::new();
        let mut cur = (*span).local.free.peek_for_verify();
        let mut walked = 0usize;
        while !cur.is_null() {
            assert_ne!(cur, UNLINKED, "UNLINKED leaked into local free-list");
            walked += 1;
            assert!(walked <= m, "local free-list longer than block_count");
            let addr = cur as usize;
            assert!(
                addr > base && addr < base + SPAN_SIZE,
                "free block outside its span"
            );
            assert!(blocks.insert(addr), "duplicate block in local free-list");
            cur = (*cur).next.load(Ordering::Relaxed);
        }
        assert_eq!(walked, q, "local free-list length != q");
    }
}

/// Per-run checks: class/span-count consistent, free state matches list
/// membership, and the run never uses block free-lists or the remote MPSC
/// path (guide A.10/A.13).
///
/// # Safety
/// `run` must be a valid initialized run header; quiescent.
unsafe fn check_run(run: *mut SpanHeader, owner: Option<usize>, class: usize) {
    // SAFETY: contract.
    unsafe {
        assert_eq!(
            (*run).size_class.load(Ordering::Relaxed),
            class,
            "run filed under wrong run class"
        );
        assert_eq!(
            (*run).block_count.load(Ordering::Relaxed),
            1usize << class,
            "run span_count mismatch"
        );
        let st = (*run).state.load(Ordering::Relaxed);
        match owner {
            Some(tid) => {
                assert_eq!(
                    (*run).owner.load(Ordering::Relaxed),
                    tid,
                    "run owner mismatch"
                );
                assert_eq!(st, SpanState::RunFreeLocal as usize, "local free run state");
            }
            None => {
                assert_eq!(
                    st,
                    SpanState::RunFreePublic as usize,
                    "public free run state"
                );
            }
        }
        assert_eq!(
            (*run).local.free_count.load(Ordering::Relaxed),
            0,
            "run must not carry a block free-list"
        );
        assert_eq!(
            (*run).remote.free_count.load(Ordering::Relaxed),
            0,
            "run must not use the remote free-list"
        );
    }
}
