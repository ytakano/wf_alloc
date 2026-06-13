# wf_alloc

Prototype of **wfspan-style wait-free dynamic memory management** in Rust.

Based on: Ouyang & Zhu, *wfspan: Wait-free Dynamic Memory Management*, ACM
Transactions on Embedded Computing Systems 21(4), 2022
([DOI 10.1145/3533724](https://doi.org/10.1145/3533724)).

Unlike lock-free allocators (mimalloc, snmalloc), where a thread can lose
CAS races indefinitely, every `alloc` and `dealloc` here finishes in a
**statically bounded number of steps** — the property real-time systems
need.

## Highlights

- **Wait-free allocation and deallocation.** Every loop is bounded by
  the runtime active thread count and fixed constants (`C`, `P`, `H`, `K`,
  ...); a failed CAS is never retried — the thread publishes a request and relies on bounded helping.
  See [docs/progress.md](docs/progress.md) for the full bound table.
- **Three allocation tiers, one memory region.** Small blocks, multi-span
  large runs, and GiB-scale huge runs are all carved from a single
  caller-provided region. The wait-free alloc/dealloc paths never call the OS; runtime metadata is initialized up front from caller-provided or hosted storage.
- **Bootstrap-friendly core.** Build with `--no-default-features` for
  `no_std`. Hosted code may use `new(active_threads)`, while bare-metal
  boot code can use `from_metadata_region` / `from_uninit` to provide
  metadata storage without `Box` or an existing heap allocator.
- **Token-based API** with explicit per-thread registration for std tests
  and direct `token_from_raw` support for RTOS/bare-metal deployments.
- **Hosted global allocator** behind `global`.
  It builds dynamic wfspan shards on demand, reuses per-thread tokens after
  thread exit, and falls back to `std::alloc::System` for requests that the
  wfspan core cannot serve.
- **x86_64 and aarch64**: the SPMC list pop uses a versioned CAS2. On
  x86_64 this is `lock cmpxchg16b`; on aarch64 it is `caspal` when built
  with `target-feature=+lse` (strong CAS, on by default for e.g. Apple
  Silicon targets) and a one-shot `ldaxp`/`stlxp` exclusive pair
  otherwise. An LL/SC attempt may fail spuriously — failures route into
  the bounded helping protocol either way, so step bounds are unchanged.
  Other targets are a compile error.

## Quick start

```rust
use core::alloc::Layout;
use wf_alloc::WfSpanAllocator;
use wf_alloc::region::OwnedRegion;

// Four active threads; size classes and huge granule use their defaults.
const ACTIVE_THREADS: usize = 4;

// Pin the allocator in place; it must not move after init.
let region = OwnedRegion::new(64); // 64 spans = 4 MiB backing memory
let alloc = Box::leak(Box::new(
    WfSpanAllocator::<{ wf_alloc::MAX_SUPPORTED_CLASSES }>::new(ACTIVE_THREADS),
));
unsafe { alloc.init(region.ptr(), region.len()) };

// Each thread registers once to obtain a token.
let token = alloc.register_thread().unwrap();

let layout = Layout::new::<u64>();
let ptr = unsafe { alloc.alloc_with_token(layout, token) };
assert!(!ptr.is_null());
unsafe { alloc.dealloc_with_token(ptr, layout, token) };
```

## Allocation tiers

Dispatch is a pure function of the `Layout`, identical in alloc and
dealloc, so every pointer is freed on the path that allocated it.

| Request | Path | Mechanism | Alloc bound | Dealloc bound |
|---|---|---|---|---|
| ≤ 16 KiB | small | per-thread heaps, SPMC span-lists, bounded helping | O(A^2) | O(1) |
| > 16 KiB, < 1 GiB | large | whole runs of 2^r spans through the same SPMC + helping machinery | O(R*A^2) | O(1) |
| ≥ 1 GiB (≤ 4 GiB) | huge | fixed slot directory, one claim CAS per slot, lazy carve | O(R_h·SLOTS) | O(R_h·SLOTS) |

The huge tier deliberately avoids the helping protocol and per-thread
caches: at GiB granularity their (bounded) retention would be absurd
(see [docs/progress.md](docs/progress.md)).

## Configuration

```rust
WfSpanAllocator<
    const C: usize = 11,                     // small size classes: 16 B ... 16 KiB
    const HUGE_GRANULE_SPANS: usize = 16384, // huge granule: 1 GiB (= huge threshold)
>
```

`WfSpanAllocator::<11>::new(active_threads)` creates exactly
`active_threads` local heaps and help-record rows. `C` and
`HUGE_GRANULE_SPANS` are validated at compile time; `active_threads >= 1`
is checked at construction.


### Bare-metal bootstrap without `Box`

When wf_alloc is the heap allocator being brought up, `Box` is not available
yet. Use a raw metadata region instead:

```rust
let (alloc_value, metadata_used) = unsafe {
    WfSpanAllocator::<{ wf_alloc::MAX_SUPPORTED_CLASSES }>::from_metadata_region(
        active_threads,
        metadata_ptr,
        metadata_len,
    ).unwrap()
};
```

`metadata_ptr` must be aligned to
`WfSpanAllocator::<C>::metadata_region_align()`. The constructor consumes
only enough bytes for `active_threads` local heaps and help rows; inactive
CPU ids get no initialized local heap.

## `GlobalAlloc` wrapper

The `global` feature exposes `global::HostedLazyGlobalWfSpanAllocator`. It can
be installed as a hosted std global allocator:

```rust
use wf_alloc::global::{GlobalAllocatorConfig, HostedLazyGlobalWfSpanAllocator};

#[global_allocator]
static ALLOC: HostedLazyGlobalWfSpanAllocator =
    HostedLazyGlobalWfSpanAllocator::with_config(GlobalAllocatorConfig::new(16, 1024));
```

The config fields are `threads_per_shard` and `region_spans`;
`HostedLazyGlobalWfSpanAllocator::new(threads_per_shard, region_spans)` remains
as a shorthand. Each shard reserves one internal service token plus
`threads_per_shard` reusable user tokens, and uses `region_spans * 64 KiB` of
wfspan backing memory. Threads borrow a token
through TLS and return it when the thread exits. If all shards are full or a
wfspan region is too small for the request, the wrapper creates another shard
using `std::alloc::System` for metadata and backing memory. Requests that are
too large for wfspan, or cannot be served by a newly created shard, fall back
to `System`. Shards and their backing regions are retained for the process
lifetime; the wrapper does not shrink or reclaim shards. A hidden
per-allocation header records the backend so deallocation works from arbitrary
threads.

## Examples

- [`examples/multithreaded.rs`](examples/multithreaded.rs) — std setup,
  per-thread alloc/free with step-counter verification, producer-consumer
  remote frees, quiescent invariant check.
  `cargo run --example multithreaded`
- [`examples/baremetal.rs`](examples/baremetal.rs) — no_std/bare-metal
  pattern: static aligned region, runtime CPU count, and no metadata for
  inactive CPU ids. `cargo run --example baremetal`
- [`examples/global_wrapper.rs`](examples/global_wrapper.rs) — hosted
  `#[global_allocator]` use of `global::HostedLazyGlobalWfSpanAllocator`.
  `cargo run --features global --example global_wrapper`

## Feature flags

| Feature | Default | Effect |
|---|---|---|
| `std` | yes | test/bench harness helpers (`region`, `verify`) |
| `global` | no | hosted `GlobalAlloc` wrapper (requires std TLS) |
| `experimental-global` | no | compatibility alias for `global` |
| `stats`, `nightly` | no | reserved (`stats` are currently always on) |
| `loom` | no | small-state loom model tests for core concurrent subprotocols |

## Testing and verification

```sh
cargo test                       # unit + integration + doc tests
cargo test --features loom --test loom_models -- --nocapture
cargo clippy --all-targets
cargo bench                      # alloc_free, remote_free, wcet_like

# Sequential suites under Miri (PlainCas2 backend, no asm):
MIRIFLAGS=-Zmiri-ignore-leaks cargo +nightly miri test \
  --test sequential --test remote_free --test exhaustion \
  --test large_sequential --test huge_sequential
```

Structural guardrails: every operation updates a `StepCounter` whose
bounds are asserted in tests and benches (an empirical wait-freedom
check), and a quiescent-state verifier (`verify::check_quiescent`) walks
all lists, help records, and huge slots to check the conservation
invariants after each test.

## Documentation

- [docs/wfspan-model.md](docs/wfspan-model.md) — the model implemented,
  parameters, target assumptions
- [docs/progress.md](docs/progress.md) — the wait-freedom argument: per-op
  bounds, allowed loops, forbidden patterns
- [docs/invariants.md](docs/invariants.md) — state-machine and ownership
  invariants the code maintains
- [docs/memory-footprint.md](docs/memory-footprint.md) — bounded extra
  memory: the paper's active-thread bound and the per-tier retention sources
- [docs/unsafe-audit.md](docs/unsafe-audit.md) — every unsafe category and
  its justification; Miri status

## Limitations

This is a research prototype, not a production allocator:

- x86_64 and AArch64 only; other architectures fail to compile.
- The core `WfSpanAllocator` has a fixed, caller-provided memory region;
  exhaustion returns null (the wait-free path never calls the OS). Raw spans
  are never returned to the pool — they recirculate through per-thread lists.
- The core `WfSpanAllocator` supports at most `active_threads` registered
  threads and a maximum wfspan-served request of 4 GiB (the largest huge run
  class). The hosted `GlobalAlloc` wrapper relaxes those hosted
  constraints by adding shards dynamically and falling back to `System`, but
  allocations served by `System` are not wait-free wfspan operations.
- No `realloc`, no NUMA awareness, no cross-process use.
- Concurrency confidence comes from smoke tests, step-bound assertions,
  Miri (sequential paths), the invariant checker, and small-state loom
  models for the core concurrent subprotocols.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
