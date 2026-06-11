//! Segregated power-of-two size classes: 16, 32, 64, ..., 16384.

use crate::align::round_up;
use crate::config::{
    MAX_BLOCK_SIZE, MAX_SUPPORTED_CLASSES, MIN_BLOCK_SIZE, SPAN_HEADER_RESERVE, SPAN_SIZE,
};

/// Block size of `class` (class 0 = `MIN_BLOCK_SIZE`).
///
/// # Examples
///
/// ```
/// use wf_alloc::class_to_size;
///
/// assert_eq!(class_to_size(0), 16);  // class 0 → 16 bytes
/// assert_eq!(class_to_size(1), 32);  // class 1 → 32 bytes
/// assert_eq!(class_to_size(2), 64);  // class 2 → 64 bytes
/// ```
pub const fn class_to_size(class: usize) -> usize {
    MIN_BLOCK_SIZE << class
}

/// Map an allocation request to a size class.
///
/// Power-of-two classes give natural alignment up to the block size, so an
/// `align` larger than `size` is handled by sizing up to `align`. Returns
/// `None` for unsupported large requests (`> MAX_BLOCK_SIZE`).
///
/// # Examples
///
/// ```
/// use wf_alloc::{size_to_class, MIN_BLOCK_SIZE};
///
/// // Small size: rounds up to the minimum block size (class 0).
/// assert_eq!(size_to_class(1, 1), Some(0));
///
/// // Size 17 rounds up to 32 bytes → class 1.
/// assert_eq!(size_to_class(17, 1), Some(1));
///
/// // Alignment larger than size: the class is sized up to satisfy alignment.
/// assert_eq!(size_to_class(8, 64), Some(2)); // 64-byte class
///
/// // Requests larger than MAX_BLOCK_SIZE are unsupported.
/// assert_eq!(size_to_class(usize::MAX, 1), None);
/// ```
pub fn size_to_class(size: usize, align: usize) -> Option<usize> {
    if align > MAX_BLOCK_SIZE {
        return None;
    }
    let needed = size
        .max(align)
        .max(MIN_BLOCK_SIZE)
        .checked_next_power_of_two()?;
    if needed > MAX_BLOCK_SIZE {
        return None;
    }
    Some((needed.trailing_zeros() - MIN_BLOCK_SIZE.trailing_zeros()) as usize)
}

/// Offset of the first block in a span for the given block size.
///
/// Spans are `SPAN_SIZE`-aligned, so placing the first block at a multiple of
/// `block_size` makes every block naturally aligned to its (power-of-two) size.
pub const fn first_block_offset(block_size: usize) -> usize {
    round_up(SPAN_HEADER_RESERVE, block_size)
}

/// Number of blocks in a span for the given block size.
pub const fn blocks_per_span(block_size: usize) -> usize {
    (SPAN_SIZE - first_block_offset(block_size)) / block_size
}

const _: () = assert!(blocks_per_span(class_to_size(MAX_SUPPORTED_CLASSES - 1)) >= 2);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roadmap_acceptance() {
        // size 1, align 1 => 16-byte class
        assert_eq!(size_to_class(1, 1), Some(0));
        // size 17, align 1 => 32-byte class
        assert_eq!(size_to_class(17, 1), Some(1));
        // alignment greater than size is handled
        assert_eq!(size_to_class(8, 64), Some(2));
        assert_eq!(class_to_size(2), 64);
        // SPAN_SIZE or larger is unsupported
        assert_eq!(size_to_class(SPAN_SIZE, 1), None);
        assert_eq!(size_to_class(MAX_BLOCK_SIZE + 1, 1), None);
        assert_eq!(size_to_class(MAX_BLOCK_SIZE, 1), Some(MAX_SUPPORTED_CLASSES - 1));
    }
}
