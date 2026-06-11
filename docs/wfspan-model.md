# wfspan model (as implemented by wf_alloc)

Source: Ouyang & Zhu, *wfspan: Wait-free Dynamic Memory Management*, ACM
TECS 21(4), 2022, DOI 10.1145/3533724. This document defines the model the
Rust prototype implements and the assumptions it makes.

## Parameters and terms

| Symbol | Meaning | Prototype value |
|---|---|---|
| `N` | Maximum number of participating threads | const generic (`WfSpanAllocator<N, C>`) |
| `C` | Number of size classes | const generic, ≤ 11 (`MAX_SUPPORTED_CLASSES`) |
| `S` | Span size | 64 KiB (`SPAN_SIZE`), spans are `S`-aligned |
| `K` | Per-thread private span limit per class | 40 (`LOCAL_SPAN_LIMIT_K`) |
| `H` | Helping budget per acquisition | 1 (`HELP_BUDGET_H`) |
| `P` | Public span-lists queried per acquisition | `N` (fixed; configurability is future work) |

- **block** — the unit returned to the user. Power-of-two sizes
  16 B … 16 KiB; a free block stores its free-list link in its own memory.
- **span** — `S`-byte, `S`-aligned region: header + same-size blocks. The
  abstract state is `(T, m, q, g)`: owner, max blocks, local free count,
  global free count. `span_from_ptr` is an address mask (no pagemap).
- **thread heap** — per-thread; for each size class a *private local
  span-list* (allocation buffer) and a *public SPMC span-list*.
- **local free-list** — owner-only block list inside a span (`q` blocks).
- **MPSC remote free-list** — per-span wait-free list for non-owner frees
  (`remote_mpsc.rs`); counted by `g` via FAA.
- **SPMC span-list** — per-heap-per-class public list; single producer (the
  heap owner, no CAS on enqueue), multiple consumers using a one-shot
  versioned-CAS2 pop (`spmc_span_list.rs`).
- **help record** — one `AtomicUsize` per thread per class encoding
  empty / pending(phase) / completed(span pointer) (`help_record.rs`).

## Why non-linearizability is acceptable

Allocator-internal lists do not need to be linearizable queues; they need to
not lose memory and to keep "extra" unavailable memory bounded:

- **MPSC push** is SWAP-then-link. A producer halted between the two steps
  leaves an `UNLINKED` link; the consumer stops there (never spins), stashes
  the blocked suffix in `span.local.pending_remote`, and retries on a later
  allocation. Blocked memory is bounded at span granularity.
- **SPMC + helping** can give one request two spans (one popped directly,
  one completed into the help record by a helper). The extra span stays in
  the record — which *owns* it — and is reclaimed by the next acquisition
  of that thread/class. At most one such span per thread per class.

## How allocation stays bounded

Every loop in the allocation path has a static bound (see docs/progress.md):
local span rotation (≤ K+1), remote-chain absorption (≤ blocks_per_span),
helping (≤ H), public-list queries (≤ P = N), raw-span FAA (1). A failed
CAS2 pop is *never retried*; the thread publishes a request and relies on
bounded helping. Total: O(N²) steps with H = 1, matching the paper.

## How deallocation stays bounded

Owner free: one local push (O(1)). Remote free: one SWAP, one link store,
one FAA, plus at most one claim CAS when `g` crosses 0→1 (discarded-span
reclaim). No loops. O(1).

## How extra memory stays bounded

See docs/memory-footprint.md; the paper's bound
`A(N) = (N + (ceil(N/P) + N − 1)(N − 1)) · C · S` is exposed as
`WfSpanAllocator::theoretical_extra_bound()`.

## Target architecture assumptions

- **x86_64 with `cmpxchg16b`** (all x86_64 CPUs of the last ~15 years):
  versioned CAS2 emulates strong LL/SC for the SPMC head. This is the only
  production backend currently implemented.
- **Miri**: a plain, non-atomic `PlainCas2` backend is substituted so
  *sequential* tests run under Miri; it is not safe concurrently.
- **aarch64** is *not* supported yet: weak LL/SC can be interrupted
  indefinitely, so a port needs CASP/LSE or a one-word versioned-pointer
  encoding; `atomic_backend.rs` emits a compile error on other targets.
- The fixed span pool requires a caller-provided region that outlives the
  allocator; the wait-free path never calls the OS.

## Out of scope for the prototype

`realloc`, full malloc ABI, OS mmap/sbrk backend, huge (> 16 KiB)
allocations, NUMA, cross-process use, returning raw spans to the pool,
re-typing a span to a different size class, hard real-time certification,
and loom model checking (feature flag reserved; see docs/progress.md).
