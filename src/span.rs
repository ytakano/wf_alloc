//! Span header layout, initialization, and owner-local alloc/free.
//!
//! A span is a `SPAN_SIZE`-byte, `SPAN_SIZE`-aligned region:
//! `[SpanHeader | padding | block 0 | block 1 | ...]`.
//! All header fields are atomics so shared `&SpanHeader` access is sound;
//! owner-only fields use `Relaxed` ordering (see `local_list.rs`).

use core::sync::atomic::{AtomicIsize, AtomicPtr, AtomicUsize, Ordering};

use crate::block::{Block, block_payload};
use crate::config::{SPAN_HEADER_RESERVE, SPAN_SIZE};
use crate::local_list::LocalFreeList;
use crate::remote_mpsc::RemoteMpscFreeList;
use crate::size_class::first_block_offset;
use crate::spmc_span_list::SpanNode;
use crate::stats::StepCounter;

/// Advisory span state. The authoritative state is the combination of
/// `owner`, the free counts, and list membership (see docs/invariants.md).
#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanState {
    Raw = 0,
    FullLocal = 1,
    NonEmptyLocal = 2,
    FullPublic = 3,
    NonEmptyPublic = 4,
    Discarded = 5,
    /// Large run handed to a user allocation (whole-run, guide Appendix A).
    RunAllocated = 6,
    /// Free large run held in an owner's private local run-list.
    RunFreeLocal = 7,
    /// Free large run in a public SPMC run-list or a run help record.
    RunFreePublic = 8,
}

/// Owner-thread-only metadata, kept on its own cache line.
#[repr(C, align(64))]
pub struct LocalMeta {
    /// Local free-list (`q` blocks).
    pub free: LocalFreeList,
    /// `q`: number of blocks in `free`.
    pub free_count: AtomicUsize,
    /// Remote chain reclaimed earlier whose consumption stopped at an
    /// UNLINKED link. Retried on a later allocation; at most one per span.
    pub pending_remote: AtomicPtr<Block>,
    /// Intrusive link for the owner's private local span-list.
    pub next_local: AtomicPtr<SpanHeader>,
}

/// Remote-free-path metadata, kept on its own cache line.
#[repr(C, align(64))]
pub struct RemoteMeta {
    /// MPSC free-list of remotely freed blocks.
    pub free: RemoteMpscFreeList,
    /// `g`: globally visible free count. Incremented by remote frees (FAA),
    /// decremented by the owner when it absorbs reclaimed blocks. May dip
    /// negative transiently (push happens before the producer's FAA).
    pub free_count: AtomicIsize,
}

#[repr(C, align(64))]
pub struct SpanHeader {
    /// Owner thread id, `OWNER_NONE` (discarded) or `OWNER_PUBLIC`.
    pub owner: AtomicUsize,
    pub size_class: AtomicUsize,
    pub block_size: AtomicUsize,
    /// `m`: maximum number of blocks in this span.
    pub block_count: AtomicUsize,
    /// Advisory `SpanState` as usize.
    pub state: AtomicUsize,
    /// The SPMC span-list node this span currently owns (nodes migrate
    /// between spans and list dummies; see spmc_span_list.rs).
    pub node: AtomicPtr<SpanNode>,
    /// Initial node storage for this span.
    pub node_storage: SpanNode,
    pub local: LocalMeta,
    pub remote: RemoteMeta,
}

const _: () = assert!(core::mem::size_of::<SpanHeader>() <= SPAN_HEADER_RESERVE);
const _: () = assert!(core::mem::align_of::<SpanHeader>() <= SPAN_SIZE);

/// Recover the span header from any pointer into the span (header or block).
/// Relies on `SPAN_SIZE`-alignment of spans.
pub fn span_from_ptr(ptr: *mut u8) -> *mut SpanHeader {
    let addr = ptr as usize;
    let base = addr & !(SPAN_SIZE - 1);
    base as *mut SpanHeader
}

/// Initialize a raw span in place for `size_class` and hand it to `owner`.
/// Builds the local free-list over all blocks (bounded by `block_count`).
///
/// # Safety
/// `base` must point to the start of a `SPAN_SIZE`-byte, `SPAN_SIZE`-aligned
/// region exclusively owned by the caller, not yet visible to other threads.
pub unsafe fn init_span(
    base: *mut u8,
    size_class: usize,
    block_size: usize,
    owner: usize,
) -> *mut SpanHeader {
    debug_assert!((base as usize).is_multiple_of(SPAN_SIZE));
    let span = base as *mut SpanHeader;
    // SAFETY: the region is exclusively owned and large enough for the
    // header (const-asserted above); we overwrite it with a fresh header.
    unsafe {
        core::ptr::write(
            span,
            SpanHeader {
                owner: AtomicUsize::new(owner),
                size_class: AtomicUsize::new(size_class),
                block_size: AtomicUsize::new(block_size),
                block_count: AtomicUsize::new(0),
                state: AtomicUsize::new(SpanState::FullLocal as usize),
                node: AtomicPtr::new(core::ptr::null_mut()),
                node_storage: SpanNode::new(),
                local: LocalMeta {
                    free: LocalFreeList::new(),
                    free_count: AtomicUsize::new(0),
                    pending_remote: AtomicPtr::new(core::ptr::null_mut()),
                    next_local: AtomicPtr::new(core::ptr::null_mut()),
                },
                remote: RemoteMeta {
                    free: RemoteMpscFreeList::new(),
                    free_count: AtomicIsize::new(0),
                },
            },
        );

        let node_ptr = &raw mut (*span).node_storage;
        (*span).node.store(node_ptr, Ordering::Relaxed);

        // Build the local free-list: link block i -> block i+1.
        let first = first_block_offset(block_size);
        let count = (SPAN_SIZE - first) / block_size;
        (*span).block_count.store(count, Ordering::Relaxed);

        let base_addr = base as usize;
        let mut head: *mut Block = core::ptr::null_mut();
        // Bounded loop: exactly `count` (= blocks_per_span) iterations.
        let mut i = count;
        while i > 0 {
            i -= 1;
            let b = (base_addr + first + i * block_size) as *mut Block;
            // SAFETY: `b` lies inside the exclusively-owned span payload.
            (*b).next.store(head, Ordering::Relaxed);
            head = b;
        }
        (*span).local.free.push_chain_head_for_init(head, count);
        (*span).local.free_count.store(count, Ordering::Relaxed);
    }
    span
}

/// Initialize the base span of a freshly carved large run and hand it to
/// `owner`. O(1): no block free-list is built — the whole run is one
/// allocation unit. Field reinterpretation for runs: `size_class` holds the
/// run class, `block_count` holds the span count (= `1 << run_class`), and
/// `block_size` holds the run size in bytes (diagnostics).
///
/// # Safety
/// `base` must point to the start of `span_count` contiguous `SPAN_SIZE`-byte,
/// `SPAN_SIZE`-aligned spans exclusively owned by the caller, not yet visible
/// to other threads.
pub unsafe fn init_run(
    base: *mut u8,
    run_class: usize,
    span_count: usize,
    owner: usize,
) -> *mut SpanHeader {
    debug_assert!((base as usize).is_multiple_of(SPAN_SIZE));
    debug_assert_eq!(span_count, 1usize << run_class);
    let run = base as *mut SpanHeader;
    // SAFETY: the region is exclusively owned and the base span is large
    // enough for the header (const-asserted above); we write a fresh header.
    unsafe {
        core::ptr::write(
            run,
            SpanHeader {
                owner: AtomicUsize::new(owner),
                size_class: AtomicUsize::new(run_class),
                block_size: AtomicUsize::new(span_count * SPAN_SIZE),
                block_count: AtomicUsize::new(span_count),
                state: AtomicUsize::new(SpanState::RunAllocated as usize),
                node: AtomicPtr::new(core::ptr::null_mut()),
                node_storage: SpanNode::new(),
                local: LocalMeta {
                    free: LocalFreeList::new(),
                    free_count: AtomicUsize::new(0),
                    pending_remote: AtomicPtr::new(core::ptr::null_mut()),
                    next_local: AtomicPtr::new(core::ptr::null_mut()),
                },
                remote: RemoteMeta {
                    free: RemoteMpscFreeList::new(),
                    free_count: AtomicIsize::new(0),
                },
            },
        );
        let node_ptr = &raw mut (*run).node_storage;
        (*run).node.store(node_ptr, Ordering::Relaxed);
    }
    run
}

/// Pop one block from the owner's local free-list. O(1).
///
/// # Safety
/// Caller must be the current owner of `span`.
pub unsafe fn alloc_from_local_span(span: *mut SpanHeader, step: &mut StepCounter) -> *mut u8 {
    step.local_steps += 1;
    // SAFETY: caller is the owner; the local list is owner-private.
    let block = unsafe { (*span).local.free.pop() };
    if block.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: owner-private counter.
    unsafe {
        let q = (*span).local.free_count.load(Ordering::Relaxed);
        (*span).local.free_count.store(q - 1, Ordering::Relaxed);
        (*span)
            .state
            .store(SpanState::NonEmptyLocal as usize, Ordering::Relaxed);
    }
    block_payload(block)
}

/// Push one block to the owner's local free-list. O(1).
///
/// # Safety
/// Caller must be the current owner of `span`; `block` must be an allocated
/// block of this span (double free is caller UB in release builds).
pub unsafe fn dealloc_to_local_span(span: *mut SpanHeader, block: *mut Block) {
    // SAFETY: caller is the owner; the local list is owner-private.
    unsafe {
        (*span).local.free.push(block);
        let q = (*span).local.free_count.load(Ordering::Relaxed);
        (*span).local.free_count.store(q + 1, Ordering::Relaxed);
    }
}

/// `(q, g, m)` snapshot for tests/diagnostics.
///
/// # Safety
/// `span` must point to an initialized span header.
pub unsafe fn span_counts(span: *mut SpanHeader) -> (usize, isize, usize) {
    // SAFETY: per contract, valid initialized header; loads are atomic.
    unsafe {
        (
            (*span).local.free_count.load(Ordering::Relaxed),
            (*span).remote.free_count.load(Ordering::Relaxed),
            (*span).block_count.load(Ordering::Relaxed),
        )
    }
}
