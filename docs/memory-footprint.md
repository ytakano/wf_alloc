# Memory footprint accounting

wfspan trades a larger — but bounded — worst-case footprint for bounded
execution time. This file documents the bound and how the implementation
measures it.

## Sources of extra (temporarily unavailable) memory

1. **Spans left in HelpRecords.** One request can yield two spans; the
   extra one stays in the requester's record until its next acquisition.
   <= 1 per thread per class -> `A * C * S`.
2. **Span-list query failure.** A thread queries only `P` lists; available
   spans may be elsewhere, forcing a raw-span acquisition:
   `(ceil(A/P) - 1) * (A - 1) * C * S`. With the prototype's fixed `P = A`
   this term is zero (making `P` configurable is listed future work).
3. **MPSC remote-list blocking.** Producers halted between SWAP and link
   make a span's free blocks temporarily unreachable:
   `A * (A - 1) * C * S` (span granularity).

Total (paper, approximate):

```
A_extra(A) = (A + (ceil(A / P) + A - 1) * (A - 1)) * C * S = O(A^2) for P = A
```

Implemented as `stats::theoretical_extra_bound(n, c, s, p)` and
`WfSpanAllocator::theoretical_extra_bound()` (with `P = active_threads`).

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
| `allocated_runs` | raw runs carved from the fixed pool |
| `published_runs` | freed runs published after the K_large overflow |
| `acquired_public_runs` | runs obtained through public run-lists / records |
| `run_help_record_runs` | runs stashed in a run record (two-runs case) |
| `run_help_record_reclaimed` | stashed runs reclaimed by a later acquire |

`FixedSpanPool::spans_used()/spans_total()` give the observed span
footprint; the WCET-style bench (`benches/wcet_like.rs`) reports these
plus the theoretical bound after a contended run, and
`benches/remote_free.rs` reports claim/discard/blocked counts.

## Large-run footprint sources

The large path adds three bounded sources of retained/stranded memory:

1. **Per-thread run caches.** Up to `LARGE_LOCAL_RUN_LIMIT_K` free runs
   per thread per run class stay in local lists (surplus is published).
2. **Runs left in run HelpRecords.** Same two-results-per-request bound
   as spans: ≤ 1 run per thread per run class.
3. **FAA-overshoot waste.** A failed multi-span carve whose rollback CAS
   loses strands fewer than `2^min_class` trailing raw spans, once per
   exhaustion race (see docs/progress.md). Carved memory always keeps
   recirculating.

Policy 1 (whole-larger-run, no split/no coalesce) additionally over-serves
a class-`k` request from a class-`j > k` run when the exact class is
unavailable; the over-allocation is temporary because the run returns to
class `j` on free.

## Huge-slot footprint sources

The huge path (guide Appendix B; fixed slot directory, header-less) has a
deliberately simple footprint profile:

1. **Carved slot memory is retained forever.** A lazily carved slot keeps
   its `2^r`-granule run for the allocator's lifetime, alternating
   FREE ↔ ALLOCATED. Worst case: the full directory capacity,
   `MAX_HUGE_RUNS_PER_CLASS · Σ 2^r · granule` (28 GiB at the 1 GiB
   default — provision the region accordingly, or accept nulls).
2. **Power-of-two granule rounding.** A 3-granule request consumes a
   4-granule run (B.1 calls this acceptable for strict RT). Header-less
   placement means requests of exactly 1/2/4 granules fit runs of exactly
   that size — no extra class bump for the header.
3. **No help-record or cache retention.** Unlike the large path, a freed
   huge run is globally claimable immediately; nothing is stranded per
   thread.

Contiguity (guide B.12): the crate returns contiguous ranges of the
caller-provided region — virtually contiguous. Physical contiguity (for
DMA etc.) is the responsibility of whoever provides the region.

`AllocatorStats::allocated_huge_runs` counts lazy carves.

## Measurement plan beyond the prototype

Benchmark with `P = A, A/2, A/4` once `P` is configurable, and track a
high-water `max_observed_spans` gauge (currently derivable from
`spans_used`, since raw spans are never returned to the pool).
