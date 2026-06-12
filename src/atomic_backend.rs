//! CAS2 backend: atomically load / compare-exchange a 16-byte `HeadWord`.
//!
//! Backend assumptions (see docs/wfspan-model.md):
//! - x86_64: `lock cmpxchg16b` emulates strong LL/SC via pointer+version.
//! - aarch64 with FEAT_LSE (`target-feature=+lse`): `caspal`, a strong CAS
//!   with the same failure semantics as cmpxchg16b.
//! - aarch64 baseline: ONE `ldaxp`/`stlxp` exclusive pair per attempt.
//!   A single attempt may fail spuriously; callers already route every
//!   failure into the bounded helping protocol, so step bounds hold, but
//!   "failure implies another thread progressed" becomes best-effort.
//! - Miri: a plain, NON-ATOMIC fallback used only for sequential tests.
//! - Other targets are currently unsupported and fail to compile clearly.

use crate::tagged::HeadWord;

pub trait Cas2Backend {
    /// Atomically load the head word, with at least `Acquire` ordering.
    ///
    /// On the LL/SC backend the 16-byte pair may tear (each 64-bit half is
    /// individually atomic, the pair is not). Callers tolerate this: a
    /// loaded head word is either validated by the subsequent versioned
    /// CAS before anything depends on the pair being consistent, or it is
    /// read quiescently (verifier).
    ///
    /// # Safety
    /// `head` must be valid for atomic access and 16-byte aligned.
    unsafe fn load(head: *const HeadWord) -> HeadWord;

    /// One compare-exchange attempt (never retries internally), with at
    /// least `AcqRel` ordering on success and `Acquire` on failure.
    /// `Ok(previous)` on success, `Err(actual)` on failure.
    ///
    /// Failure may be SPURIOUS on the LL/SC backend: `Err(actual)` can
    /// equal `current`, and the failure payload may be torn. Callers must
    /// treat `Err` as "this attempt made no progress" — never as proof
    /// that another thread succeeded — and must not act on the payload
    /// without re-validating it.
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

#[cfg(all(target_arch = "aarch64", not(miri)))]
mod aarch64 {
    use super::*;

    /// One-shot LL/SC backend (`ldaxp`/`stlxp`, ARMv8.0 baseline).
    ///
    /// Exactly ONE exclusive pair per `compare_exchange` call — never an
    /// internal retry loop — so the crate's per-operation step accounting
    /// counts hardware attempts one-to-one. The price is weak-CAS
    /// semantics: `stlxp` can fail spuriously (interrupt, cache-line
    /// migration, unrelated store into the exclusive reservation granule),
    /// so `Err(actual)` may equal `current`.
    pub struct LdxpStxp;

    /// One `ldaxp`/`stlxp` attempt: returns `(loaded_value, success)`.
    /// On mismatch the reservation is dropped with `clrex` and the loaded
    /// (possibly torn) pair is returned with `success == false`.
    ///
    /// # Safety
    /// `dst` must be valid for read/write and 16-byte aligned (required by
    /// `ldaxp`/`stlxp`; guaranteed by `HeadWord`'s `repr(align(16))`).
    #[inline]
    unsafe fn cas2_once(dst: *mut HeadWord, current: HeadWord, new: HeadWord) -> (HeadWord, bool) {
        let olo: u64;
        let ohi: u64;
        let status: u64;
        // SAFETY: one exclusive pair on a 16-byte-aligned location (caller
        // contract). `ldaxp` gives Acquire, `stlxp` gives Release, matching
        // the trait's ordering contract. Outputs are plain `out` (not
        // `lateout`): `ldaxp` writes them before the inputs are consumed.
        unsafe {
            core::arch::asm!(
                "ldaxp {olo}, {ohi}, [{dst}]",
                "cmp {olo}, {clo}",
                "ccmp {ohi}, {chi}, #0, eq",
                "b.ne 2f",
                "stlxp {status:w}, {nlo}, {nhi}, [{dst}]",
                "b 3f",
                "2:",
                "clrex",
                "mov {status:w}, #1",
                "3:",
                dst = in(reg) dst,
                clo = in(reg) current.ptr as u64,
                chi = in(reg) current.version as u64,
                nlo = in(reg) new.ptr as u64,
                nhi = in(reg) new.version as u64,
                olo = out(reg) olo,
                ohi = out(reg) ohi,
                status = out(reg) status,
                options(nostack),
            );
        }
        (
            HeadWord {
                ptr: olo as usize,
                version: ohi as usize,
            },
            status == 0,
        )
    }

    impl Cas2Backend for LdxpStxp {
        unsafe fn load(head: *const HeadWord) -> HeadWord {
            let lo: u64;
            let hi: u64;
            // SAFETY: aligned atomic location (caller contract). A bare
            // `ldaxp` (reservation dropped with `clrex`) loads each half
            // atomically with Acquire ordering; the PAIR may tear, which
            // the trait contract on `load` explicitly permits.
            unsafe {
                core::arch::asm!(
                    "ldaxp {lo}, {hi}, [{src}]",
                    "clrex",
                    src = in(reg) head,
                    lo = out(reg) lo,
                    hi = out(reg) hi,
                    options(nostack),
                );
            }
            HeadWord {
                ptr: lo as usize,
                version: hi as usize,
            }
        }

        unsafe fn compare_exchange(
            head: *mut HeadWord,
            current: HeadWord,
            new: HeadWord,
        ) -> Result<HeadWord, HeadWord> {
            // SAFETY: forwarded caller contract.
            let (prev, ok) = unsafe { cas2_once(head, current, new) };
            if ok { Ok(prev) } else { Err(prev) }
        }
    }

    /// FEAT_LSE backend (`caspal`, ARMv8.1+): a single-instruction strong
    /// CAS with the same failure semantics as x86 `cmpxchg16b` (failure
    /// proves the value differed; no spurious failures, no torn results).
    #[cfg(target_feature = "lse")]
    pub struct CaspLse;

    /// One `caspal`: returns `(previous_value, success)`.
    ///
    /// # Safety
    /// `dst` must be valid for read/write and 16-byte aligned.
    #[cfg(target_feature = "lse")]
    #[inline]
    unsafe fn caspal(dst: *mut HeadWord, current: HeadWord, new: HeadWord) -> (HeadWord, bool) {
        let mut lo = current.ptr as u64;
        let mut hi = current.version as u64;
        // SAFETY: single `caspal` (acquire+release) on a 16-byte-aligned
        // location (caller contract). `caspal` requires consecutive
        // even/odd register pairs, so x4..x7 are pinned explicitly.
        unsafe {
            core::arch::asm!(
                "caspal x4, x5, x6, x7, [{dst}]",
                dst = in(reg) dst,
                inout("x4") lo,
                inout("x5") hi,
                in("x6") new.ptr as u64,
                in("x7") new.version as u64,
                options(nostack),
            );
        }
        let prev = HeadWord {
            ptr: lo as usize,
            version: hi as usize,
        };
        (prev, prev == current)
    }

    #[cfg(target_feature = "lse")]
    impl Cas2Backend for CaspLse {
        unsafe fn load(head: *const HeadWord) -> HeadWord {
            // Same trick as the x86 backend: a CAS with an arbitrary
            // expected value performs an atomic 16-byte load (on the
            // unlikely match it rewrites the identical value).
            // SAFETY: forwarded caller contract.
            unsafe { caspal(head as *mut HeadWord, HeadWord::ZERO, HeadWord::ZERO).0 }
        }

        unsafe fn compare_exchange(
            head: *mut HeadWord,
            current: HeadWord,
            new: HeadWord,
        ) -> Result<HeadWord, HeadWord> {
            // SAFETY: forwarded caller contract.
            let (prev, ok) = unsafe { caspal(head, current, new) };
            if ok { Ok(prev) } else { Err(prev) }
        }
    }
}

#[cfg(all(target_arch = "aarch64", not(miri)))]
pub use aarch64::LdxpStxp;

#[cfg(all(target_arch = "aarch64", target_feature = "lse", not(miri)))]
pub use aarch64::CaspLse;

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

#[cfg(all(target_arch = "aarch64", target_feature = "lse", not(miri)))]
pub type DefaultCas2Backend = CaspLse;

#[cfg(all(target_arch = "aarch64", not(target_feature = "lse"), not(miri)))]
pub type DefaultCas2Backend = LdxpStxp;

#[cfg(miri)]
pub type DefaultCas2Backend = PlainCas2;

#[cfg(all(not(target_arch = "x86_64"), not(target_arch = "aarch64"), not(miri)))]
compile_error!(
    "wf_alloc currently provides CAS2 backends only for x86_64 \
     (lock cmpxchg16b) and aarch64 (caspal / ldaxp+stlxp). \
     Port `Cas2Backend` for this target first."
);

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::UnsafeCell;

    #[repr(align(16))]
    struct Slot(UnsafeCell<HeadWord>);

    /// CAS wrapper that retries a bounded number of times when the failure
    /// is spurious (`Err(actual) == current`), so the weak LL/SC backend
    /// can be checked deterministically in this sequential harness.
    ///
    /// # Safety
    /// As for [`Cas2Backend::compare_exchange`].
    unsafe fn cas_no_spurious<B: Cas2Backend>(
        p: *mut HeadWord,
        current: HeadWord,
        new: HeadWord,
    ) -> Result<HeadWord, HeadWord> {
        for _ in 0..64 {
            // SAFETY: forwarded caller contract.
            match unsafe { B::compare_exchange(p, current, new) } {
                Err(actual) if actual == current => continue, // spurious
                r => return r,
            }
        }
        panic!("compare_exchange failed spuriously 64 times in a row");
    }

    fn check_backend<B: Cas2Backend>() {
        let slot = Slot(UnsafeCell::new(HeadWord::new(0x10, 7)));
        let p = slot.0.get();
        // SAFETY: exclusive, aligned slot.
        unsafe {
            assert_eq!(B::load(p), HeadWord::new(0x10, 7));
            // success
            let r = cas_no_spurious::<B>(p, HeadWord::new(0x10, 7), HeadWord::new(0x20, 8));
            assert_eq!(r, Ok(HeadWord::new(0x10, 7)));
            // failure with stale expected (ABA guard: same ptr, old version)
            let r = cas_no_spurious::<B>(p, HeadWord::new(0x10, 7), HeadWord::new(0x30, 9));
            assert_eq!(r, Err(HeadWord::new(0x20, 8)));
            assert_eq!(B::load(p), HeadWord::new(0x20, 8));
        }
    }

    #[test]
    fn cas2_success_failure_and_version() {
        check_backend::<DefaultCas2Backend>();
    }

    #[cfg(all(target_arch = "aarch64", not(miri)))]
    #[test]
    fn cas2_llsc_backend() {
        check_backend::<LdxpStxp>();
    }

    #[cfg(all(target_arch = "aarch64", target_feature = "lse", not(miri)))]
    #[test]
    fn cas2_casp_backend() {
        check_backend::<CaspLse>();
    }
}
