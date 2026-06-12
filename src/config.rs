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
/// stronger alignment) fall through to the large-object allocator.
pub const MAX_BLOCK_SIZE: usize = SPAN_SIZE / 4;

/// Number of power-of-two large-run classes: run class `r` is a contiguous
/// run of `2^r` spans (64 KiB, 128 KiB, …, 4 GiB). Allocations that do not
/// fit a small size class are served by whole runs (guide Appendix A).
pub const MAX_LARGE_RUN_CLASSES: usize = 17;

/// Spans in the largest run class (2^16 spans = 4 GiB of 64 KiB spans).
/// Part of the worst-case bound: pagemap-style loops are bounded by it.
pub const MAX_LARGE_SPANS: usize = 1 << (MAX_LARGE_RUN_CLASSES - 1);

/// Largest run size in bytes (the maximum REQUEST is slightly smaller:
/// header reserve + LargeAllocHeader + alignment slack must also fit).
pub const MAX_LARGE_SIZE: usize = MAX_LARGE_SPANS * SPAN_SIZE;

/// Maximum number of free runs a thread heap retains per run class before
/// it publishes freed runs to its public SPMC run-list.
pub const LARGE_LOCAL_RUN_LIMIT_K: usize = 8;

/// Default huge-granule size in spans (16384 spans × 64 KiB = 1 GiB).
/// The actual granule is the `HUGE_GRANULE_SPANS` const generic on
/// [`crate::WfSpanAllocator`]; this is its default. The huge threshold
/// equals the granule size: requests with `size >= granule` dispatch to
/// the huge path (guide Appendix B.3/B.4).
pub const DEFAULT_HUGE_GRANULE_SPANS: usize = 16 * 1024;

/// Number of power-of-two huge-run classes: class `r` is `2^r` huge
/// granules (1/2/4 granules with the default of 3 classes).
pub const MAX_HUGE_RUN_CLASSES: usize = 3;

/// Granules in the largest huge run class.
pub const MAX_HUGE_GRANULES: usize = 1 << (MAX_HUGE_RUN_CLASSES - 1);

/// Fixed huge-run directory slots per class (guide B.7): at most this many
/// runs of one huge class may be live simultaneously; further huge
/// allocations of that class return null until one is freed.
pub const MAX_HUGE_RUNS_PER_CLASS: usize = 4;

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
const _: () = assert!(MAX_LARGE_RUN_CLASSES >= 1);
const _: () = assert!(LARGE_LOCAL_RUN_LIMIT_K >= 1);
const _: () = assert!(MAX_HUGE_RUN_CLASSES >= 1);
const _: () = assert!(MAX_HUGE_RUNS_PER_CLASS >= 1);
const _: () = assert!(DEFAULT_HUGE_GRANULE_SPANS >= 1);
// MAX_LARGE_SIZE (4 GiB) requires a 64-bit usize.
const _: () = assert!(usize::BITS >= 64);
