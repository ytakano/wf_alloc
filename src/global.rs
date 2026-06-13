//! Optional hosted `GlobalAlloc` wrapper (feature = "global").
//!
//! The public wrapper is [`HostedLazyGlobalWfSpanAllocator`], a hosted/std
//! bootstrap wrapper that can be used as `#[global_allocator] static`. It
//! keeps wf_alloc's fixed-participant core intact by creating dynamic shards:
//! each shard owns one [`WfSpanAllocator`] with a bounded token table and a
//! bounded span region. Threads borrow a token from a shard through TLS and
//! return it when the thread exits. If all existing shards are full or a shard
//! runs out of backing spans, the wrapper can add another shard with memory
//! obtained directly from [`std::alloc::System`]. Requests that wf_alloc cannot
//! represent fall back to `System`.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;
use core::mem::{align_of, size_of};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

use std::alloc::System;

use crate::allocator::WfSpanAllocator;
use crate::config::SPAN_SIZE;
use crate::thread::ThreadToken;

const SERVICE_TOKEN_ID: usize = 0;
const DEFAULT_THREADS_PER_SHARD: usize = 64;
const DEFAULT_REGION_SPANS: usize = 1024;
const HEADER_MAGIC: usize = 0x5746_474C_4F42_3100; // "WFGLOB1\0"
const BACKEND_WFSPAN: usize = 1;
const BACKEND_SYSTEM: usize = 2;

std::thread_local! {
    static THREAD_ALLOC_ID: Cell<usize> = const { Cell::new(0) };
    static THREAD_SHARD: Cell<usize> = const { Cell::new(0) };
    static THREAD_TOKEN_ID: Cell<usize> = const { Cell::new(usize::MAX) };
    static THREAD_TOKEN_SLOT: Cell<usize> = const { Cell::new(0) };
    static THREAD_GUARD: ThreadTokenGuard = const { ThreadTokenGuard };
}

struct ThreadTokenGuard;

impl Drop for ThreadTokenGuard {
    fn drop(&mut self) {
        let _ = THREAD_TOKEN_SLOT.try_with(|slot| {
            let slot_ptr = slot.replace(0) as *mut AtomicUsize;
            if !slot_ptr.is_null() {
                // SAFETY: the TLS binding stores the exact AtomicUsize cell
                // acquired for this thread's reusable token. Shards are leaked
                // for process lifetime, so the cell remains valid here.
                unsafe { (*slot_ptr).store(0, Ordering::Release) };
            }
        });
        let _ = THREAD_TOKEN_ID.try_with(|token| token.set(usize::MAX));
        let _ = THREAD_SHARD.try_with(|shard| shard.set(0));
        let _ = THREAD_ALLOC_ID.try_with(|alloc| alloc.set(0));
    }
}

#[repr(C)]
struct AllocationHeader {
    magic: usize,
    backend: usize,
    shard: usize,
    storage_size: usize,
    storage_align: usize,
    offset: usize,
}

const HEADER_SIZE: usize = size_of::<AllocationHeader>();
const HEADER_ALIGN: usize = align_of::<AllocationHeader>();

#[derive(Clone, Copy)]
struct HeaderInfo {
    backend: usize,
    shard: usize,
    storage: Layout,
    offset: usize,
}

fn validate_header(h: &AllocationHeader) -> Option<HeaderInfo> {
    debug_assert_eq!(
        h.magic, HEADER_MAGIC,
        "invalid global allocation header magic"
    );
    if h.magic != HEADER_MAGIC {
        return None;
    }

    debug_assert!(
        h.backend == BACKEND_WFSPAN || h.backend == BACKEND_SYSTEM,
        "invalid global allocation header backend"
    );
    if h.backend != BACKEND_WFSPAN && h.backend != BACKEND_SYSTEM {
        return None;
    }

    debug_assert!(
        h.offset >= HEADER_SIZE && h.offset <= h.storage_size,
        "invalid global allocation header offset"
    );
    if h.offset < HEADER_SIZE || h.offset > h.storage_size {
        return None;
    }

    let Ok(storage) = Layout::from_size_align(h.storage_size, h.storage_align) else {
        debug_assert!(false, "invalid global allocation header layout");
        return None;
    };

    debug_assert!(
        h.backend != BACKEND_WFSPAN || h.shard != 0,
        "wfspan allocation header has null shard"
    );
    if h.backend == BACKEND_WFSPAN && h.shard == 0 {
        return None;
    }

    Some(HeaderInfo {
        backend: h.backend,
        shard: h.shard,
        storage,
        offset: h.offset,
    })
}

struct AllocationLayout {
    storage: Layout,
    offset: usize,
}

fn allocation_layout(layout: Layout) -> Option<AllocationLayout> {
    let align = layout.align().max(HEADER_ALIGN);
    let offset = align_up(HEADER_SIZE, align)?;
    let size = offset.checked_add(layout.size())?;
    let storage = Layout::from_size_align(size, align).ok()?;
    Some(AllocationLayout { storage, offset })
}

struct SpinLock(AtomicBool);

impl SpinLock {
    const fn new() -> Self {
        Self(AtomicBool::new(false))
    }

    fn lock(&self) -> SpinLockGuard<'_> {
        while self
            .0
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        SpinLockGuard(self)
    }
}

struct SpinLockGuard<'a>(&'a SpinLock);

impl Drop for SpinLockGuard<'_> {
    fn drop(&mut self) {
        self.0.0.store(false, Ordering::Release);
    }
}

struct HostedShard<
    const C: usize = { crate::config::MAX_SUPPORTED_CLASSES },
    const HUGE_GRANULE_SPANS: usize = { crate::config::DEFAULT_HUGE_GRANULE_SPANS },
> {
    next: AtomicPtr<HostedShard<C, HUGE_GRANULE_SPANS>>,
    user_slots: usize,
    token_in_use: *mut AtomicUsize,
    service_lock: SpinLock,
    region_spans: usize,
    alloc: WfSpanAllocator<C, HUGE_GRANULE_SPANS>,
}

/// Hosted global allocator configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlobalAllocatorConfig {
    /// Reusable user thread tokens per wfspan shard.
    pub threads_per_shard: usize,
    /// Backing memory per wfspan shard in 64 KiB spans.
    pub region_spans: usize,
}

impl GlobalAllocatorConfig {
    /// Conservative default for hosted experiments.
    pub const DEFAULT: Self = Self {
        threads_per_shard: DEFAULT_THREADS_PER_SHARD,
        region_spans: DEFAULT_REGION_SPANS,
    };

    /// Create a config from explicit shard parameters.
    pub const fn new(threads_per_shard: usize, region_spans: usize) -> Self {
        Self {
            threads_per_shard,
            region_spans,
        }
    }
}

impl Default for GlobalAllocatorConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Point-in-time diagnostics for [`HostedLazyGlobalWfSpanAllocator`].
///
/// These counters describe the hosted wrapper, not only the wfspan core. In
/// particular, System fallback and service-token frees are outside the
/// wait-free wfspan operation bounds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GlobalAllocatorStats {
    /// Number of wfspan shards created so far.
    pub shard_count: usize,
    /// Number of user tokens currently borrowed across all shards.
    pub user_tokens_in_use: usize,
    /// Failed attempts to bind the current thread to a shard token.
    pub token_acquisition_failures: usize,
    /// Failed shard creation attempts.
    pub shard_creation_failures: usize,
    /// Successful allocations served by wfspan.
    pub wfspan_allocations: usize,
    /// Allocation attempts that wfspan could not serve after retrying growth.
    pub wfspan_allocation_failures: usize,
    /// Successful allocations served by `std::alloc::System`.
    pub system_allocations: usize,
    /// Deallocations returned to `std::alloc::System`.
    pub system_deallocations: usize,
    /// Wfspan deallocations that used a shard's reserved service token.
    pub service_token_deallocations: usize,
    /// Largest wfspan shard backing region created so far, in 64 KiB spans.
    pub largest_shard_spans: usize,
}

impl<const C: usize, const HG: usize> HostedShard<C, HG> {
    unsafe fn create(user_slots: usize, region_spans: usize) -> *mut Self {
        if user_slots == 0 || region_spans == 0 {
            return ptr::null_mut();
        }
        let Some(active_threads) = user_slots.checked_add(1) else {
            return ptr::null_mut();
        };
        let Some(metadata_size) = WfSpanAllocator::<C, HG>::metadata_region_size(active_threads)
        else {
            return ptr::null_mut();
        };
        let Ok(metadata_layout) = Layout::from_size_align(
            metadata_size,
            WfSpanAllocator::<C, HG>::metadata_region_align(),
        ) else {
            return ptr::null_mut();
        };
        let Some(region_len) = region_spans.checked_mul(SPAN_SIZE) else {
            return ptr::null_mut();
        };
        let Ok(region_layout) = Layout::from_size_align(region_len, SPAN_SIZE) else {
            return ptr::null_mut();
        };
        let Some(token_size) = size_of::<AtomicUsize>().checked_mul(user_slots) else {
            return ptr::null_mut();
        };
        let Ok(token_layout) = Layout::from_size_align(token_size, align_of::<AtomicUsize>())
        else {
            return ptr::null_mut();
        };
        let shard_layout = Layout::new::<Self>();

        // SAFETY: direct System calls avoid recursing into the global allocator.
        let metadata = unsafe { System.alloc(metadata_layout) };
        if metadata.is_null() {
            return ptr::null_mut();
        }
        let region = unsafe { System.alloc(region_layout) };
        if region.is_null() {
            unsafe { System.dealloc(metadata, metadata_layout) };
            return ptr::null_mut();
        }
        let token_mem = unsafe { System.alloc(token_layout) } as *mut AtomicUsize;
        if token_mem.is_null() {
            unsafe { System.dealloc(region, region_layout) };
            unsafe { System.dealloc(metadata, metadata_layout) };
            return ptr::null_mut();
        }
        let shard_mem = unsafe { System.alloc(shard_layout) } as *mut Self;
        if shard_mem.is_null() {
            unsafe { System.dealloc(token_mem.cast(), token_layout) };
            unsafe { System.dealloc(region, region_layout) };
            unsafe { System.dealloc(metadata, metadata_layout) };
            return ptr::null_mut();
        }

        for i in 0..user_slots {
            // SAFETY: token_mem points to user_slots AtomicUsize cells.
            unsafe { token_mem.add(i).write(AtomicUsize::new(0)) };
        }

        let Some((alloc, _metadata_used)) = (unsafe {
            WfSpanAllocator::<C, HG>::from_metadata_region(active_threads, metadata, metadata_size)
        }) else {
            unsafe { System.dealloc(shard_mem.cast(), shard_layout) };
            unsafe { System.dealloc(token_mem.cast(), token_layout) };
            unsafe { System.dealloc(region, region_layout) };
            unsafe { System.dealloc(metadata, metadata_layout) };
            return ptr::null_mut();
        };

        // SAFETY: the region is SPAN_SIZE-aligned and owned by this shard for
        // the process lifetime.
        unsafe { alloc.init(region, region_len) };

        // SAFETY: shard_mem is properly aligned and uninitialized storage.
        unsafe {
            shard_mem.write(Self {
                next: AtomicPtr::new(ptr::null_mut()),
                user_slots,
                token_in_use: token_mem,
                service_lock: SpinLock::new(),
                region_spans,
                alloc,
            })
        };
        shard_mem
    }

    fn acquire_user_token(&self) -> Option<(usize, *mut AtomicUsize)> {
        for i in 0..self.user_slots {
            // SAFETY: token_in_use points to user_slots initialized cells.
            let slot_ptr = unsafe { self.token_in_use.add(i) };
            let slot = unsafe { &*slot_ptr };
            if slot
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some((i + 1, slot_ptr));
            }
        }
        None
    }

    unsafe fn release_user_token(&self, token_id: usize) {
        if token_id == 0 || token_id > self.user_slots {
            return;
        }
        // SAFETY: token_id was returned by acquire_user_token for this shard.
        let slot = unsafe { &*self.token_in_use.add(token_id - 1) };
        slot.store(0, Ordering::Release);
    }

    fn user_tokens_in_use(&self) -> usize {
        let mut n = 0;
        for i in 0..self.user_slots {
            // SAFETY: token_in_use points to user_slots initialized cells.
            let slot = unsafe { &*self.token_in_use.add(i) };
            n += usize::from(slot.load(Ordering::Acquire) != 0);
        }
        n
    }
}

/// Hosted `#[global_allocator]` wrapper for [`WfSpanAllocator`].
///
/// This type is intended for std/hosted targets. It can be const-initialized
/// as a global allocator because it uses [`std::alloc::System`] directly to
/// bootstrap wf_alloc shards on demand. Each shard has one reserved service
/// token plus `threads_per_shard` reusable user tokens. A thread keeps its
/// token in TLS and returns it when the thread exits, so thread counts can
/// grow and shrink over the process lifetime. If all live shards are full, a
/// new shard is created.
///
/// Allocations are first attempted through the calling thread's wfspan shard.
/// If wfspan cannot serve the adjusted request, the wrapper creates another
/// shard and retries; requests too large for wfspan, or requests that still
/// cannot be served because the host is out of memory, fall back to
/// [`std::alloc::System`]. A small hidden header records which backend owns
/// each allocation so deallocation is safe from any thread.
///
/// ```rust,ignore
/// use wf_alloc::global::HostedLazyGlobalWfSpanAllocator;
///
/// #[global_allocator]
/// static ALLOC: HostedLazyGlobalWfSpanAllocator =
///     HostedLazyGlobalWfSpanAllocator::new(64, 1024);
/// ```
pub struct HostedLazyGlobalWfSpanAllocator<
    const C: usize = { crate::config::MAX_SUPPORTED_CLASSES },
    const HUGE_GRANULE_SPANS: usize = { crate::config::DEFAULT_HUGE_GRANULE_SPANS },
> {
    head: AtomicPtr<HostedShard<C, HUGE_GRANULE_SPANS>>,
    create_lock: SpinLock,
    threads_per_shard: usize,
    region_spans: usize,
    shard_count: AtomicUsize,
    token_acquisition_failures: AtomicUsize,
    shard_creation_failures: AtomicUsize,
    wfspan_allocations: AtomicUsize,
    wfspan_allocation_failures: AtomicUsize,
    system_allocations: AtomicUsize,
    system_deallocations: AtomicUsize,
    service_token_deallocations: AtomicUsize,
}

impl<const C: usize, const HG: usize> HostedLazyGlobalWfSpanAllocator<C, HG> {
    /// Create a hosted lazy global allocator.
    ///
    /// `threads_per_shard` is the number of reusable user thread tokens per
    /// shard. `region_spans` is the backing region size for each wfspan shard
    /// in 64 KiB spans. Pass non-zero values; zero falls back to conservative
    /// defaults so a `#[global_allocator] static` cannot be accidentally inert.
    ///
    /// For example, `HostedLazyGlobalWfSpanAllocator::new(16, 1024)` creates
    /// wfspan shards with 16 reusable user-thread tokens each. Each shard also
    /// reserves one internal service token, so the underlying `WfSpanAllocator`
    /// in that shard is initialized with 17 tokens. The second argument means
    /// each shard starts with `1024 * 64 KiB = 64 MiB` of wfspan backing memory.
    /// wf_alloc records this region but does not touch the whole 64 MiB during
    /// initialization; physical memory is usually committed as pages are later
    /// touched by allocations on demand-paged hosted systems. This is not a
    /// portable guarantee: virtual address space is still reserved, and actual
    /// physical commitment or allocation failure behavior depends on the OS,
    /// overcommit policy, resource limits, and the `std::alloc::System` backend.
    ///
    /// If more than `threads_per_shard` threads need tokens at the same time,
    /// or if a request needs a larger wfspan region, the wrapper lazily creates
    /// another shard. Requests that cannot be served by wfspan fall back to
    /// `std::alloc::System`.
    pub const fn new(threads_per_shard: usize, region_spans: usize) -> Self {
        Self::with_config(GlobalAllocatorConfig::new(threads_per_shard, region_spans))
    }

    /// Create a hosted lazy global allocator from a named configuration.
    pub const fn with_config(config: GlobalAllocatorConfig) -> Self {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
            create_lock: SpinLock::new(),
            threads_per_shard: config.threads_per_shard,
            region_spans: config.region_spans,
            shard_count: AtomicUsize::new(0),
            token_acquisition_failures: AtomicUsize::new(0),
            shard_creation_failures: AtomicUsize::new(0),
            wfspan_allocations: AtomicUsize::new(0),
            wfspan_allocation_failures: AtomicUsize::new(0),
            system_allocations: AtomicUsize::new(0),
            system_deallocations: AtomicUsize::new(0),
            service_token_deallocations: AtomicUsize::new(0),
        }
    }

    /// Create a hosted lazy global allocator with conservative defaults.
    pub const fn default_hosted() -> Self {
        Self::with_config(GlobalAllocatorConfig::DEFAULT)
    }

    /// Number of wfspan shards created so far.
    pub fn shard_count(&self) -> usize {
        self.shard_count.load(Ordering::Acquire)
    }

    /// Number of currently borrowed user tokens across all shards.
    pub fn user_tokens_in_use(&self) -> usize {
        let mut n = 0;
        let mut cur = self.head.load(Ordering::Acquire);
        while !cur.is_null() {
            // SAFETY: shards are process-lifetime allocations linked from head.
            unsafe {
                n += (*cur).user_tokens_in_use();
                cur = (*cur).next.load(Ordering::Acquire);
            }
        }
        n
    }

    /// Number of allocations that used the System fallback. This is a
    /// diagnostic counter, not a live allocation count.
    pub fn system_allocations(&self) -> usize {
        self.system_allocations.load(Ordering::Acquire)
    }

    fn largest_shard_spans(&self) -> usize {
        let mut largest = 0;
        let mut cur = self.head.load(Ordering::Acquire);
        while !cur.is_null() {
            // SAFETY: shards are process-lifetime allocations linked from head.
            unsafe {
                largest = largest.max((*cur).region_spans);
                cur = (*cur).next.load(Ordering::Acquire);
            }
        }
        largest
    }

    /// Return a point-in-time diagnostics snapshot for the hosted wrapper.
    pub fn stats(&self) -> GlobalAllocatorStats {
        GlobalAllocatorStats {
            shard_count: self.shard_count(),
            user_tokens_in_use: self.user_tokens_in_use(),
            token_acquisition_failures: self.token_acquisition_failures.load(Ordering::Acquire),
            shard_creation_failures: self.shard_creation_failures.load(Ordering::Acquire),
            wfspan_allocations: self.wfspan_allocations.load(Ordering::Acquire),
            wfspan_allocation_failures: self.wfspan_allocation_failures.load(Ordering::Acquire),
            system_allocations: self.system_allocations.load(Ordering::Acquire),
            system_deallocations: self.system_deallocations.load(Ordering::Acquire),
            service_token_deallocations: self.service_token_deallocations.load(Ordering::Acquire),
            largest_shard_spans: self.largest_shard_spans(),
        }
    }

    /// Return the first initialized shard's inner allocator, if any. This is
    /// mainly for diagnostics and tests; global allocations may use later
    /// shards too.
    pub fn inner(&self) -> Option<&WfSpanAllocator<C, HG>> {
        let head = self.head.load(Ordering::Acquire);
        if head.is_null() {
            None
        } else {
            // SAFETY: shards are leaked for process lifetime.
            Some(unsafe { &(*head).alloc })
        }
    }

    fn threads_per_shard(&self) -> usize {
        if self.threads_per_shard == 0 {
            DEFAULT_THREADS_PER_SHARD
        } else {
            self.threads_per_shard
        }
    }

    fn region_spans(&self) -> usize {
        if self.region_spans == 0 {
            DEFAULT_REGION_SPANS
        } else {
            self.region_spans
        }
    }

    fn bind_thread(&self) -> Option<(*mut HostedShard<C, HG>, ThreadToken)> {
        let alloc_id = self as *const Self as usize;
        let existing = THREAD_ALLOC_ID
            .try_with(|alloc_cell| {
                THREAD_SHARD.with(|shard_cell| {
                    THREAD_TOKEN_ID.with(|token_cell| {
                        let shard = shard_cell.get() as *mut HostedShard<C, HG>;
                        let token_id = token_cell.get();
                        if alloc_cell.get() == alloc_id
                            && !shard.is_null()
                            && token_id != usize::MAX
                        {
                            return Some((shard, ThreadToken { id: token_id }));
                        }
                        None
                    })
                })
            })
            .ok()
            .flatten();
        if existing.is_some() {
            return existing;
        }

        // During TLS destruction this guard may already be unavailable. In
        // that phase we cannot safely install a reusable wfspan token, so the
        // caller will fall back to System allocation.
        if THREAD_GUARD.try_with(|_| {}).is_err() {
            return None;
        }

        let (shard, token_id, token_slot) = self.acquire_token_from_any_shard()?;
        THREAD_ALLOC_ID.with(|alloc_cell| alloc_cell.set(alloc_id));
        THREAD_SHARD.with(|shard_cell| shard_cell.set(shard as usize));
        THREAD_TOKEN_ID.with(|token_cell| token_cell.set(token_id));
        THREAD_TOKEN_SLOT.with(|slot_cell| slot_cell.set(token_slot as usize));
        Some((shard, ThreadToken { id: token_id }))
    }

    fn acquire_token_from_any_shard(
        &self,
    ) -> Option<(*mut HostedShard<C, HG>, usize, *mut AtomicUsize)> {
        loop {
            let mut cur = self.head.load(Ordering::Acquire);
            while !cur.is_null() {
                // SAFETY: shards are process-lifetime allocations linked from head.
                unsafe {
                    if let Some((token, slot)) = (*cur).acquire_user_token() {
                        return Some((cur, token, slot));
                    }
                    cur = (*cur).next.load(Ordering::Acquire);
                }
            }

            let Some(new_shard) = self.create_and_link_shard(self.region_spans()) else {
                self.token_acquisition_failures
                    .fetch_add(1, Ordering::Relaxed);
                return None;
            };
            // SAFETY: just-created shard has all user tokens free.
            if let Some((token, slot)) = unsafe { (*new_shard).acquire_user_token() } {
                return Some((new_shard, token, slot));
            }
        }
    }

    fn create_and_link_shard(&self, min_region_spans: usize) -> Option<*mut HostedShard<C, HG>> {
        let _guard = self.create_lock.lock();

        let region_spans = self.region_spans().max(min_region_spans);
        // SAFETY: create uses System allocation and returns a fully initialized
        // shard or null.
        let shard = unsafe { HostedShard::<C, HG>::create(self.threads_per_shard(), region_spans) };
        if shard.is_null() {
            self.shard_creation_failures.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        let old_head = self.head.load(Ordering::Acquire);
        // SAFETY: shard is exclusively owned until published.
        unsafe { (*shard).next.store(old_head, Ordering::Relaxed) };
        self.head.store(shard, Ordering::Release);
        self.shard_count.fetch_add(1, Ordering::AcqRel);
        Some(shard)
    }

    unsafe fn alloc_from_wfspan(&self, storage: Layout) -> *mut u8 {
        let Some((mut shard, mut token)) = self.bind_thread() else {
            self.token_acquisition_failures
                .fetch_add(1, Ordering::Relaxed);
            return ptr::null_mut();
        };

        // Try the currently bound shard first.
        let mut raw = unsafe { (*shard).alloc.alloc_with_token(storage, token) };
        if !raw.is_null() {
            self.wfspan_allocations.fetch_add(1, Ordering::Relaxed);
            return raw;
        }

        let needed_spans = storage.size().div_ceil(SPAN_SIZE).max(1);
        if let Some(new_shard) = self.create_and_link_shard(needed_spans.max(self.region_spans())) {
            // Move this thread's reusable token to the new shard so future
            // allocations naturally use the expanded capacity.
            unsafe { (*shard).release_user_token(token.id) };
            if let Some((new_token_id, new_token_slot)) =
                unsafe { (*new_shard).acquire_user_token() }
            {
                THREAD_SHARD.with(|shard_cell| shard_cell.set(new_shard as usize));
                THREAD_TOKEN_ID.with(|token_cell| token_cell.set(new_token_id));
                THREAD_TOKEN_SLOT.with(|slot_cell| slot_cell.set(new_token_slot as usize));
                shard = new_shard;
                token = ThreadToken { id: new_token_id };
                raw = unsafe { (*shard).alloc.alloc_with_token(storage, token) };
                if !raw.is_null() {
                    self.wfspan_allocations.fetch_add(1, Ordering::Relaxed);
                    return raw;
                }
            }
        }

        self.wfspan_allocation_failures
            .fetch_add(1, Ordering::Relaxed);
        ptr::null_mut()
    }

    unsafe fn dealloc_to_wfspan(
        &self,
        shard: *mut HostedShard<C, HG>,
        raw: *mut u8,
        storage: Layout,
    ) {
        if shard.is_null() {
            return;
        }

        let current = THREAD_SHARD.try_with(|shard_cell| {
            THREAD_TOKEN_ID.with(|token_cell| {
                let current_shard = shard_cell.get() as *mut HostedShard<C, HG>;
                let token_id = token_cell.get();
                if current_shard == shard && token_id != usize::MAX {
                    Some(ThreadToken { id: token_id })
                } else {
                    None
                }
            })
        });

        if let Ok(Some(token)) = current {
            unsafe { (*shard).alloc.dealloc_with_token(raw, storage, token) };
            return;
        }

        // The freeing thread is not registered with the allocation's shard.
        // Serialize use of the reserved service token so dealloc remains safe
        // for arbitrary cross-thread frees without requiring unbounded token
        // registration.
        let _guard = unsafe { (*shard).service_lock.lock() };
        let token = ThreadToken {
            id: SERVICE_TOKEN_ID,
        };
        self.service_token_deallocations
            .fetch_add(1, Ordering::Relaxed);
        unsafe { (*shard).alloc.dealloc_with_token(raw, storage, token) };
    }

    unsafe fn alloc_system(&self, adjusted: AllocationLayout) -> *mut u8 {
        let raw = unsafe { System.alloc(adjusted.storage) };
        if raw.is_null() {
            return ptr::null_mut();
        }
        self.system_allocations.fetch_add(1, Ordering::Relaxed);
        unsafe { finish_allocation(raw, adjusted, BACKEND_SYSTEM, 0) }
    }
}

// SAFETY: shard publication is synchronized by atomics/locks; shards are never
// moved or freed while reachable, and WfSpanAllocator is Sync.
unsafe impl<const C: usize, const HG: usize> Sync for HostedLazyGlobalWfSpanAllocator<C, HG> {}

unsafe impl<const C: usize, const HG: usize> GlobalAlloc
    for HostedLazyGlobalWfSpanAllocator<C, HG>
{
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let Some(adjusted) = allocation_layout(layout) else {
            return ptr::null_mut();
        };

        let raw = unsafe { self.alloc_from_wfspan(adjusted.storage) };
        if !raw.is_null() {
            let shard = THREAD_SHARD.with(|shard_cell| shard_cell.get());
            return unsafe { finish_allocation(raw, adjusted, BACKEND_WFSPAN, shard) };
        }

        unsafe { self.alloc_system(adjusted) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { self.alloc(layout) };
        if !ptr.is_null() {
            // SAFETY: ptr was just allocated for layout.size() bytes by this
            // allocator and has not been exposed to user code yet.
            unsafe { ptr.write_bytes(0, layout.size()) };
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, old_layout: Layout, new_size: usize) -> *mut u8 {
        let Ok(new_layout) = Layout::from_size_align(new_size, old_layout.align()) else {
            return ptr::null_mut();
        };
        if new_size == 0 {
            if !ptr.is_null() {
                unsafe { self.dealloc(ptr, old_layout) };
            }
            return ptr::null_mut();
        }
        if ptr.is_null() {
            return unsafe { self.alloc(new_layout) };
        }

        let new_ptr = unsafe { self.alloc(new_layout) };
        if new_ptr.is_null() {
            return ptr::null_mut();
        }

        // SAFETY: both pointers are valid for their layouts, non-overlapping
        // because this implementation always allocates a fresh destination,
        // and the copied prefix is valid in both allocations.
        unsafe { ptr::copy_nonoverlapping(ptr, new_ptr, old_layout.size().min(new_size)) };
        unsafe { self.dealloc(ptr, old_layout) };
        new_ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        if ptr.is_null() {
            return;
        }
        let header = unsafe { ptr.sub(HEADER_SIZE) as *mut AllocationHeader };
        // SAFETY: every pointer returned by alloc has this header immediately
        // before the user pointer. GlobalAlloc callers must pass such a pointer.
        let h = unsafe { &*header };
        let Some(info) = validate_header(h) else {
            return;
        };
        let raw = unsafe { ptr.sub(info.offset) };
        match info.backend {
            BACKEND_WFSPAN => unsafe {
                self.dealloc_to_wfspan(info.shard as *mut HostedShard<C, HG>, raw, info.storage)
            },
            BACKEND_SYSTEM => unsafe {
                self.system_deallocations.fetch_add(1, Ordering::Relaxed);
                System.dealloc(raw, info.storage)
            },
            _ => unreachable!("validated global allocation backend"),
        }
    }
}

unsafe fn finish_allocation(
    raw: *mut u8,
    adjusted: AllocationLayout,
    backend: usize,
    shard: usize,
) -> *mut u8 {
    let user = unsafe { raw.add(adjusted.offset) };
    let header = unsafe { user.sub(HEADER_SIZE) as *mut AllocationHeader };
    // SAFETY: allocation_layout reserves HEADER_SIZE bytes immediately before
    // user, and raw has adjusted.storage size/alignment.
    unsafe {
        header.write(AllocationHeader {
            magic: HEADER_MAGIC,
            backend,
            shard,
            storage_size: adjusted.storage.size(),
            storage_align: adjusted.storage.align(),
            offset: adjusted.offset,
        });
    }
    user
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    debug_assert!(align.is_power_of_two());
    value.checked_add(align - 1).map(|v| v & !(align - 1))
}
