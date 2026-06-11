# Memory footprint accounting

wfspan trades a larger — but bounded — worst-case footprint for bounded
execution time. This file documents the bound and how the implementation
measures it.

## Sources of extra (temporarily unavailable) memory

1. **Spans left in HelpRecords.** One request can yield two spans; the
   extra one stays in the requester's record until its next acquisition.
   ≤ 1 per thread per class → `N · C · S`.
2. **Span-list query failure.** A thread queries only `P` lists; available
   spans may be elsewhere, forcing a raw-span acquisition:
   `(ceil(N/P) − 1) · (N − 1) · C · S`. With the prototype's fixed `P = N`
   this term is zero (making `P` configurable is listed future work).
3. **MPSC remote-list blocking.** Producers halted between SWAP and link
   make a span's free blocks temporarily unreachable:
   `N · (N − 1) · C · S` (span granularity).

Total (paper, approximate):

```
A(N) = (N + (ceil(N / P) + N − 1) · (N − 1)) · C · S  =  O(N²) for P = N
```

Implemented as `stats::theoretical_extra_bound(n, c, s, p)` and
`WfSpanAllocator::theoretical_extra_bound()` (with `P = N`).

## Observed statistics

`AllocatorStats` (always-on, Relaxed counters):

| Counter | Meaning |
|---|---|
| `allocated_spans` | raw spans taken from the fixed pool |
| `discarded_spans` | spans made ownerless with no visible free blocks |
| `claimed_spans` | discarded spans claimed by a remote freer (g: 0→1) |
| `published_spans` | full spans published after K overflow |
| `acquired_public_spans` | spans obtained through SPMC lists / help records |
| `help_record_spans` | spans stashed in a record (two-spans case) |
| `help_record_reclaimed` | stashed spans reclaimed by a later acquire |
| `remote_blocked_events` | consumptions stopped at an UNLINKED link |

`FixedSpanPool::spans_used()/spans_total()` give the observed span
footprint; the WCET-style bench (`benches/wcet_like.rs`) reports these
plus the theoretical bound after a contended run, and
`benches/remote_free.rs` reports claim/discard/blocked counts.

## Measurement plan beyond the prototype

Benchmark with `P = N, N/2, N/4` once `P` is configurable, and track a
high-water `max_observed_spans` gauge (currently derivable from
`spans_used`, since raw spans are never returned to the pool).
