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

- **Wait-free allocation and deallocation.** Every loop is bounded by a
  compile-time constant (`N`, `C`, `P`, `H`, `K`, …); a failed CAS is never
  retried — the thread publishes a request and relies on bounded helping.
  See [docs/progress.md](docs/progress.md) for the full bound table.
- **Three allocation tiers, one memory region.** Small blocks, multi-span
  large runs, and GiB-scale huge runs are all carved from a single
  caller-provided region. The allocator never calls the OS.
- **`no_std`-friendly core.** Build with `--no-default-features`; the `std`
  feature only adds test/bench harness helpers.
- **Token-based API** plus an optional `GlobalAlloc` wrapper (`global`
  feature) with automatic per-thread registration.
- **x86_64 only** for now: the SPMC list pop uses a versioned CAS2
  (`lock cmpxchg16b`) to emulate strong LL/SC. Other targets are a
  compile error (aarch64 needs CASP/LSE — future work).

## Quick start

```rust
use core::alloc::Layout;
use wf_alloc::WfSpanAllocator;
use wf_alloc::region::OwnedRegion;

// Up to 4 threads; size classes and huge granule use their defaults.
const N: usize = 4;

// Pin the allocator in place; it must not move after init.
let region = OwnedRegion::new(64); // 64 spans = 4 MiB backing memory
let alloc = Box::leak(Box::new(WfSpanAllocator::<N>::new()));
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
| ≤ 16 KiB | small | per-thread heaps, SPMC span-lists, bounded helping | O(N²) | O(1) |
| > 16 KiB, < 1 GiB | large | whole runs of 2^r spans through the same SPMC + helping machinery | O(R·N²) | O(1) |
| ≥ 1 GiB (≤ 4 GiB) | huge | fixed slot directory, one claim CAS per slot, lazy carve | O(R_h·SLOTS) | O(R_h·SLOTS) |

The huge tier deliberately avoids the helping protocol and per-thread
caches: at GiB granularity their (bounded) retention would be absurd
(see [docs/progress.md](docs/progress.md)).

## Configuration

```rust
WfSpanAllocator<
    const N: usize,                          // max participating threads
    const C: usize = 11,                     // small size classes: 16 B … 16 KiB
    const HUGE_GRANULE_SPANS: usize = 16384, // huge granule: 1 GiB (= huge threshold)
>
```

`WfSpanAllocator<4>` is a complete configuration: 4 threads, all 11 small
classes, 1 GiB huge granule. Parameters are validated at compile time.

## `GlobalAlloc` wrapper (feature `global`)

```rust
use wf_alloc::global::GlobalWfSpanAllocator;

// 128 SPAN_SIZE-aligned spans as backing memory.
#[repr(align(65536))]
struct AlignedRegion([u8; 128 * 65536]);
static mut REGION: AlignedRegion = AlignedRegion([0u8; 128 * 65536]);

#[global_allocator]
static ALLOC: GlobalWfSpanAllocator<8> = GlobalWfSpanAllocator::new();

// Call once before any heap allocation (e.g., early in `main`).
fn setup() {
    unsafe { ALLOC.init((&raw mut REGION.0).cast::<u8>(), 128 * 65536) };
}
```

Threads register automatically on first use. Threads beyond `N` cannot
register: their allocations return null and their frees leak (never UB).

## Examples

- [`examples/multithreaded.rs`](examples/multithreaded.rs) — std setup,
  per-thread alloc/free with step-counter verification, producer-consumer
  remote frees, quiescent invariant check.
  `cargo run --example multithreaded`
- [`examples/baremetal.rs`](examples/baremetal.rs) — no_std/bare-metal
  pattern: static aligned region, `const fn new()` in `.bss`, runtime CPU
  count with `token_from_raw`. `cargo run --example baremetal`

## Feature flags

| Feature | Default | Effect |
|---|---|---|
| `std` | yes | test/bench harness helpers (`region`, `verify`) |
| `global` | no | `GlobalAlloc` wrapper (requires std TLS) |
| `stats`, `loom`, `nightly` | no | reserved (stats are currently always on; loom is not wired up) |

## Testing and verification

```sh
cargo test                       # unit + integration + doc tests
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
  memory: the paper's A(N) bound and the per-tier retention sources
- [docs/unsafe-audit.md](docs/unsafe-audit.md) — every unsafe category and
  its justification; Miri status

## Limitations

This is a research prototype, not a production allocator:

- x86_64 with `cmpxchg16b` only; other architectures fail to compile.
- Fixed, caller-provided memory region; exhaustion returns null (the
  wait-free path never calls the OS). Raw spans are never returned to the
  pool — they recirculate through per-thread lists.
- Maximum single request: 4 GiB (the largest huge run class).
- At most `N` threads (compile-time constant).
- No `realloc`, no NUMA awareness, no cross-process use.
- Concurrency confidence comes from smoke tests, step-bound assertions,
  Miri (sequential paths), and the invariant checker — loom model checking
  is future work.

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
