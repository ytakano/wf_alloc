# Invariants

The std-only quiescent checker `verify::check_quiescent` asserts the
checkable subset of these after every test; debug_asserts in the core check
local ones online.

## Block invariants

- A block is either allocated or free, never both.
- A free block appears in at most one of: its span's local free-list, its
  span's remote MPSC list, the span's `pending_remote` stash.
- An allocated block appears in no free-list (pattern-checked in tests).
- A block belongs to exactly one span; `span_from_ptr(block) == its span`
  (guaranteed by `S`-alignment and in-span placement).
- `block_from_payload(block_payload(b)) == b` (identity casts).
- `UNLINKED` may appear only as a remote-list `next` link, never in a local
  free-list.

## Span invariants

- After `init_span`, `size_class`, `block_size`, `block_count` never change
  (spans are never re-typed in the prototype).
- `owner` is a thread id `< N`, `OWNER_NONE` (discarded), or `OWNER_PUBLIC`
  (in a public list or help record / in transit).
- An owned span is in exactly one local span-list — its owner's, filed
  under its own size class.
- A span is never in a local list and a public SPMC list at the same time
  (checked via the `seen` set in the verifier).
- A discarded span is in no list and has `owner == OWNER_NONE`; the first
  remote free whose FAA moves `g` from 0 to 1 claims it (CAS on owner).
- Accounting: `0 ≤ q ≤ m`; at quiescence `g ≥ 0` and `q + g ≤ m`; `g` may
  dip negative transiently while a producer is between its SWAP and FAA.
  The free count is an accounting protocol, not a traversal of reality.
- Full means `q + g = m`; only full spans are published in this prototype.
- At most one `pending_remote` chain per span; while it is non-null the
  live remote list is not reclaimed (prevents two stashed chains).

## HelpRecord invariants

- Empty record contains 0; pending has low bit 1 (`phase << 1 | 1`);
  completed contains an even (aligned) span pointer.
- A completed record OWNS its span: it is never overwritten without
  `reclaim_request`, and every acquire reclaims before publishing.
- Phases increase monotonically (mod word size); helpers re-read the phase
  before completing, so helping the same record twice is safe.
- Spans in records keep `owner == OWNER_PUBLIC`, so a racing remote freer
  cannot claim them away (no double ownership).

## SPMC invariants

- Only the heap-owner thread enqueues (no CAS on enqueue; release store).
- `try_pop_head_once` performs at most ONE CAS2 attempt; head pointer and
  version are one 16-byte atomic value, never two atomics.
- The version increments on every successful pop (ABA protection).
- A popped span is returned to the caller, stored in a help record, or (in
  `spanlists_acquire_span`) stashed back into the caller's own record —
  never dropped.
- Node conservation: every span owns one `SpanNode`; enqueue consumes it,
  a successful pop hands the outgoing dummy to the popped span. Nodes are
  never freed.

## LargeRun invariants (guide Appendix A, Policy 1)

- A run is `2^run_class` contiguous spans carved from the same pool as
  small spans; its base span carries a `SpanHeader` (`init_run`) with
  `size_class = run_class`, `block_count = span_count`. Interior spans
  carry NO header and are never walked.
- After `init_run`, the run class never changes (Policy 1: no splitting,
  no coalescing). A freed run returns to a list of its OWN class.
- A run is either allocated (`RunAllocated`, in no list) or free
  (`RunFreeLocal` in exactly one local run-list, or `RunFreePublic` in
  one public run-list / run help record) — never both, never in two lists.
- An allocated run has a valid `LargeAllocHeader` (magic, run pointer,
  run class) immediately before the payload; `header.run` points to the
  base span.
- No run overlaps a small span or another run (page-occupancy check in
  the verifier).
- Runs never use the per-block machinery: local free-list empty,
  `remote.free_count == 0`, no `OWNER_NONE`/`try_discard`/remote MPSC.
  Cross-thread free transfers the WHOLE run to the freeing thread.

## Dispatch invariant (single region)

- Small-vs-large dispatch is a pure function of `Layout`
  (`size_to_class` yields a class `< C` → small), applied identically in
  alloc and dealloc. Hence `span_from_ptr` (SPAN_SIZE masking) is never
  applied to a large payload, whose masked address could be a headerless
  interior span of a run. This relies on the GlobalAlloc-style contract
  that dealloc receives the Layout the pointer was allocated with.

## MPSC invariants

- Push is SWAP then link; `UNLINKED` is a valid temporary `next`.
- The consumer stops at `UNLINKED` without spinning and stashes the suffix;
  consumption is bounded by `block_count`.
- Blocked memory is bounded at span granularity and is recovered once the
  producer's link store lands.
