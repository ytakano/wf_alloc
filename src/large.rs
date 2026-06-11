//! Large-object allocator: power-of-two size classes from MIN_LARGE_SIZE to MAX_LARGE_SIZE.
//!
//! Allocations that exceed the span-based small-object allocator's MAX_BLOCK_SIZE
//! are served here.  A bump pointer provides fresh blocks (one CAS-loop per
//! allocation); freed blocks are recycled through a per-class Treiber stack
//! (CAS2 + version counter for ABA safety, reusing the existing HeadWord
//! infrastructure).  This path is lock-free, not wait-free; large allocations
//! are assumed infrequent.
//!
//! ## Memory layout of each allocation
//!
//! ```text
//! alloc_base (alloc_size-aligned)
//!   ├─ [padding: back_offset - HEADER_SIZE bytes]   (only when align > HEADER_SIZE)
//!   ├─ LargeHeader { back_offset, alloc_size }       (HEADER_SIZE bytes)
//!   └─ payload (back_offset bytes from alloc_base)   ← returned to caller
//! ```
//!
//! While a block is on the free list, its first `usize` stores the next pointer.

use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::align::round_up;
use crate::atomic_backend::{Cas2Backend, DefaultCas2Backend};
use crate::config::{LARGE_CLASSES, MAX_LARGE_SIZE, MIN_LARGE_SIZE};
use crate::tagged::HeadWord;

/// Size of `LargeHeader` in bytes (two `usize` fields).
const HEADER_SIZE: usize = 16;
const _: () = assert!(core::mem::size_of::<LargeHeader>() == HEADER_SIZE);

/// Header placed immediately before the payload of every large allocation.
#[repr(C)]
pub struct LargeHeader {
    /// Distance in bytes from `alloc_base` to the payload pointer.
    pub back_offset: usize,
    /// Total allocated size (a power of two ≥ MIN_LARGE_SIZE).
    pub alloc_size: usize,
}

/// Per-size-class free list backed by a CAS2 Treiber stack (ABA-safe).
/// Head stores `(alloc_base_addr, version)`.
#[repr(C, align(16))]
struct LargeBin {
    head: UnsafeCell<HeadWord>,
}

// SAFETY: `head` is accessed only through `DefaultCas2Backend` (CAS2 / lock cmpxchg16b).
unsafe impl Send for LargeBin {}
unsafe impl Sync for LargeBin {}

impl LargeBin {
    const fn new() -> Self {
        Self {
            head: UnsafeCell::new(HeadWord::ZERO),
        }
    }

    /// Push `alloc_base` (a non-zero address) onto the stack.
    ///
    /// # Safety
    /// The first `usize`-sized word at `alloc_base` must be exclusively writable
    /// by this call (the block is not simultaneously visible to other threads).
    unsafe fn push(&self, alloc_base: usize) {
        let mut cur = unsafe { DefaultCas2Backend::load(self.head.get()) };
        loop {
            // Store next pointer in the free block's first word.
            unsafe { *(alloc_base as *mut usize) = cur.ptr };
            let new = HeadWord::new(alloc_base, cur.version.wrapping_add(1));
            match unsafe { DefaultCas2Backend::compare_exchange(self.head.get(), cur, new) } {
                Ok(_) => return,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Pop the top entry. Returns 0 if the stack is empty.
    ///
    /// # Safety
    /// The stack must contain only addresses of valid large allocations.
    unsafe fn pop(&self) -> usize {
        let mut cur = unsafe { DefaultCas2Backend::load(self.head.get()) };
        loop {
            if cur.ptr == 0 {
                return 0;
            }
            let next = unsafe { *(cur.ptr as *const usize) };
            let new = HeadWord::new(next, cur.version.wrapping_add(1));
            match unsafe { DefaultCas2Backend::compare_exchange(self.head.get(), cur, new) } {
                Ok(_) => return cur.ptr,
                Err(actual) => cur = actual,
            }
        }
    }
}

/// Bump-pointer + per-class Treiber-stack pool for large objects.
pub struct LargePool {
    base: AtomicUsize,
    bump: AtomicUsize,
    end: AtomicUsize,
    bins: [LargeBin; LARGE_CLASSES],
}

// SAFETY: all shared state is atomics or CAS2-protected UnsafeCell.
unsafe impl Send for LargePool {}
unsafe impl Sync for LargePool {}

impl LargePool {
    pub const fn new() -> Self {
        const EMPTY_BIN: LargeBin = LargeBin::new();
        Self {
            base: AtomicUsize::new(0),
            bump: AtomicUsize::new(0),
            end: AtomicUsize::new(0),
            bins: [EMPTY_BIN; LARGE_CLASSES],
        }
    }

    /// Install the backing memory region for large objects.
    ///
    /// # Safety
    /// `ptr..ptr+len` must be valid, writable, exclusively owned memory that
    /// outlives the pool.  Must be called at most once before any allocation.
    pub unsafe fn set_region(&self, ptr: *mut u8, len: usize) {
        let base = round_up(ptr as usize, HEADER_SIZE);
        let end = (ptr as usize).saturating_add(len);
        self.base.store(base, Ordering::Relaxed);
        self.end.store(end, Ordering::Relaxed);
        self.bump.store(base, Ordering::Release);
    }

    /// Base address of the installed region (0 if not yet installed).
    pub fn base_addr(&self) -> usize {
        self.base.load(Ordering::Relaxed)
    }

    /// One-past-end address of the installed region (0 if not yet installed).
    pub fn end_addr(&self) -> usize {
        self.end.load(Ordering::Relaxed)
    }

    /// Bump the internal pointer forward by `alloc_size` bytes, aligning the
    /// result to `alloc_size`.  Returns the aligned base address, or 0 on
    /// exhaustion.  Lock-free (CAS retry).
    fn bump_aligned(&self, alloc_size: usize) -> usize {
        let end = self.end.load(Ordering::Relaxed);
        let mut cur = self.bump.load(Ordering::Relaxed);
        loop {
            let aligned = round_up(cur, alloc_size);
            let next = match aligned.checked_add(alloc_size) {
                Some(n) => n,
                None => return 0,
            };
            if next > end {
                return 0;
            }
            match self.bump.compare_exchange_weak(
                cur,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return aligned,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Allocate a large object.  Returns null on unsupported layout or exhaustion.
    ///
    /// The returned pointer is aligned to `layout.align()` (or HEADER_SIZE,
    /// whichever is larger).
    ///
    /// # Safety
    /// `set_region` must have been called before the first allocation.
    pub unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        // Guarantee the header fits and is naturally aligned before the payload.
        let align = layout.align().max(HEADER_SIZE);

        // back_offset: distance from alloc_base to payload.
        // Smallest multiple of `align` that is >= HEADER_SIZE.
        let back_offset = round_up(HEADER_SIZE, align);

        let needed = match back_offset.checked_add(size) {
            Some(n) => n,
            None => return core::ptr::null_mut(),
        };
        if needed > MAX_LARGE_SIZE {
            return core::ptr::null_mut();
        }

        let class = match large_size_class(needed) {
            Some(c) => c,
            None => return core::ptr::null_mut(),
        };
        let alloc_size = MIN_LARGE_SIZE << class;

        // Prefer a recycled block; fall back to fresh bump allocation.
        //
        // SAFETY: the stack only holds addresses of live large allocations.
        let alloc_base = unsafe { self.bins[class].pop() };
        let alloc_base = if alloc_base != 0 {
            alloc_base
        } else {
            let b = self.bump_aligned(alloc_size);
            if b == 0 {
                return core::ptr::null_mut();
            }
            b
        };

        // `alloc_base` is alloc_size-aligned; since alloc_size >= align,
        // `alloc_base + back_offset` is align-aligned.
        let payload_ptr = alloc_base + back_offset;
        let header = (payload_ptr - HEADER_SIZE) as *mut LargeHeader;
        // SAFETY: `payload_ptr - HEADER_SIZE` lies within the allocation.
        unsafe {
            (*header).back_offset = back_offset;
            (*header).alloc_size = alloc_size;
        }

        payload_ptr as *mut u8
    }

    /// Deallocate a large object returned by `alloc` on this pool.
    ///
    /// # Safety
    /// `ptr` must have been returned by `alloc` on this pool and not yet freed.
    pub unsafe fn dealloc(&self, ptr: *mut u8) {
        let payload_addr = ptr as usize;
        let header = (payload_addr - HEADER_SIZE) as *const LargeHeader;
        // SAFETY: header was written by `alloc`; the block is exclusively owned.
        let back_offset = unsafe { (*header).back_offset };
        let alloc_size = unsafe { (*header).alloc_size };
        let alloc_base = payload_addr - back_offset;
        let class = large_size_class(alloc_size)
            .expect("corrupted LargeHeader: invalid alloc_size");
        // SAFETY: `alloc_base` is the start of a live large allocation; first
        // word is writable and not concurrently observed by other threads.
        unsafe { self.bins[class].push(alloc_base) };
    }
}

impl Default for LargePool {
    fn default() -> Self {
        Self::new()
    }
}

/// Map a byte count to a large-object size class index.
///
/// Returns the index of the smallest large class whose capacity is ≥ `needed`,
/// or `None` if `needed > MAX_LARGE_SIZE` or the value overflows.
pub fn large_size_class(needed: usize) -> Option<usize> {
    if needed > MAX_LARGE_SIZE {
        return None;
    }
    let size = needed.max(MIN_LARGE_SIZE).checked_next_power_of_two()?;
    if size > MAX_LARGE_SIZE {
        return None;
    }
    let class = size.trailing_zeros() as usize - MIN_LARGE_SIZE.trailing_zeros() as usize;
    if class >= LARGE_CLASSES { None } else { Some(class) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MAX_BLOCK_SIZE;

    #[test]
    fn size_class_boundaries() {
        // Anything <= MAX_BLOCK_SIZE maps to class 0 (MIN_LARGE_SIZE is the floor).
        assert_eq!(large_size_class(1), Some(0));
        assert_eq!(large_size_class(MAX_BLOCK_SIZE), Some(0));
        assert_eq!(large_size_class(MIN_LARGE_SIZE), Some(0));
        // One byte over MIN_LARGE_SIZE needs class 1.
        assert_eq!(large_size_class(MIN_LARGE_SIZE + 1), Some(1));
        // Exact power-of-two boundaries.
        assert_eq!(large_size_class(MIN_LARGE_SIZE * 2), Some(1));
        assert_eq!(large_size_class(MIN_LARGE_SIZE * 2 + 1), Some(2));
        // Top of the range.
        assert_eq!(large_size_class(MAX_LARGE_SIZE), Some(LARGE_CLASSES - 1));
        // Over the limit.
        assert_eq!(large_size_class(MAX_LARGE_SIZE + 1), None);
        assert_eq!(large_size_class(usize::MAX), None);
    }

    #[cfg(feature = "std")]
    #[test]
    fn alloc_dealloc_roundtrip() {
        use std::alloc::Layout;

        // 4 spans = 4 × 64 KiB = 256 KiB; enough for multiple class-0 blocks.
        let backing = crate::region::OwnedRegion::new(4);
        let pool = LargePool::new();
        unsafe { pool.set_region(backing.ptr(), backing.len()) };

        let layout = Layout::from_size_align(MIN_LARGE_SIZE / 2, 16).unwrap();
        let ptr = unsafe { pool.alloc(layout) };
        assert!(!ptr.is_null());
        assert_eq!(ptr as usize % 16, 0);

        // Write + read back to catch trivial corruption.
        unsafe { ptr.write(0xAB) };
        assert_eq!(unsafe { ptr.read() }, 0xAB);

        unsafe { pool.dealloc(ptr) };

        // After freeing, the next alloc of the same class should reuse the block.
        let ptr2 = unsafe { pool.alloc(layout) };
        assert_eq!(ptr, ptr2, "recycled block should be reused");
        unsafe { pool.dealloc(ptr2) };
    }
}
