//! CAS2 backend: atomically load / compare-exchange a 16-byte `HeadWord`.
//!
//! Backend assumptions (see docs/wfspan-model.md):
//! - x86_64: `lock cmpxchg16b` emulates strong LL/SC via pointer+version.
//! - Miri: a plain, NON-ATOMIC fallback used only for sequential tests.
//! - Other targets are currently unsupported and fail to compile clearly.

use crate::tagged::HeadWord;

pub trait Cas2Backend {
    /// Atomically load the head word.
    ///
    /// # Safety
    /// `head` must be valid for atomic access and 16-byte aligned.
    unsafe fn load(head: *const HeadWord) -> HeadWord;

    /// One compare-exchange attempt (never retries internally).
    /// `Ok(previous)` on success, `Err(actual)` on failure.
    ///
    /// # Safety
    /// `head` must be valid for atomic access and 16-byte aligned.
    unsafe fn compare_exchange(
        head: *mut HeadWord,
        current: HeadWord,
        new: HeadWord,
    ) -> Result<HeadWord, HeadWord>;
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
mod x86 {
    use super::*;

    /// `lock cmpxchg16b` backend. Requires the cmpxchg16b CPU feature,
    /// present on all x86_64 CPUs of the last ~15 years.
    pub struct Cmpxchg16b;

    /// One `lock cmpxchg16b`: returns `(previous_value, success)`.
    ///
    /// # Safety
    /// `dst` must be valid for read/write and 16-byte aligned.
    #[inline]
    unsafe fn cmpxchg16b(
        dst: *mut HeadWord,
        current: HeadWord,
        new: HeadWord,
    ) -> (HeadWord, bool) {
        let mut lo = current.ptr as u64;
        let mut hi = current.version as u64;
        let ok: u8;
        // SAFETY: single `lock cmpxchg16b` on a 16-byte-aligned location
        // (caller contract). rbx is reserved by LLVM, so it is swapped with
        // a scratch register around the instruction.
        unsafe {
            core::arch::asm!(
                "xchg {nb}, rbx",
                "lock cmpxchg16b [{dst}]",
                "sete {ok}",
                "mov rbx, {nb}",
                dst = in(reg) dst,
                nb = inout(reg) new.ptr as u64 => _,
                in("rcx") new.version as u64,
                inout("rax") lo,
                inout("rdx") hi,
                ok = out(reg_byte) ok,
                options(nostack),
            );
        }
        (
            HeadWord {
                ptr: lo as usize,
                version: hi as usize,
            },
            ok != 0,
        )
    }

    impl Cas2Backend for Cmpxchg16b {
        unsafe fn load(head: *const HeadWord) -> HeadWord {
            // A cmpxchg16b with an arbitrary expected value performs an
            // atomic 16-byte load: on mismatch it returns the actual value;
            // on (unlikely) match it rewrites the identical value.
            // SAFETY: forwarded caller contract.
            unsafe { cmpxchg16b(head as *mut HeadWord, HeadWord::ZERO, HeadWord::ZERO).0 }
        }

        unsafe fn compare_exchange(
            head: *mut HeadWord,
            current: HeadWord,
            new: HeadWord,
        ) -> Result<HeadWord, HeadWord> {
            // SAFETY: forwarded caller contract.
            let (prev, ok) = unsafe { cmpxchg16b(head, current, new) };
            if ok { Ok(prev) } else { Err(prev) }
        }
    }
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
pub use x86::Cmpxchg16b;

#[cfg(miri)]
mod miri_backend {
    use super::*;

    /// Plain (non-atomic) fallback so sequential tests run under Miri,
    /// where inline asm is unavailable. NOT safe for concurrent use.
    pub struct PlainCas2;

    impl Cas2Backend for PlainCas2 {
        unsafe fn load(head: *const HeadWord) -> HeadWord {
            // SAFETY: sequential-only per module contract.
            unsafe { core::ptr::read(head) }
        }

        unsafe fn compare_exchange(
            head: *mut HeadWord,
            current: HeadWord,
            new: HeadWord,
        ) -> Result<HeadWord, HeadWord> {
            // SAFETY: sequential-only per module contract.
            unsafe {
                let actual = core::ptr::read(head);
                if actual == current {
                    core::ptr::write(head, new);
                    Ok(actual)
                } else {
                    Err(actual)
                }
            }
        }
    }
}

#[cfg(miri)]
pub use miri_backend::PlainCas2;

#[cfg(all(target_arch = "x86_64", not(miri)))]
pub type DefaultCas2Backend = Cmpxchg16b;

#[cfg(miri)]
pub type DefaultCas2Backend = PlainCas2;

#[cfg(all(not(target_arch = "x86_64"), not(miri)))]
compile_error!(
    "wf_alloc currently provides a CAS2 backend only for x86_64 \
     (lock cmpxchg16b). Port `Cas2Backend` for this target first."
);

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::UnsafeCell;

    #[repr(align(16))]
    struct Slot(UnsafeCell<HeadWord>);

    #[test]
    fn cas2_success_failure_and_version() {
        let slot = Slot(UnsafeCell::new(HeadWord::new(0x10, 7)));
        let p = slot.0.get();
        // SAFETY: exclusive, aligned slot.
        unsafe {
            assert_eq!(DefaultCas2Backend::load(p), HeadWord::new(0x10, 7));
            // success
            let r = DefaultCas2Backend::compare_exchange(
                p,
                HeadWord::new(0x10, 7),
                HeadWord::new(0x20, 8),
            );
            assert_eq!(r, Ok(HeadWord::new(0x10, 7)));
            // failure with stale expected (ABA guard: same ptr, old version)
            let r = DefaultCas2Backend::compare_exchange(
                p,
                HeadWord::new(0x10, 7),
                HeadWord::new(0x30, 9),
            );
            assert_eq!(r, Err(HeadWord::new(0x20, 8)));
            assert_eq!(DefaultCas2Backend::load(p), HeadWord::new(0x20, 8));
        }
    }
}
