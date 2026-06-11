//! std-only quiescent invariant checker (debug/test harness; NOT part of
//! the wait-free core — it may allocate and walks lists non-atomically).
//!
//! Call only while no other thread is using the allocator.

use std::collections::HashSet;
use std::sync::atomic::Ordering;

use crate::allocator::WfSpanAllocator;
use crate::atomic_backend::{Cas2Backend, DefaultCas2Backend};
use crate::block::UNLINKED;
use crate::config::{OWNER_PUBLIC, SPAN_SIZE};
use crate::help_record::EncodedReq;
use crate::span::SpanHeader;
use crate::spmc_span_list::SpanNode;

/// Panics on any violated invariant. Returns the number of spans seen in
/// local lists, public lists, and help records (discarded spans are in no
/// list and are not counted).
///
/// # Safety
/// The allocator must be initialized and quiescent (no concurrent ops).
pub unsafe fn check_quiescent<const N: usize, const C: usize>(
    alloc: &WfSpanAllocator<N, C>,
) -> (usize, usize, usize) {
    let mut seen: HashSet<usize> = HashSet::new();
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
                    local += 1;
                    cur = (*cur).local.next_local.load(Ordering::Relaxed);
                }
                assert_eq!(walked, list.len(), "local list length mismatch");
            }

            for (class, list) in heap.public_spans.iter().enumerate() {
                // Walk the SPMC queue from the dummy at head.
                let head = DefaultCas2Backend::load(list.head_ptr());
                let mut node = head.ptr as *const SpanNode;
                assert!(!node.is_null(), "uninitialized SPMC list");
                // Skip the dummy: spans live in nodes after it.
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
                    check_span(span, None, class);
                    assert_eq!(
                        (*span).owner.load(Ordering::Relaxed),
                        OWNER_PUBLIC,
                        "public span must be ownerless (PUBLIC)"
                    );
                    public += 1;
                    node = next;
                }
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
                    assert_eq!(
                        (*span).owner.load(Ordering::Relaxed),
                        OWNER_PUBLIC,
                        "help-record span must be PUBLIC"
                    );
                    in_help += 1;
                }
            }
        }
    }
    (local, public, in_help)
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
