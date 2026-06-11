//! Per-thread heap: private local span-lists plus public SPMC span-lists,
//! one of each per size class.

use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::span::SpanHeader;
use crate::spmc_span_list::SpmcSpanList;

/// Owner-only intrusive list of privately held spans (linked through
/// `span.local.next_local`). All accesses are Relaxed: only the owning
/// thread touches it; ownership handover is synchronized elsewhere.
pub struct LocalSpanList {
    head: AtomicPtr<SpanHeader>,
    tail: AtomicPtr<SpanHeader>,
    len: AtomicUsize,
}

impl LocalSpanList {
    pub const fn new() -> Self {
        Self {
            head: AtomicPtr::new(core::ptr::null_mut()),
            tail: AtomicPtr::new(core::ptr::null_mut()),
            len: AtomicUsize::new(0),
        }
    }

    pub fn front(&self) -> *mut SpanHeader {
        self.head.load(Ordering::Relaxed)
    }

    pub fn len(&self) -> usize {
        self.len.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// # Safety
    /// Owner-only. `span` must be owned by the caller and in no list.
    pub unsafe fn push_front(&self, span: *mut SpanHeader) {
        let old = self.head.load(Ordering::Relaxed);
        // SAFETY: owner-private link field of an owned span.
        unsafe { (*span).local.next_local.store(old, Ordering::Relaxed) };
        self.head.store(span, Ordering::Relaxed);
        if old.is_null() {
            self.tail.store(span, Ordering::Relaxed);
        }
        self.len.fetch_add(1, Ordering::Relaxed);
    }

    /// # Safety
    /// Owner-only. `span` must be owned by the caller and in no list.
    pub unsafe fn push_back(&self, span: *mut SpanHeader) {
        // SAFETY: owner-private link fields.
        unsafe {
            (*span)
                .local
                .next_local
                .store(core::ptr::null_mut(), Ordering::Relaxed);
            let tail = self.tail.load(Ordering::Relaxed);
            if tail.is_null() {
                self.head.store(span, Ordering::Relaxed);
            } else {
                (*tail).local.next_local.store(span, Ordering::Relaxed);
            }
        }
        self.tail.store(span, Ordering::Relaxed);
        self.len.fetch_add(1, Ordering::Relaxed);
    }

    /// # Safety
    /// Owner-only.
    pub unsafe fn pop_front(&self) -> *mut SpanHeader {
        let span = self.head.load(Ordering::Relaxed);
        if span.is_null() {
            return span;
        }
        // SAFETY: owner-private link field of the list head.
        let next = unsafe { (*span).local.next_local.load(Ordering::Relaxed) };
        self.head.store(next, Ordering::Relaxed);
        if next.is_null() {
            self.tail.store(core::ptr::null_mut(), Ordering::Relaxed);
        }
        self.len.fetch_sub(1, Ordering::Relaxed);
        span
    }

    /// Unlink `span` if found within the first `limit` entries (bounded
    /// scan). Returns whether it was removed.
    ///
    /// # Safety
    /// Owner-only.
    pub unsafe fn remove_bounded(&self, span: *mut SpanHeader, limit: usize) -> bool {
        let mut prev: *mut SpanHeader = core::ptr::null_mut();
        let mut cur = self.head.load(Ordering::Relaxed);
        // Bounded loop: at most `limit` iterations.
        for _ in 0..limit {
            if cur.is_null() {
                return false;
            }
            // SAFETY: owner-private link fields of listed spans.
            unsafe {
                let next = (*cur).local.next_local.load(Ordering::Relaxed);
                if cur == span {
                    if prev.is_null() {
                        self.head.store(next, Ordering::Relaxed);
                    } else {
                        (*prev).local.next_local.store(next, Ordering::Relaxed);
                    }
                    if next.is_null() {
                        self.tail.store(prev, Ordering::Relaxed);
                    }
                    self.len.fetch_sub(1, Ordering::Relaxed);
                    return true;
                }
                prev = cur;
                cur = next;
            }
        }
        false
    }
}

impl Default for LocalSpanList {
    fn default() -> Self {
        Self::new()
    }
}

/// One thread's heap: for each size class, a private local span-list, a
/// public SPMC span-list, and saved acquire positions.
pub struct ThreadHeap<const C: usize> {
    pub local_spans: [LocalSpanList; C],
    pub public_spans: [SpmcSpanList; C],
    pub cur_query: [AtomicUsize; C],
    pub helping_pos: [AtomicUsize; C],
}

impl<const C: usize> ThreadHeap<C> {
    pub const fn new() -> Self {
        Self {
            local_spans: [const { LocalSpanList::new() }; C],
            public_spans: [const { SpmcSpanList::new() }; C],
            cur_query: [const { AtomicUsize::new(0) }; C],
            helping_pos: [const { AtomicUsize::new(0) }; C],
        }
    }
}

impl<const C: usize> Default for ThreadHeap<C> {
    fn default() -> Self {
        Self::new()
    }
}
