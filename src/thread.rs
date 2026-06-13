//! Explicit thread tokens and bounded thread registration.
//!
//! The core API is token-based (no TLS) so it works in no_std / RTOS-style
//! environments; a TLS wrapper exists only behind the `global` feature.

use core::sync::atomic::{AtomicUsize, Ordering};

/// Identity of a registered thread; `id < registry max`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadToken {
    pub id: usize,
}

/// Bounded, wait-free thread registration: one FAA per registration.
pub struct ThreadRegistry {
    next: AtomicUsize,
    max: usize,
}

impl ThreadRegistry {
    pub const fn new(max: usize) -> Self {
        Self {
            next: AtomicUsize::new(0),
            max,
        }
    }

    /// Register the calling thread. Fails (None) after `max` registrations.
    ///
    /// # Examples
    ///
    /// ```
    /// use wf_alloc::ThreadRegistry;
    ///
    /// let reg = ThreadRegistry::new(2);
    /// let t0 = reg.register(); // first registration
    /// let t1 = reg.register(); // second registration
    /// let t2 = reg.register(); // exceeds max=2, returns None
    /// assert!(t0.is_some());
    /// assert!(t1.is_some());
    /// assert!(t2.is_none());
    /// ```
    pub fn register(&self) -> Option<ThreadToken> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        if id < self.max {
            Some(ThreadToken { id })
        } else {
            None
        }
    }

    pub fn registered(&self) -> usize {
        self.next.load(Ordering::Relaxed).min(self.max)
    }

    /// Build a token from an externally managed id (e.g. a CPU id on an
    /// RTOS where one heap is bound per core).
    ///
    /// # Safety
    /// `id` must be `< registry max` and must not be used by two threads concurrently.
    ///
    /// # Examples
    ///
    /// ```
    /// use wf_alloc::ThreadRegistry;
    ///
    /// // On an RTOS with 4 cores, CPU 0 always uses token 0.
    /// let reg = ThreadRegistry::new(4);
    /// let token = unsafe { reg.token_from_raw(0) };
    /// assert_eq!(token.id, 0);
    /// ```
    pub unsafe fn token_from_raw(&self, id: usize) -> ThreadToken {
        debug_assert!(id < self.max);
        ThreadToken { id }
    }
}
