//! Optional `GlobalAlloc` wrapper (feature = "global").
//!
//! Added only on top of the working token-based core, per the roadmap.
//! Unsupported layouts, unregistered threads (beyond the configured active
//! thread count), and exhaustion
//! all return null.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;

use crate::allocator::WfSpanAllocator;
use crate::thread::ThreadToken;

/// [`GlobalAlloc`] wrapper around [`WfSpanAllocator`] with automatic
/// thread registration via thread-local storage.
///
/// Thread tokens are allocated on first use per thread, so callers do not
/// manage [`crate::ThreadToken`]s directly. Requires the `global` feature.
///
/// # Examples
///
/// ```no_run
/// // Requires `features = ["global"]`.
/// use wf_alloc::global::GlobalWfSpanAllocator;
///
/// // 128 SPAN_SIZE-aligned spans as backing memory.
/// #[repr(align(65536))]
/// struct AlignedRegion([u8; 128 * 65536]);
/// static mut REGION: AlignedRegion = AlignedRegion([0u8; 128 * 65536]);
///
/// fn setup() {
///     let alloc = GlobalWfSpanAllocator::<{ wf_alloc::MAX_SUPPORTED_CLASSES }>::new(8);
///     unsafe { alloc.init((&raw mut REGION.0).cast::<u8>(), 128 * 65536) };
/// }
/// ```
pub struct GlobalWfSpanAllocator<
    const C: usize = { crate::config::MAX_SUPPORTED_CLASSES },
    const HUGE_GRANULE_SPANS: usize = { crate::config::DEFAULT_HUGE_GRANULE_SPANS },
> {
    pub inner: WfSpanAllocator<C, HUGE_GRANULE_SPANS>,
}

std::thread_local! {
    /// Token id for this thread, or usize::MAX before registration.
    /// const-initialized Cell: no lazy init, hence no recursive allocation.
    static THREAD_TOKEN_ID: Cell<usize> = const { Cell::new(usize::MAX) };
}

impl<const C: usize, const HG: usize> GlobalWfSpanAllocator<C, HG> {
    /// Create a new, uninitialized allocator for `active_threads` threads.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // Requires `features = ["global"]`.
    /// use wf_alloc::global::GlobalWfSpanAllocator;
    ///
    /// let alloc = GlobalWfSpanAllocator::<{ wf_alloc::MAX_SUPPORTED_CLASSES }>::new(4);
    /// ```
    pub fn new(active_threads: usize) -> Self {
        Self {
            inner: WfSpanAllocator::new(active_threads),
        }
    }

    /// See [`WfSpanAllocator::init`].
    ///
    /// # Safety
    /// Same contract as [`WfSpanAllocator::init`].
    pub unsafe fn init(&self, region: *mut u8, len: usize) {
        // SAFETY: forwarded contract.
        unsafe { self.inner.init(region, len) }
    }

    /// Token for the current thread, registering it on first use.
    /// None once `active_threads` threads are registered (or if TLS is gone).
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(all(feature = "std", feature = "global"))] {
    /// use wf_alloc::global::GlobalWfSpanAllocator;
    /// use wf_alloc::region::OwnedRegion;
    ///
    /// let region = OwnedRegion::new(16);
    /// let g = Box::leak(Box::new(GlobalWfSpanAllocator::<{ wf_alloc::MAX_SUPPORTED_CLASSES }>::new(4)));
    /// unsafe { g.init(region.ptr(), region.len()) };
    ///
    /// // First call registers this thread; subsequent calls return the cached token.
    /// let token = g.current_thread_token();
    /// assert!(token.is_some());
    /// let again = g.current_thread_token();
    /// assert_eq!(token.unwrap().id, again.unwrap().id);
    /// # }
    /// ```
    pub fn current_thread_token(&self) -> Option<ThreadToken> {
        THREAD_TOKEN_ID
            .try_with(|cell| {
                let id = cell.get();
                if id != usize::MAX {
                    // SAFETY: id was produced by this registry for this thread.
                    return Some(unsafe { self.inner.registry.token_from_raw(id) });
                }
                let token = self.inner.register_thread()?;
                cell.set(token.id);
                Some(token)
            })
            .ok()
            .flatten()
    }
}

// SAFETY: GlobalAlloc requires Sync; the inner allocator is Sync.
unsafe impl<const C: usize, const HG: usize> GlobalAlloc for GlobalWfSpanAllocator<C, HG> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match self.current_thread_token() {
            // SAFETY: token is valid for this thread; forwarded contract.
            Some(token) => unsafe { self.inner.alloc_with_token(layout, token) },
            None => core::ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if let Some(token) = self.current_thread_token() {
            // SAFETY: forwarded contract; remote frees are handled, so any
            // registered thread may free any pointer of this allocator.
            unsafe { self.inner.dealloc_with_token(ptr, layout, token) }
        }
        // A thread that can no longer register cannot free; with a bounded
        // active thread count this is documented as a leak, never UB.
    }
}
