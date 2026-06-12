//! Public SPMC wait-free span-list (one per thread heap per size class).
//!
//! MS-queue-shaped with a dummy node: the single producer (heap owner)
//! appends at the tail with a release store — no CAS. Consumers pop at the
//! head with EXACTLY ONE versioned CAS2 attempt (`try_pop_head_once`);
//! a failed attempt means another thread made progress (or, on the LL/SC
//! backend, the attempt failed spuriously); either way the caller proceeds
//! on its bounded budget via the helping protocol, never by retrying here.
//!
//! Node ownership: every span carries one `SpanNode`. Enqueue consumes the
//! span's node as the new tail; a successful pop hands the outgoing dummy
//! node to the popped span. Node count is conserved (spans + one dummy per
//! list); nodes are never freed.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicPtr, Ordering};

use crate::atomic_backend::Cas2Backend;
use crate::span::SpanHeader;
use crate::stats::StepCounter;
use crate::tagged::HeadWord;

#[repr(C)]
pub struct SpanNode {
    pub next: AtomicPtr<SpanNode>,
    pub span: AtomicPtr<SpanHeader>,
}

impl SpanNode {
    pub const fn new() -> Self {
        Self {
            next: AtomicPtr::new(core::ptr::null_mut()),
            span: AtomicPtr::new(core::ptr::null_mut()),
        }
    }
}

impl Default for SpanNode {
    fn default() -> Self {
        Self::new()
    }
}

/// Outcome of a single pop attempt. The three cases are deliberately
/// distinct: `Failed` (lost a race, or a spurious LL/SC failure) must
/// route the caller into the helping protocol, not into a retry loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryPop {
    Span(*mut SpanHeader),
    Empty,
    Failed,
}

pub struct SpmcSpanList {
    /// Pointer + version updated atomically (CAS2) by consumers.
    head: UnsafeCell<HeadWord>,
    /// Producer-only tail pointer.
    tail: UnsafeCell<*mut SpanNode>,
    /// Embedded initial dummy node (makes the struct address-sensitive:
    /// the list must not move after `init`).
    dummy: SpanNode,
}

// SAFETY: all cross-thread access goes through the CAS2 backend (head) or
// release/acquire atomics (node links); `tail` is producer-only by contract.
unsafe impl Send for SpmcSpanList {}
unsafe impl Sync for SpmcSpanList {}

impl SpmcSpanList {
    pub const fn new() -> Self {
        Self {
            head: UnsafeCell::new(HeadWord::ZERO),
            tail: UnsafeCell::new(core::ptr::null_mut()),
            dummy: SpanNode::new(),
        }
    }

    /// Point head and tail at the embedded dummy node.
    ///
    /// # Safety
    /// Must be called exactly once, before the list is shared, and the list
    /// must not move afterwards (self-referential dummy pointer).
    pub unsafe fn init(&self) {
        let dummy = &self.dummy as *const SpanNode as *mut SpanNode;
        // SAFETY: pre-share exclusive access per contract.
        unsafe {
            *self.head.get() = HeadWord::new(dummy as usize, 0);
            *self.tail.get() = dummy;
        }
    }

    pub fn head_ptr(&self) -> *mut HeadWord {
        self.head.get()
    }

    /// Owner-only enqueue: no CAS, one release store publishes the node.
    /// O(1), no loop.
    ///
    /// # Safety
    /// Caller must be the unique producer of this list. `span` must hold a
    /// node (every initialized span does) and must not be in any list.
    pub unsafe fn enqueue_by_owner(&self, span: *mut SpanHeader, _step: &mut StepCounter) {
        // SAFETY: producer-only fields; span/node ownership per contract.
        unsafe {
            let node = (*span).node.swap(core::ptr::null_mut(), Ordering::Relaxed);
            debug_assert!(!node.is_null(), "span enqueued without a node");
            (*node).span.store(span, Ordering::Relaxed);
            (*node).next.store(core::ptr::null_mut(), Ordering::Relaxed);

            let tail = *self.tail.get();
            // Release store publishes node contents and all prior span
            // metadata writes to consumers (who acquire-load `next`).
            (*tail).next.store(node, Ordering::Release);
            *self.tail.get() = node;
        }
    }

    /// One-shot pop: performs AT MOST ONE CAS2 attempt; never retries.
    ///
    /// # Safety
    /// The list must have been `init`ed and be reachable (not moved).
    pub unsafe fn try_pop_head_once<B: Cas2Backend>(&self, step: &mut StepCounter) -> TryPop {
        // SAFETY: head is a valid, aligned HeadWord; loads are atomic.
        let old = unsafe { B::load(self.head.get()) };
        let head_node = old.ptr as *mut SpanNode;
        if head_node.is_null() {
            // List not initialized yet (only possible pre-init).
            return TryPop::Empty;
        }
        // SAFETY: nodes are never freed; head_node stays valid even if stale.
        let next = unsafe { (*head_node).next.load(Ordering::Acquire) };
        if next.is_null() {
            return TryPop::Empty;
        }
        let new = HeadWord::new(next as usize, old.version.wrapping_add(1));
        step.cas2_attempts += 1;
        // SAFETY: single CAS2 attempt on the valid head word.
        match unsafe { B::compare_exchange(self.head.get(), old, new) } {
            Ok(_) => {
                // SAFETY: we won the pop; `next` (the new dummy) carries the
                // span, and the outgoing dummy is handed to that span.
                unsafe {
                    let span = (*next).span.load(Ordering::Relaxed);
                    debug_assert!(!span.is_null());
                    (*span).node.store(head_node, Ordering::Relaxed);
                    TryPop::Span(span)
                }
            }
            Err(_) => TryPop::Failed,
        }
    }
}

impl Default for SpmcSpanList {
    fn default() -> Self {
        Self::new()
    }
}
