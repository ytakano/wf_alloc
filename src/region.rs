//! std-only helper: owned, SPAN_ALIGN-aligned backing region for tests and
//! benches. Not part of the allocator core (the core never touches the OS).

use std::alloc::{Layout, alloc_zeroed, dealloc};

use crate::config::{SPAN_ALIGN, SPAN_SIZE};

pub struct OwnedRegion {
    ptr: *mut u8,
    layout: Layout,
}

impl OwnedRegion {
    /// Allocate a region holding exactly `spans` spans.
    pub fn new(spans: usize) -> Self {
        let layout = Layout::from_size_align(spans * SPAN_SIZE, SPAN_ALIGN).unwrap();
        // SAFETY: layout has non-zero size for spans >= 1.
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "failed to allocate test region");
        Self { ptr, layout }
    }

    pub fn ptr(&self) -> *mut u8 {
        self.ptr
    }

    pub fn len(&self) -> usize {
        self.layout.size()
    }

    pub fn is_empty(&self) -> bool {
        self.layout.size() == 0
    }
}

impl Drop for OwnedRegion {
    fn drop(&mut self) {
        // SAFETY: ptr/layout from alloc_zeroed above.
        unsafe { dealloc(self.ptr, self.layout) };
    }
}

// SAFETY: plain memory region; ownership semantics are the user's contract.
unsafe impl Send for OwnedRegion {}
unsafe impl Sync for OwnedRegion {}
