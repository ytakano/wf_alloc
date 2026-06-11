//! Owner-only local free-list of blocks inside one span.
//!
//! Only the span owner touches this list, so all atomic accesses are
//! `Relaxed`; cross-thread ownership transfer is synchronized by the
//! release/acquire edges of the SPMC span-list, the help-record CAS, or the
//! owner-claim CAS, all of which order these relaxed writes.

use core::sync::atomic::{AtomicPtr, Ordering};

use crate::block::Block;

pub struct LocalFreeList {
    head: AtomicPtr<Block>,
}

impl LocalFreeList {
    pub const fn new() -> Self {
        Self {
            head: AtomicPtr::new(core::ptr::null_mut()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.head.load(Ordering::Relaxed).is_null()
    }

    /// Push a free block. O(1), no loop.
    ///
    /// # Safety
    /// Caller must be the span owner. `block` must point to a valid block of
    /// this span and must not be in any other free-list.
    pub unsafe fn push(&self, block: *mut Block) {
        let old = self.head.load(Ordering::Relaxed);
        // SAFETY: `block` is a valid block owned by the caller (owner thread).
        unsafe { (*block).next.store(old, Ordering::Relaxed) };
        self.head.store(block, Ordering::Relaxed);
    }

    /// Read the head without popping (diagnostics/verifier only).
    pub fn peek_for_verify(&self) -> *mut Block {
        self.head.load(Ordering::Relaxed)
    }

    /// Install an already-linked chain as the whole list during span
    /// initialization. O(1).
    ///
    /// # Safety
    /// The list must be empty and not yet visible to other threads; `head`
    /// must be a well-linked chain of `count` blocks ending in null.
    pub unsafe fn push_chain_head_for_init(&self, head: *mut Block, count: usize) {
        debug_assert!(self.is_empty());
        let _ = count;
        self.head.store(head, Ordering::Relaxed);
    }

    /// Pop a free block, or null. O(1), no loop.
    ///
    /// # Safety
    /// Caller must be the span owner.
    pub unsafe fn pop(&self) -> *mut Block {
        let block = self.head.load(Ordering::Relaxed);
        if block.is_null() {
            return block;
        }
        // SAFETY: non-null head of an owner-private list is a valid block.
        let next = unsafe { (*block).next.load(Ordering::Relaxed) };
        self.head.store(next, Ordering::Relaxed);
        block
    }
}

impl Default for LocalFreeList {
    fn default() -> Self {
        Self::new()
    }
}
