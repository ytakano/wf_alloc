//! wf_alloc: prototype of wfspan-style wait-free dynamic memory management.
//!
//! Based on: Ouyang & Zhu, "wfspan: Wait-free Dynamic Memory Management",
//! ACM TECS 21(4), 2022 (DOI 10.1145/3533724). See `docs/wfspan-model.md`
//! for the model, `docs/progress.md` for the progress (wait-freedom)
//! argument, and `docs/invariants.md` for the invariants the code keeps.
//!
//! The core is no_std-friendly (build with `--no-default-features`); the
//! `std` feature only adds test/bench harness helpers. The
//! `global` feature exposes a hosted `GlobalAlloc` wrapper for
//! hosted std targets.
//!
//! # Quick start
//!
//! ```
//! # #[cfg(feature = "std")] {
//! use core::alloc::Layout;
//! use wf_alloc::WfSpanAllocator;
//! use wf_alloc::region::OwnedRegion;
//!
//! // Four active threads; size classes (16 B – 16 KiB) and the huge granule
//! // use their defaults.
//! const ACTIVE_THREADS: usize = 4;
//!
//! // Pin the allocator in place; it must not move after init.
//! let region = OwnedRegion::new(64);
//! let alloc = Box::leak(Box::new(WfSpanAllocator::<{ wf_alloc::MAX_SUPPORTED_CLASSES }>::new(ACTIVE_THREADS)));
//! unsafe { alloc.init(region.ptr(), region.len()) };
//!
//! // Each thread registers once to obtain a token.
//! let token = alloc.register_thread().unwrap();
//!
//! let layout = Layout::new::<u64>();
//! let ptr = unsafe { alloc.alloc_with_token(layout, token) };
//! assert!(!ptr.is_null());
//! unsafe { alloc.dealloc_with_token(ptr, layout, token) };
//! # }
//! ```

#![cfg_attr(not(feature = "std"), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod acquire;
pub mod align;
pub mod allocator;
pub mod atomic_backend;
pub mod block;
pub mod config;
pub mod heap;
pub mod help_record;
pub mod huge;
pub mod large;
pub mod local_list;
pub mod metadata;
pub mod pagemap;
pub mod remote_mpsc;
pub mod size_class;
pub mod span;
pub mod spmc_span_list;
pub mod stats;
pub mod tagged;
pub mod thread;

#[cfg(feature = "global")]
pub mod global;
#[cfg(feature = "std")]
pub mod region;
#[cfg(feature = "std")]
pub mod verify;

pub use allocator::WfSpanAllocator;
pub use atomic_backend::{Cas2Backend, DefaultCas2Backend};
pub use config::{
    DEFAULT_HUGE_GRANULE_SPANS, HELP_BUDGET_H, LARGE_LOCAL_RUN_LIMIT_K, LOCAL_SPAN_LIMIT_K,
    MAX_BLOCK_SIZE, MAX_HUGE_GRANULES, MAX_HUGE_RUN_CLASSES, MAX_HUGE_RUNS_PER_CLASS,
    MAX_LARGE_RUN_CLASSES, MAX_LARGE_SIZE, MAX_LARGE_SPANS, MAX_SUPPORTED_CLASSES, MIN_BLOCK_SIZE,
    OWNER_NONE, OWNER_PUBLIC, SPAN_ALIGN, SPAN_SIZE,
};
pub use huge::{
    HUGE_SLOT_ALLOCATED, HUGE_SLOT_EMPTY, HUGE_SLOT_FREE, HugeArena, HugeRunSlot,
    huge_class_granules,
};
pub use large::{
    LARGE_MAGIC, LargeAllocHeader, run_class_bytes, run_class_for_layout, run_class_spans,
};
pub use metadata::RuntimeSlice;
pub use size_class::{class_to_size, size_to_class};
pub use stats::{AllocatorStats, StepCounter, theoretical_extra_bound};
pub use thread::{ThreadRegistry, ThreadToken};
