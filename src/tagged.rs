//! Versioned head word for ABA-safe SPMC pops.
//!
//! The pointer and version form ONE logical value and must always be
//! compared-and-exchanged together (CAS2 / 128-bit CAS). Never split them
//! into two independent atomics.

#[repr(C, align(16))]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct HeadWord {
    pub ptr: usize,
    pub version: usize,
}

impl HeadWord {
    pub const ZERO: HeadWord = HeadWord { ptr: 0, version: 0 };

    pub const fn new(ptr: usize, version: usize) -> Self {
        Self { ptr, version }
    }
}

const _: () = assert!(core::mem::size_of::<HeadWord>() == 16);
const _: () = assert!(core::mem::align_of::<HeadWord>() == 16);
