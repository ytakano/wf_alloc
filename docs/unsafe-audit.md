# Unsafe audit

Policy: `#![deny(unsafe_op_in_unsafe_fn)]`; every `unsafe` block carries a
`// SAFETY:` comment; every `unsafe fn` documents its contract in a
`# Safety` section. This file records the categories and the residual
risks.

## Categories of unsafe in the core

1. **Raw span/block pointer dereference** (`span.rs`, `local_list.rs`,
   `remote_mpsc.rs`, `heap.rs`, `allocator.rs`). Justification: spans live
   in the caller-provided region for the allocator's lifetime; blocks are
   derived from span payloads; nothing is ever unmapped or returned to the
   pool, so pointers cannot dangle. All header fields are atomics, so
   shared access has no data races; *owner-only* fields use Relaxed and
   rely on the documented ownership-transfer edges:
   - SPMC enqueue release-store → consumer acquire-load of `next`,
   - help-record completion CAS (AcqRel) → `reclaim_request` SWAP (AcqRel),
   - owner claim/discard CAS/stores (AcqRel/Release/Acquire),
   - MPSC SWAP (AcqRel) and link store (Release) → consumer Acquire.
2. **Inline asm `lock cmpxchg16b`** (`atomic_backend.rs`). 16-byte-aligned
   operand guaranteed by `HeadWord`'s `align(16)` through `UnsafeCell`.
   The rbx-reservation is handled with an xchg/mov pair. The atomic-load
   trick (CAS with arbitrary expected) may issue a benign identical-value
   store when the guess matches.
3. **Self-referential SPMC dummies** (`spmc_span_list.rs::init`,
   `WfSpanAllocator::init`). Contract: init exactly once, before sharing,
   and never move the allocator afterwards (tests/benches use
   `Box::leak`). A `Pin`-based constructor would make this typed; future
   work.
4. **In-place header construction** (`span.rs::init_span`,
   `span.rs::init_run`) via `ptr::write` into exclusively owned raw
   memory.
5. **Producer-only `UnsafeCell` tail** (`spmc_span_list.rs`): mutated only
   by the unique producer per the `enqueue_by_owner` contract.
6. **Huge slot claim and payload placement** (`huge.rs`): a single
   EMPTY/FREE→ALLOCATED CAS hands the claiming thread exclusive use of
   the slot's run; `base` is written once under the EMPTY claim and is
   immutable afterwards. The payload pointer is derived from `base` by
   pointer arithmetic (provenance-preserving). Deallocation recovers the
   slot by a bounded address-range scan of the fixed directory — no
   hidden header is read or written; double free is debug-detected via
   the slot state.
7. **Large-run header recovery** (`large.rs`):
   `place_large_payload` writes a `LargeAllocHeader` past the base span's
   reserve area (placement proven by `run_class_for_layout`);
   `dealloc_large_with_token_counted` reads it back at
   `ptr - size_of::<LargeAllocHeader>()`. Soundness rests on the dispatch
   invariant (docs/invariants.md): the Layout passed to dealloc routes
   every large pointer back to the large path, and the magic field is
   debug-asserted. `alloc_large_with_token_counted` relies on exclusive
   run ownership handed over by the local list pop, the helping acquire
   (release/acquire via the SPMC list), or the raw carve.

## Caller-facing contracts

- `alloc_with_token`/`dealloc_with_token`: token from this allocator's
  registry, one thread per token at a time; pointers freed exactly once.
  Double free is detected only opportunistically in debug builds (list
  pattern asserts); in release builds it is documented caller UB, as in
  the paper's C-level model.
- `ThreadRegistry::token_from_raw`: id < N, externally deduplicated.
- `FixedSpanPool::set_region`: region valid, unused, outliving the
  allocator.

## Miri status

- Sequential test suites (`sequential`, `remote_free`, `exhaustion`,
  `large_sequential`, `huge_sequential`) pass under Miri with
  `-Zmiri-ignore-leaks` (the leaks are intentional `Box::leak` test
  fixtures pinning the allocator).
- Miri uses the `PlainCas2` backend (no asm); it is non-atomic and
  documented sequential-only.
- Integer↔pointer casts (span masking, `HeadWord.ptr`, pool base) make
  Miri run in permissive-provenance mode; `UNLINKED` uses
  `ptr::without_provenance_mut`. Strict provenance would require carrying
  provenance through the SPMC head word (e.g. `AtomicPtr`-based 2-word
  encoding) — future work.
- Concurrent tests are not Miri-clean by construction (asm backend);
  they run under the native build only.

## Known residual risks

- aarch64 and other targets unsupported (compile error) — see
  docs/wfspan-model.md.
- The quiescent verifier requires external quiescence; calling it
  concurrently is a (test-harness-only) race.
- `GlobalAlloc` wrapper: threads beyond N cannot register; their frees
  leak (never UB) and their allocs return null. Documented in
  `global.rs`.
