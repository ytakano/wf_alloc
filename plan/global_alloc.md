# Production-grade std global allocator roadmap

This document tracks the work needed to promote the current
`global` wrapper into a production-grade `std` global allocator.
The core `WfSpanAllocator` remains a fixed-participant, fixed-region,
wait-free allocator. Hosted `GlobalAlloc` support is a wrapper layer that maps
dynamic `std` process behavior onto that core with dynamic shards, TLS token
management, and fallback allocation.

## Current state

The hosted wrapper is `global::HostedLazyGlobalWfSpanAllocator` behind
the `global` feature.

Implemented behavior:

- `#[global_allocator] static` can be const-initialized.
- Shards are created lazily with `std::alloc::System`.
- Each shard owns one `WfSpanAllocator`, one backing span region, one reserved
  service token, and a bounded number of reusable user token slots.
- Threads borrow a shard token through TLS and return it from a TLS destructor
  when the thread exits.
- If all existing shards are full, the wrapper creates another shard.
- If the current shard cannot satisfy a request because its region is too
  small or exhausted, the wrapper can create a larger shard and retry.
- Requests not served by wfspan fall back to `System`.
- A hidden allocation header records whether a pointer belongs to wfspan or to
  `System`, and records the shard needed for wfspan deallocation.
- Cross-thread frees are supported. If the freeing thread is not registered
  with the allocation's shard, the wrapper serializes use of the shard's
  reserved service token.

Residual constraints:

- Shards and their backing regions are process-lifetime allocations and are not
  reclaimed.
- Shard creation, fallback allocation, TLS setup/destruction, and service-token
  frees are hosted glue; they are not wfspan wait-free operations.
- Allocation headers are validated before backend dispatch; bad magic, backend,
  layout, offset, and null wfspan shard are debug-asserted and ignored in
  release builds.
- TLS destructor allocation is covered by System fallback when reusable wfspan
  token TLS is already unavailable. Process shutdown and unusual runtime
  contexts still need stress and model coverage.
- Diagnostics now expose a snapshot with shard count, token pressure, wfspan
  success/failure counters, System fallback counters, and service-token
  deallocation count.

## Production acceptance criteria

A production-grade std global allocator must satisfy these criteria:

- It is safe to use as `#[global_allocator]` in ordinary Rust `std` programs
  with dynamic thread creation and destruction.
- It handles allocations and deallocations from arbitrary threads, including
  frees after the allocating thread has exited.
- It handles all `GlobalAlloc` entry points: `alloc`, `alloc_zeroed`,
  `dealloc`, and `realloc`.
- It preserves requested alignment for all supported layouts.
- It has defined behavior for wfspan exhaustion, host OOM, oversized requests,
  invalid headers in debug builds, and fallback allocations.
- It exposes diagnostics for unexpected System fallback, shard growth, token
  pressure, and allocation failures in tests and production telemetry.
- It documents which paths retain wfspan wait-free bounds and which paths are
  hosted/non-wait-free support paths.
- It has stress tests that exercise long-running thread churn, cross-thread
  frees, mixed allocation sizes, high alignment, fallback, and `realloc`.

## Phase 1: safety hardening

Completed. Diagnostics counters, debug header validation, hidden-header
alignment tests, System fallback diagnostics, free-after-allocating-thread-exit
coverage, cross-shard service-token stress, TLS-destructor allocation fallback,
and release-mode invalid-header policy documentation are implemented.

## Phase 2: complete the `GlobalAlloc` surface

Completed. `alloc_zeroed`, explicit allocate-copy-free `realloc`, named
`GlobalAllocatorConfig`, `with_config`, and `default_hosted` are implemented and
covered by focused tests. Future diagnostics extensions should be driven by
Phase 4 stress results rather than speculative API surface.

## Phase 3: memory lifecycle and growth policy

Completed for the first production milestone. Shard growth policy is
deterministic and covered by tests: token-pressure shards use configured
`region_spans`, large-request shards grow to at least the required span count,
and requests beyond the wfspan limit use System fallback. Diagnostics include
`largest_shard_spans`. Shards and their backing regions are intentionally
retained for process lifetime; reclaim/shrink remains a future project because
it requires proving no live allocation references a shard and no thread holds a
token to it.

## Phase 4: verification and testing

Completed for the current production candidate. Coverage includes manual
`GlobalAlloc` calls, `#[global_allocator]` std collection workloads, thread
churn, channel handoff, cross-thread frees, high alignment, System fallback,
`alloc_zeroed`, `realloc`, deterministic shard growth, and an explicitly
ignored longer soak workload.

Verified command matrix:

- `cargo fmt`
- `cargo check --no-default-features`
- `cargo check --all-features`
- `cargo test`
- `cargo test --all-features`
- `cargo test --features global --test global_allocator_std -- --ignored`
- `cargo run --features global --example global_wrapper`

Miri/sanitizer work remains optional follow-up for isolated helper paths; the
threaded asm backend remains native-test only.

## Phase 5: stabilization

Completed for the current production candidate. The stable feature is `global`;
`experimental-global` remains as a compatibility alias. README, unsafe audit,
progress documentation, examples, and tests use the stable feature name and
document the production contract: wfspan-served operations retain core bounds,
while shard creation, TLS lifecycle, service-token frees, and System fallback
are hosted support paths. System fallback is an intentional part of this hybrid
std global allocator; a future pure-wfspan mode can be added separately if
needed.

## Non-goals for the first production milestone

- Reclaiming or shrinking shards.
- Making shard creation wait-free.
- Making System fallback wait-free.
- Supporting targets not already supported by the core atomic backend.
- Replacing the core fixed-participant wfspan model with a dynamically resized
  core metadata table.

