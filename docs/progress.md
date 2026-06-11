# Progress (wait-freedom) constraints

This translates the paper's progress claims into implementation-level
constraints and shows where each is enforced.

## Bounds

| Operation | Bound | Enforced in |
|---|---|---|
| `try_pop_head_once` | O(1): exactly one CAS2 attempt, no retry | `spmc_span_list.rs` (single `compare_exchange`, three return paths) |
| `help_finishing_req` | O(1): ≤ 1 pop attempt + ≤ 1 CAS | `help_record.rs` |
| published helping request | O(((N−1)/H)·(N−1) + 1) completions by others | helping loop in `acquire.rs` |
| `spanlists_acquire_span` | O(N²) with H = 1, P = N | `acquire.rs`: help loop ≤ H + P iterations, query loop ≤ P iterations |
| allocation | bounded by `spanlists_acquire_span` + K-bounded rotation | `allocator.rs::alloc_with_token_counted` |
| deallocation | O(1): local push, or SWAP + link + FAA + ≤ 1 CAS | `allocator.rs::dealloc_local/dealloc_remote` |
| raw span acquisition | O(1): one FAA | `pagemap.rs` |
| remote-chain absorption | ≤ blocks_per_span per span | `remote_mpsc.rs::append_remote_to_local_bounded` |

The paper's alternative wfqueue-style protocol with an O(N) bound is not
implemented.

## Allowed loop bounds

Only loops statically bounded by one of: `N`, `C`, `P`, `H`, `K`,
`blocks_per_span` (plus exact-length list walks bounded by a maintained
`len`). Audit points:

- `alloc` rotation: `for _ in 0..=LOCAL_SPAN_LIMIT_K`
- helping: `while help_count < H && help_query < N`
- querying: `while help_query < N`
- span init / remote absorption: `for _ in 0..block_count`
- `remove_bounded`: `for _ in 0..limit` (limit = current list length)
- allocator init: `N × C`

## Forbidden patterns (checked in review, guarded by StepCounter)

- `loop { compare_exchange }` / unbounded CAS retry — a failed one-shot pop
  routes into publish → help(≤H) → finish, never a retry.
- Treiber-stack substitution for the SPMC list + helping.
- `Vec`/`Box`/`String`/`format!`/`println!`, `Mutex`/`RwLock`, OS
  allocation in the allocator core (`std` code lives only in
  `region.rs`/`verify.rs`/benches/tests).
- Dropping a completed HelpRecord span; treating `UNLINKED` as corruption;
  splitting pointer/version into two atomics.

## StepCounter guardrail

Every public alloc/dealloc path updates a `StepCounter`;
`StepCounter::assert_bounds(N, H, P, blocks_per_span, K)` is asserted per
operation in the concurrent smoke tests and the WCET-style bench. This is
an empirical guardrail against accidental unbounded loops, not a proof.

## Failure (null) semantics

Wait-freedom bounds steps, not success. Within its budget a thread may
fail to find a span (all P queried lists empty/contended) and may find the
fixed pool exhausted; it then returns null in bounded time. The paper's
footprint bound (docs/memory-footprint.md) quantifies the extra memory
this policy can strand temporarily.

## Backend caveat

On x86_64, versioned CAS2 (`lock cmpxchg16b`) emulates strong LL/SC, so
the one-shot pop is genuinely wait-free. On architectures with weak LL/SC
(aarch64), an equivalent guarantee needs CASP/LSE or different encoding —
unimplemented; compile error.

## Loom status

The `loom` feature flag is reserved but model tests are NOT wired up yet:
the core uses raw pointers to atomics inside spans (memory not owned by
loom types), which requires a shim layer mapping span headers onto
`loom::sync::atomic` cells. This is documented future work; concurrency
confidence currently comes from the concurrent smoke tests, the
StepCounter bounds, Miri on the sequential paths, and the quiescent
invariant checker.
