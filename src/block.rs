//! A block is the unit returned to the user. While free, the block's own
//! memory stores the free-list link; the payload is the block address itself.

use core::sync::atomic::AtomicPtr;

#[repr(C)]
pub struct Block {
    pub next: AtomicPtr<Block>,
}

/// Sentinel `next` value used by the MPSC remote free-list between the
/// producer's SWAP and the completing link store. This is an expected
/// intermediate state of the non-linearizable design, not corruption.
pub const UNLINKED: *mut Block = core::ptr::without_provenance_mut(usize::MAX);

/// Payload address handed to the user for a free block.
pub fn block_payload(block: *mut Block) -> *mut u8 {
    block.cast()
}

/// Recover the block from a user payload pointer.
pub fn block_from_payload(ptr: *mut u8) -> *mut Block {
    ptr.cast()
}
