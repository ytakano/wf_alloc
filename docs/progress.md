# Progress (wait-freedom) constraints

This translates the paper's progress claims into implementation-level
constraints and shows where each is enforced.

## Bounds

| Operation | Bound | Enforced in |
|---|---|---|
| `try_pop_head_once` | O(1): exactly one CAS2 attempt, no retry | `spmc_span_list.rs` (single `compare_exchange`, three return paths) |
| `help_finishing_req` | O(1): ≤ 1 pop attempt + ≤ 1 CAS | `help_record.rs` |
| published helping request | O(((A-1)/H)*(A-1) + 1) completions by others | helping loop in `acquire.rs` |
| `spanlists_acquire_span` | O(A^2) with H = 1, P = A | `acquire.rs`: help loop ≤ H + P iterations, query loop ≤ P iterations |
| allocation | bounded by `spanlists_acquire_span` + K-bounded rotation | `allocator.rs::alloc_with_token_counted` |
| deallocation | O(1): local push, or SWAP + link + FAA + ≤ 1 CAS | `allocator.rs::dealloc_local/dealloc_remote` |
| raw span acquisition | O(1): one FAA | `pagemap.rs` |
| raw run acquisition | O(1): one FAA + ≤ 1 rollback CAS (never retried) | `pagemap.rs::acquire_raw_run` |
| `runlists_acquire_run` | O(A^2) with H = 1, P = A (same shared core) | `acquire.rs::acquire_from_lists` |
| large allocation | O(R * A^2): ≤ R = MAX_LARGE_RUN_CLASSES class steps, each ≤ one acquire; one carve | `large.rs::alloc_large_with_token_counted` |
| large deallocation | O(1): header read + owner store + one push or one publish | `large.rs::dealloc_large_with_token_counted` |
| huge allocation | O(R_h · SLOTS): ≤ 12 slot scans, ≤ 1 claim CAS each, ≤ 1 carve FAA per EMPTY claim | `huge.rs::alloc_huge_with_token_counted` |
| huge deallocation | O(R_h · SLOTS): bounded directory reverse lookup + one release store, no CAS | `huge.rs::dealloc_huge_with_token_counted` |
| remote-chain absorption | ≤ blocks_per_span per span | `remote_mpsc.rs::append_remote_to_local_bounded` |

The paper's alternative wfqueue-style protocol with an O(A) bound is not
implemented.

## Allowed loop bounds

Only loops bounded by one of: active thread count `A`, `C`, `P`, `H`, `K`,
`blocks_per_span` (plus exact-length list walks bounded by a maintained
`len`). Audit points:

- `alloc` rotation: `for _ in 0..=LOCAL_SPAN_LIMIT_K`
- helping: `while help_count < H && help_query < active_threads`
- querying: `while help_query < active_threads`
- span init / remote absorption: `for _ in 0..block_count`
- `remove_bounded`: `for _ in 0..limit` (limit = current list length)
- allocator init: `A * (C + MAX_LARGE_RUN_CLASSES)`
- large class search: `for class in min_class..MAX_LARGE_RUN_CLASSES`
- huge slot scan: `for class in min_class..MAX_HUGE_RUN_CLASSES`,
  `for slot in 0..MAX_HUGE_RUNS_PER_CLASS` (alloc and dealloc lookup)

## Why the huge path avoids the helping protocol

GiB-scale requests must not flow through the large path's SPMC + helping
machinery: a HelpRecord may strand one extra run per thread per class and
`local_runs` retains up to K freed runs per thread per class — bounded,
but at GiB granularity the bound is absurd (guide B.13: A = 64 threads *
3 classes * 1 GiB ~= 192 GiB of stranded memory). The fixed slot directory
has no help records and no caches: a freed huge run is globally claimable
after one store, and the worst-case retained memory is exactly the carved
directory capacity.

## Large-path exhaustion semantics

`acquire_raw_run` keeps wait-freedom at exhaustion by NOT retrying: the
multi-span FAA may overshoot `next`, and a single rollback CAS tries to
hand the tail back. If that CAS loses, fewer than `2^min_class` trailing
spans are permanently skipped (bounded, one-shot waste per exhaustion
race); carved spans and runs keep recirculating through their lists, so
no live memory is lost. Fresh carving is attempted only at the exact
class — after a `2^k`-span carve fails, any larger carve must fail too,
so escalated classes use only list reuse (Policy 1).

## Forbidden patterns (checked in review, guarded by StepCounter)

- `loop { compare_exchange }` / unbounded CAS retry — a failed one-shot pop
  routes into publish → help(≤H) → finish, never a retry.
- Treiber-stack substitution for the SPMC list + helping.
- `Vec`/`Box`/`String`/`format!`/`println!`, `Mutex`/`RwLock`, or OS
  allocation in the wait-free alloc/dealloc paths. `WfSpanAllocator::new`
  may allocate hosted runtime metadata before the allocator is shared;
  bare-metal code can use `from_metadata_region` or `from_uninit` to avoid
  heap allocation during bootstrap.
- Dropping a completed HelpRecord span; treating `UNLINKED` as corruption;
  splitting pointer/version into two atomics.

## StepCounter guardrail

Every public alloc/dealloc path updates a `StepCounter`;
`StepCounter::assert_bounds(A, H, P, blocks_per_span, K)` is asserted per
operation in the concurrent smoke tests and the WCET-style bench, and
`StepCounter::assert_large_bounds(A, H, P, R)` per large operation in the
large test suites (including under contention, where the removed
Treiber-stack implementation would have spun), and
`StepCounter::assert_huge_bounds(R_h, SLOTS)` per huge operation in the
huge test suites. This is an empirical guardrail against accidental
unbounded loops, not a proof.

## Hosted `GlobalAlloc` wrapper boundary

The `global` feature provides a hosted `GlobalAlloc` wrapper around the core
allocator. A wfspan-served allocation or deallocation still routes through the
core bounded operations after a thread has a shard token. The wrapper itself is
not a wait-free proof boundary: shard creation, TLS token binding/destruction,
reserved service-token frees, `std::alloc::System` fallback, and
destructor-time fallback are hosted support paths and may allocate, spin, or
call the system allocator. Diagnostics expose these paths so production tests
can detect unexpected fallback or shard growth.

## Failure (null) semantics

Wait-freedom bounds steps, not success. Within its budget a thread may
fail to find a span (all P queried lists empty/contended) and may find the
fixed pool exhausted; it then returns null in bounded time. The paper's
footprint bound (docs/memory-footprint.md) quantifies the extra memory
this policy can strand temporarily.

## Backend caveat

On x86_64 (`lock cmpxchg16b`) and aarch64 with FEAT_LSE (`caspal`),
versioned CAS2 is a strong CAS, so the one-shot pop is genuinely
wait-free: failure proves another thread progressed. On baseline aarch64
the backend is one `ldaxp`/`stlxp` exclusive pair per attempt — step
bounds still hold unconditionally, but a failure may be spurious, so the
"failure implies global progress" lemma is best-effort there (see
docs/wfspan-model.md, Target architecture assumptions). Other targets:
compile error.

## Loom status

The `loom` feature flag is reserved but model tests are NOT wired up yet:
the core uses raw pointers to atomics inside spans (memory not owned by
loom types), which requires a shim layer mapping span headers onto
`loom::sync::atomic` cells. This is documented future work; concurrency
confidence currently comes from the concurrent smoke tests, the
StepCounter bounds, Miri on the sequential paths, and the quiescent
invariant checker.
