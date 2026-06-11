//! Small alignment helpers. `align` must be a power of two.

/// Round `x` up to the next multiple of `align`.
pub const fn round_up(x: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (x + align - 1) & !(align - 1)
}

/// Round `x` down to the previous multiple of `align`.
pub const fn round_down(x: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    x & !(align - 1)
}

/// Whether `x` is a multiple of `align`.
pub const fn is_aligned(x: usize, align: usize) -> bool {
    debug_assert!(align.is_power_of_two());
    x & (align - 1) == 0
}
