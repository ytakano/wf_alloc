//! Optional hosted `GlobalAlloc` wrapper (feature = "global").
//!
//! The public wrapper is [`HostedLazyGlobalWfSpanAllocator`], a hosted/std
//! bootstrap wrapper that can be used as `#[global_allocator] static`. It
//! lazily allocates wf_alloc metadata and the backing span region with
//! `std::alloc::System`, then routes subsequent allocations through
//! [`WfSpanAllocator`].

use core::alloc::{GlobalAlloc, Layout};
use core::cell::{Cell, UnsafeCell};
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicUsize, Ordering};

use std::alloc::System;

use crate::allocator::WfSpanAllocator;
use crate::config::SPAN_SIZE;
use crate::thread::ThreadToken;

const UNINIT: usize = 0;
const INITING: usize = 1;
const READY: usize = 2;
const FAILED: usize = 3;

std::thread_local! {
    /// Cached allocator identity for this thread's token.
    static THREAD_ALLOC_ID: Cell<usize> = const { Cell::new(0) };
    /// Token id for this thread, or usize::MAX before registration.
    /// const-initialized Cell: no lazy init, hence no recursive allocation.
    static THREAD_TOKEN_ID: Cell<usize> = const { Cell::new(usize::MAX) };
}

/// Hosted `#[global_allocator]` wrapper for [`WfSpanAllocator`].
///
/// This type is intended for std/hosted targets. It can be const-initialized
/// as a global allocator because it does not construct the inner allocator in
/// the static initializer. On first allocation it uses [`std::alloc::System`]
/// directly to allocate metadata and the backing span region, initializes
/// [`WfSpanAllocator`] with [`WfSpanAllocator::from_metadata_region`], and
/// then serves allocations from wf_alloc.
///
/// ```rust,ignore
/// use wf_alloc::global::HostedLazyGlobalWfSpanAllocator;
///
/// #[global_allocator]
/// static ALLOC: HostedLazyGlobalWfSpanAllocator =
///     HostedLazyGlobalWfSpanAllocator::new(8, 1024);
/// ```
pub struct HostedLazyGlobalWfSpanAllocator<
    const C: usize = { crate::config::MAX_SUPPORTED_CLASSES },
    const HUGE_GRANULE_SPANS: usize = { crate::config::DEFAULT_HUGE_GRANULE_SPANS },
> {
    state: AtomicUsize,
    active_threads: usize,
    region_spans: usize,
    inner: UnsafeCell<MaybeUninit<WfSpanAllocator<C, HUGE_GRANULE_SPANS>>>,
}

impl<const C: usize, const HG: usize> HostedLazyGlobalWfSpanAllocator<C, HG> {
    /// Create a hosted lazy global allocator.
    ///
    /// `active_threads` bounds automatic thread registration. `region_spans`
    /// is the size of the wf_alloc backing region in 64 KiB spans.
    pub const fn new(active_threads: usize, region_spans: usize) -> Self {
        Self {
            state: AtomicUsize::new(UNINIT),
            active_threads,
            region_spans,
            inner: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// Return the initialized inner allocator, if lazy initialization has
    /// already completed successfully.
    pub fn inner(&self) -> Option<&WfSpanAllocator<C, HG>> {
        if self.state.load(Ordering::Acquire) == READY {
            // SAFETY: READY is stored only after `inner` is fully written and
            // initialized, and the static wrapper never moves it afterward.
            Some(unsafe { (&*self.inner.get()).assume_init_ref() })
        } else {
            None
        }
    }

    fn ensure_initialized(&self) -> Result<&WfSpanAllocator<C, HG>, ()> {
        loop {
            match self.state.load(Ordering::Acquire) {
                READY => {
                    // SAFETY: READY is stored only after full initialization.
                    return Ok(unsafe { (&*self.inner.get()).assume_init_ref() });
                }
                FAILED => return Err(()),
                INITING => {
                    core::hint::spin_loop();
                }
                UNINIT => {
                    if self
                        .state
                        .compare_exchange(UNINIT, INITING, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        // SAFETY: this thread won initialization and no other
                        // thread can read `inner` until READY is published.
                        let ok = unsafe { self.initialize_once() };
                        self.state
                            .store(if ok { READY } else { FAILED }, Ordering::Release);
                    }
                }
                _ => return Err(()),
            }
        }
    }

    unsafe fn initialize_once(&self) -> bool {
        if self.active_threads == 0 || self.region_spans == 0 {
            return false;
        }

        let Some(metadata_size) =
            WfSpanAllocator::<C, HG>::metadata_region_size(self.active_threads)
        else {
            return false;
        };
        let Ok(metadata_layout) = Layout::from_size_align(
            metadata_size,
            WfSpanAllocator::<C, HG>::metadata_region_align(),
        ) else {
            return false;
        };

        let Some(region_len) = self.region_spans.checked_mul(SPAN_SIZE) else {
            return false;
        };
        let Ok(region_layout) = Layout::from_size_align(region_len, SPAN_SIZE) else {
            return false;
        };

        // SAFETY: direct calls to System avoid recursing into this global allocator.
        let metadata = unsafe { System.alloc(metadata_layout) };
        if metadata.is_null() {
            return false;
        }
        let region = unsafe { System.alloc(region_layout) };
        if region.is_null() {
            // SAFETY: metadata was allocated above with this layout and is not exposed.
            unsafe { System.dealloc(metadata, metadata_layout) };
            return false;
        }

        let Some((alloc, _metadata_used)) = (unsafe {
            WfSpanAllocator::<C, HG>::from_metadata_region(
                self.active_threads,
                metadata,
                metadata_size,
            )
        }) else {
            unsafe { System.dealloc(region, region_layout) };
            unsafe { System.dealloc(metadata, metadata_layout) };
            return false;
        };

        // SAFETY: the pool region was allocated with SPAN_SIZE alignment and
        // remains owned by this global allocator for the process lifetime.
        unsafe { alloc.init(region, region_len) };

        // SAFETY: this thread exclusively initializes `inner` while state is INITING.
        unsafe { (*self.inner.get()).write(alloc) };
        true
    }

    fn current_thread_token(&self, inner: &WfSpanAllocator<C, HG>) -> Option<ThreadToken> {
        let alloc_id = self as *const Self as usize;
        THREAD_ALLOC_ID
            .try_with(|alloc_cell| {
                THREAD_TOKEN_ID.with(|token_cell| {
                    let id = token_cell.get();
                    if alloc_cell.get() == alloc_id && id != usize::MAX {
                        // SAFETY: id was produced by this registry for this thread.
                        return Some(unsafe { inner.registry.token_from_raw(id) });
                    }
                    let token = inner.register_thread()?;
                    alloc_cell.set(alloc_id);
                    token_cell.set(token.id);
                    Some(token)
                })
            })
            .ok()
            .flatten()
    }
}

// SAFETY: all mutation is synchronized by atomics during initialization, and
// the initialized inner allocator is Sync. `inner` is written once before READY.
unsafe impl<const C: usize, const HG: usize> Sync for HostedLazyGlobalWfSpanAllocator<C, HG> {}

unsafe impl<const C: usize, const HG: usize> GlobalAlloc
    for HostedLazyGlobalWfSpanAllocator<C, HG>
{
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let Ok(inner) = self.ensure_initialized() else {
            return core::ptr::null_mut();
        };
        match self.current_thread_token(inner) {
            // SAFETY: token is valid for this thread; forwarded contract.
            Some(token) => unsafe { inner.alloc_with_token(layout, token) },
            None => core::ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ptr.is_null() {
            return;
        }
        let Ok(inner) = self.ensure_initialized() else {
            return;
        };
        if let Some(token) = self.current_thread_token(inner) {
            // SAFETY: forwarded contract; remote frees are handled, so any
            // registered thread may free any pointer of this allocator.
            unsafe { inner.dealloc_with_token(ptr, layout, token) }
        }
        // A thread that can no longer register cannot free; with a bounded
        // active thread count this is documented as a leak, never UB.
    }
}
