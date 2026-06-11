//! Wait-free, non-linearizable MPSC remote free-list (one per span).
//!
//! Producers (remote deallocators) publish with a single SWAP and then link
//! the old head. Between those two steps the node's `next` is `UNLINKED`;
//! the consumer (span owner) stops at such a link and retries later. This is
//! an expected state of the design, never corruption — do not spin on it.

use core::sync::atomic::{AtomicPtr, Ordering};

use crate::block::{Block, UNLINKED};
use crate::span::SpanHeader;
use crate::stats::StepCounter;

pub struct RemoteMpscFreeList {
    pub head: AtomicPtr<Block>,
}

impl RemoteMpscFreeList {
    pub const fn new() -> Self {
        Self {
            head: AtomicPtr::new(core::ptr::null_mut()),
        }
    }

    /// First half of `push`: mark `block` UNLINKED and SWAP it in as the new
    /// head. Returns the old head, which the caller must pass to
    /// [`push_link`] to complete the operation. Split out so tests can model
    /// a producer halted between the two steps. O(1), no loop.
    ///
    /// # Safety
    /// `block` must be an allocated block of the span owning this list and
    /// must not be in any free-list.
    pub unsafe fn push_publish(&self, block: *mut Block) -> *mut Block {
        // SAFETY: `block` is exclusively held by the caller until the SWAP.
        unsafe { (*block).next.store(UNLINKED, Ordering::Relaxed) };
        self.head.swap(block, Ordering::AcqRel)
    }

    /// Second half of `push`: link the published block to the old head.
    ///
    /// # Safety
    /// `block` must be the block passed to `push_publish` and `old_head` its
    /// return value; called exactly once per publish.
    pub unsafe fn push_link(block: *mut Block, old_head: *mut Block) {
        // SAFETY: only this producer writes `block.next` until the link
        // resolves; consumers read it with Acquire and stop at UNLINKED.
        unsafe { (*block).next.store(old_head, Ordering::Release) };
    }

    /// Wait-free remote push: SWAP then link. O(1), no loop.
    ///
    /// # Safety
    /// See [`push_publish`].
    pub unsafe fn push(&self, block: *mut Block) {
        // SAFETY: forwarded contract.
        unsafe {
            let old = self.push_publish(block);
            Self::push_link(block, old);
        }
    }

    /// Detach the entire list. O(1), no loop. Owner-only.
    pub fn reclaim_all(&self) -> *mut Block {
        self.head.swap(core::ptr::null_mut(), Ordering::AcqRel)
    }
}

impl Default for RemoteMpscFreeList {
    fn default() -> Self {
        Self::new()
    }
}

/// Move a reclaimed remote chain into the owner's local free-list.
///
/// Bounded by `span.block_count`. Returns `(appended, leftover)`: `leftover`
/// is the suffix starting at the first node whose `next` is still UNLINKED
/// (a stalled producer); the caller must stash it in
/// `span.local.pending_remote` and retry later — never drop it.
///
/// # Safety
/// Caller must own `span`; `head` must be a chain detached from this span's
/// remote list (or a previously stashed pending chain).
pub unsafe fn append_remote_to_local_bounded(
    span: *mut SpanHeader,
    head: *mut Block,
    step: &mut StepCounter,
) -> (usize, *mut Block) {
    let mut appended = 0usize;
    let mut cur = head;
    // SAFETY: chain nodes are blocks of `span`; owner-private local list.
    unsafe {
        let limit = (*span).block_count.load(Ordering::Relaxed);
        // Bounded loop: at most blocks_per_span iterations.
        for _ in 0..limit {
            if cur.is_null() {
                break;
            }
            step.blocks_scanned += 1;
            let next = (*cur).next.load(Ordering::Acquire);
            if next == UNLINKED {
                // Producer stalled between SWAP and link: stop, keep suffix.
                return (appended, cur);
            }
            (*span).local.free.push(cur);
            let q = (*span).local.free_count.load(Ordering::Relaxed);
            (*span).local.free_count.store(q + 1, Ordering::Relaxed);
            appended += 1;
            cur = next;
        }
    }
    (appended, cur)
}
