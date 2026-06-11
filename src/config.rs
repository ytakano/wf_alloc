//! Fixed implementation parameters (paper: SPAN_SIZE = 64 KiB, K = 40, H = 1, P = N).

/// Size of one span. Must be a power of two.
pub const SPAN_SIZE: usize = 64 * 1024;

/// Spans are aligned to their own size so the owning span of any block
/// pointer can be recovered by masking low address bits (no pagemap needed).
pub const SPAN_ALIGN: usize = SPAN_SIZE;

/// Default maximum number of participating threads (N for the default config).
pub const MAX_THREADS: usize = 64;

/// Maximum number of private spans a thread heap retains per size class
/// before it starts publishing surplus full spans to its public SPMC list.
pub const LOCAL_SPAN_LIMIT_K: usize = 40;

/// Helping budget H: maximum number of pending requests helped during one
/// `spanlists_acquire_span` call.
pub const HELP_BUDGET_H: usize = 1;

/// Smallest block size (and therefore strongest natural alignment of the
/// smallest size class).
pub const MIN_BLOCK_SIZE: usize = 16;

/// Largest supported block size. Allocations larger than this (or with
/// stronger alignment) are unsupported by the prototype and return null.
pub const MAX_BLOCK_SIZE: usize = SPAN_SIZE / 4;

/// Bytes reserved at the start of every span for the `SpanHeader`.
pub const SPAN_HEADER_RESERVE: usize = 1024;

/// Number of power-of-two size classes representable in one span:
/// 16, 32, ..., 16384.
pub const MAX_SUPPORTED_CLASSES: usize =
    MAX_BLOCK_SIZE.trailing_zeros() as usize - MIN_BLOCK_SIZE.trailing_zeros() as usize + 1;

/// Owner-field sentinel: span is discarded / ownerless and may be claimed by
/// a remote deallocator whose FAA moves the global free count from 0 to 1.
pub const OWNER_NONE: usize = usize::MAX;

/// Owner-field sentinel: span is published in (or in transit through) a
/// public SPMC span-list or a help record. It must not be claimed by remote
/// deallocators; only the popping/reclaiming thread takes ownership.
pub const OWNER_PUBLIC: usize = usize::MAX - 1;

const _: () = assert!(SPAN_SIZE.is_power_of_two());
const _: () = assert!(MIN_BLOCK_SIZE.is_power_of_two());
const _: () = assert!(MAX_BLOCK_SIZE.is_power_of_two());
const _: () = assert!(MIN_BLOCK_SIZE >= core::mem::size_of::<usize>() * 2);
