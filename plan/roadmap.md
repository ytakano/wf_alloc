# Roadmap for Implementing wfspan in Rust with a Coding Agent

The paper’s implementation target is not “a slightly lock-free malloc,” but a **wait-free allocator built around span ownership transfer, non-linearizable wait-free lists, and a helping protocol**. The core tradeoff is clear: wfspan gives up strict linearizability inside allocator-internal lists in exchange for **bounded execution steps** for allocation and deallocation, while accepting an increased but still bounded worst-case memory footprint. 

---

# 1. Reading wfspan as an implementation design

## What problem does wfspan solve?

Modern lock-free allocators often perform well, but lock-free progress does not guarantee that every individual thread finishes in a bounded number of steps. A slow thread can keep losing CAS or LL/SC attempts and starve. That is a problem for real-time embedded systems.

wfspan addresses this by building a **wait-free dynamic memory allocator**. In other words, both `alloc` and `free` must finish within bounded execution steps.

The paper’s Table 1 summarizes the positioning:

```text
mimalloc / snmalloc:
  lock-free
  good average performance
  but unbounded worst-case execution time

wfspan:
  wait-free
  SPMC/MPSC wait-free lists
  bounded worst-case execution time:
    O(N^2), or O(N) with a different helping protocol
```

## Main design idea

wfspan is based on:

```text
per-thread heaps
per-size-class spans
local free-lists
remote MPSC free-lists
public SPMC span-lists
bounded helping
```

The key allocator-level trick is:

```text
Non-linearizable wait-free lists are acceptable inside the allocator
as long as any “extra” unavailable memory is bounded.
```

For example, in an ordinary queue, one dequeue request removing two nodes would be incorrect. In wfspan, this can be tolerated because the extra span can be kept in a help record and reused by the same thread on a later allocation. This breaks linearizability, but allocator correctness is still preserved.

## High-level architecture

wfspan has no single global heap. Instead, each thread owns a heap.

Each thread heap has, for each size class:

```text
1. a private local span-list
2. a public SPMC wait-free span-list
```

The local span-list is used as a thread-local allocation buffer. The public SPMC span-list is visible to other threads, so they can steal or acquire spans when their own heap lacks a suitable span.

Each span contains:

```text
span header
same-size memory blocks
local free-list
remote MPSC free-list
owner thread id
local free block count
global/free count
size class
block size
```

Remote frees go into the span’s MPSC free-list. Local allocation and local free use the owner’s local free-list.

---

# 2. Rust implementation strategy

Do **not** start by implementing a production `malloc` replacement. Start with a faithful, testable prototype.

The staged target should be:

```text
Phase A: no_std-friendly core allocator
Phase B: std-based test harness
Phase C: optional GlobalAlloc wrapper
Phase D: benchmarks and WCET-style measurements
```

The first public API should be explicit-token based:

```rust
pub struct WfSpanAllocator<const N: usize, const C: usize>;

impl<const N: usize, const C: usize> WfSpanAllocator<N, C> {
    pub unsafe fn alloc_with_token(
        &self,
        layout: core::alloc::Layout,
        token: ThreadToken,
    ) -> *mut u8;

    pub unsafe fn dealloc_with_token(
        &self,
        ptr: *mut u8,
        layout: core::alloc::Layout,
        token: ThreadToken,
    );
}
```

Do **not** implement `GlobalAlloc` first. `GlobalAlloc` hides thread identity and makes bounded thread registration harder. Add it only after the core allocator works.

---

# 3. Crate layout

Ask the coding agent to create this structure first:

```text
wfspan-rs/
  Cargo.toml
  src/
    lib.rs
    config.rs
    align.rs
    atomic_backend.rs
    tagged.rs
    heap.rs
    thread.rs
    size_class.rs
    pagemap.rs
    span.rs
    block.rs
    local_list.rs
    remote_mpsc.rs
    spmc_span_list.rs
    help_record.rs
    acquire.rs
    allocator.rs
    global.rs
    stats.rs
  tests/
    sequential.rs
    remote_free.rs
    spmc_basic.rs
    helping_small.rs
    concurrent_smoke.rs
    exhaustion.rs
  benches/
    alloc_free.rs
    remote_free.rs
    wcet_like.rs
  docs/
    wfspan-model.md
    invariants.md
    progress.md
    memory-footprint.md
    unsafe-audit.md
```

Suggested Cargo features:

```toml
[features]
default = ["std"]
std = []
no_std = []
global = []
stats = []
loom = []
nightly = []
```

---

# 4. Fixed implementation parameters

The paper evaluates wfspan with:

```text
SPAN_SIZE = 64 KiB
K = 40
H = 1
P = N
```

For the Rust prototype, start with:

```rust
pub const SPAN_SIZE: usize = 64 * 1024;
pub const SPAN_ALIGN: usize = SPAN_SIZE;
pub const MAX_THREADS: usize = 64;
pub const SIZE_CLASSES: usize = 64;
pub const LOCAL_SPAN_LIMIT_K: usize = 40;
pub const HELP_BUDGET_H: usize = 1;
pub const QUERY_LIMIT_P: usize = MAX_THREADS;
pub const MIN_BLOCK_SIZE: usize = 16;
```

Prefer `const generics` for the final allocator:

```rust
WfSpanAllocator<const N: usize, const C: usize>
```

where:

```text
N = maximum number of participating threads
C = number of size classes
```

---

# 5. Milestone 0: Documentation before implementation

The first PR should mostly be documentation.

Create:

```text
docs/wfspan-model.md
docs/invariants.md
docs/progress.md
docs/memory-footprint.md
docs/unsafe-audit.md
```

`docs/wfspan-model.md` must define:

```text
N: maximum number of threads
C: number of size classes
S: span size
K: per-thread private span limit
H: helping budget
P: span-list query limit
block
span
thread heap
help record
MPSC remote free-list
SPMC span-list
```

It must also explain:

```text
why non-linearizability is acceptable
how allocation remains bounded
how deallocation remains bounded
how extra memory footprint is bounded
target architecture assumptions
```

`docs/progress.md` should translate the paper’s progress claims into implementation-level constraints:

```text
try_pop_head_once:
  O(1), exactly one pop attempt

published helping request:
  O(((N - 1) / H) * (N - 1) + 1)

spanlists_acquire_span:
  O(N^2)

deallocation:
  O(1)
```

Initial out-of-scope items:

```text
realloc
full malloc ABI
OS mmap/sbrk backend
huge allocations
NUMA
cross-process allocator
hard real-time certification
```

Acceptance criteria:

```text
docs/wfspan-model.md exists
docs/invariants.md exists
docs/progress.md exists
docs/memory-footprint.md exists
docs/unsafe-audit.md exists
target assumptions are explicit
unsupported features are explicit
```

---

# 6. Milestone 1: Span and sequential allocator

Start without concurrency. Get the memory layout right first.

## Core types

```rust
#[repr(C, align(64))]
pub struct SpanHeader {
    size_class: usize,
    block_size: usize,
    block_count: usize,
    owner: core::sync::atomic::AtomicUsize,
    free_count: core::sync::atomic::AtomicIsize,

    // owner-thread only
    local_free: LocalFreeList,
    local_free_count: usize,

    // remote-free path
    remote_free: RemoteMpscFreeList,

    state: core::sync::atomic::AtomicUsize,
}

#[repr(C)]
pub struct Block {
    next: core::sync::atomic::AtomicPtr<Block>,
}
```

Prefer splitting metadata into cache-line-sized regions:

```rust
#[repr(C, align(64))]
struct LocalMeta {
    local_free: LocalFreeList,
    local_free_count: usize,
    block_size: usize,
}

#[repr(C, align(64))]
struct RemoteMeta {
    remote_free: RemoteMpscFreeList,
    free_count: AtomicIsize,
}
```

## Required functions

```rust
unsafe fn init_span(
    span: NonNull<u8>,
    size_class: usize,
    block_size: usize,
    owner: usize,
);

unsafe fn span_from_ptr(ptr: *mut u8) -> *mut SpanHeader;

unsafe fn block_payload(block: *mut Block) -> *mut u8;

unsafe fn block_from_payload(ptr: *mut u8) -> *mut Block;
```

Because spans are `SPAN_SIZE`-aligned, `span_from_ptr` can use masking:

```rust
fn span_base(ptr: *mut u8) -> *mut SpanHeader {
    let addr = ptr as usize;
    let base = addr & !(SPAN_SIZE - 1);
    base as *mut SpanHeader
}
```

For this milestone, only implement local allocation/free:

```rust
unsafe fn alloc_from_local_span(span: *mut SpanHeader) -> *mut u8;

unsafe fn dealloc_to_local_span(span: *mut SpanHeader, ptr: *mut u8);
```

Acceptance criteria:

```text
one span can be initialized
block_count allocations succeed
the next allocation returns null
deallocated blocks can be reallocated
span_from_ptr is correct
Miri passes sequential tests
```

---

# 7. Milestone 2: Size classes and fixed heap

wfspan uses a segregated-fit strategy. For the first Rust prototype, support only small allocations below `SPAN_SIZE`.

Implement:

```rust
pub fn size_to_class(size: usize, align: usize) -> Option<usize>;

pub fn class_to_size(class: usize) -> usize;
```

Start with simple power-of-two classes:

```text
16, 32, 64, 128, 256, ...
```

Later, this can be replaced by a table closer to snmalloc/supermalloc-style size classes.

Acceptance criteria:

```text
size 1, align 1 => 16-byte class
size 17, align 1 => 32-byte class
alignment greater than size is handled
SPAN_SIZE or larger returns UnsupportedLarge
```

---

# 8. Milestone 3: Atomic backend and tagged head word

The SPMC span-list needs ABA protection. The paper uses LL/SC where available, and versioned CAS2 on x86.

Do **not** implement this as two independent atomics:

```rust
AtomicPtr<Node>
AtomicUsize version
```

That is not enough. The pointer and version must be updated atomically as one logical value.

Define:

```rust
#[repr(C, align(16))]
#[derive(Copy, Clone)]
pub struct HeadWord {
    pub ptr: usize,
    pub version: usize,
}
```

Define a backend trait:

```rust
pub trait Cas2Backend {
    unsafe fn load(head: *const HeadWord) -> HeadWord;

    unsafe fn compare_exchange(
        head: *mut HeadWord,
        current: HeadWord,
        new: HeadWord,
    ) -> Result<HeadWord, HeadWord>;
}
```

Implement backends in this order:

```text
1. x86_64 cmpxchg16b backend
2. cfg(target_has_atomic = "128") AtomicU128 backend, if usable
3. loom/test backend for model testing
```

Acceptance criteria:

```text
HeadWord is 16-byte aligned
CAS2 success test passes
CAS2 failure test passes
version increments prevent ABA
unsupported platforms fail clearly or require a feature flag
```

---

# 9. Milestone 4: MPSC remote free-list

This corresponds to the paper’s MPSC wait-free free-list.

Remote free-list push:

```text
1. set new node's next to UNLINKED
2. SWAP head with new node
3. link new node to old head
```

The temporary `UNLINKED` state is not a bug. It is part of the non-linearizable design.

## Rust shape

```rust
const UNLINKED: *mut Block = usize::MAX as *mut Block;

pub struct RemoteMpscFreeList {
    head: AtomicPtr<Block>,
}

impl RemoteMpscFreeList {
    pub unsafe fn push(&self, block: *mut Block) {
        (*block).next.store(UNLINKED, Ordering::Relaxed);
        let old = self.head.swap(block, Ordering::AcqRel);
        (*block).next.store(old, Ordering::Release);
    }

    pub fn reclaim_all(&self) -> *mut Block {
        self.head.swap(core::ptr::null_mut(), Ordering::AcqRel)
    }
}
```

Reclaiming the head is `O(1)`. Converting the reclaimed list into a local free-list is bounded by the number of blocks in the span:

```rust
unsafe fn append_remote_to_local_bounded(
    span: *mut SpanHeader,
    head: *mut Block,
) -> usize {
    // at most span.block_count steps
}
```

If `UNLINKED` is encountered, stop and delay reuse of that span.

Acceptance criteria:

```text
remote push has no loop
reclaim has no loop
append is bounded by block_count
producer-stopped-after-SWAP scenario is tested
blocked memory remains bounded to span-level effects
```

---

# 10. Milestone 5: SPMC span-list enqueue and one-shot pop

Each public span-list is:

```text
single-producer
multi-consumer
```

The owner thread is the only producer. Therefore enqueue does not need a CAS.

## Types

```rust
#[repr(C)]
pub struct SpanNode {
    next: AtomicPtr<SpanNode>,
    span: *mut SpanHeader,
}

pub struct SpmcSpanList {
    head: UnsafeCell<HeadWord>,       // pointer + version, updated by consumers
    tail: UnsafeCell<*mut SpanNode>,  // producer only
}
```

## API

```rust
pub enum TryPop {
    Span(*mut SpanHeader),
    Empty,
    Failed,
}

impl SpmcSpanList {
    pub unsafe fn enqueue_by_owner(&self, node: *mut SpanNode);

    pub unsafe fn try_pop_head_once<B: Cas2Backend>(&self) -> TryPop;
}
```

The important rule:

```text
try_pop_head_once may perform exactly one CAS2/LLSC attempt.
It must not retry in a loop.
```

Acceptance criteria:

```text
enqueue_by_owner uses no CAS
try_pop_head_once performs exactly one atomic update attempt
Empty, Failed, and Span are distinct outcomes
ABA/version behavior is tested
there is no retry loop
```

---

# 11. Milestone 6: HelpRecord and helping protocol

This is the most important milestone.

The implementation should mirror the paper’s Algorithms 2, 3, and 4, but with Rust-level bounded loops.

## HelpRecord encoding

The paper encodes a pending flag in the low bit. Do the same with `AtomicUsize`.

```rust
#[derive(Copy, Clone)]
pub struct EncodedReq(usize);

impl EncodedReq {
    pub fn pending(phase: usize) -> Self {
        Self((phase << 1) | 1)
    }

    pub fn done_with_span(span: *mut SpanHeader) -> Self {
        debug_assert_eq!((span as usize) & 1, 0);
        Self(span as usize)
    }

    pub fn empty() -> Self {
        Self(0)
    }

    pub fn is_pending(self) -> bool {
        self.0 & 1 == 1
    }

    pub fn phase(self) -> usize {
        self.0 >> 1
    }

    pub fn span(self) -> *mut SpanHeader {
        (self.0 & !1) as *mut SpanHeader
    }
}

pub struct HelpRecord {
    phase_pending_or_span: AtomicUsize,
    last_phase: AtomicUsize,
}
```

## Help table

```rust
pub struct HelpTable<const N: usize, const C: usize> {
    records: [[HelpRecord; C]; N],
}
```

Avoid `Vec` in the allocator core. Use arrays, `MaybeUninit`, or static initialization.

## Bounded help operation

The agent must not blindly translate the paper’s `while` loops into unbounded Rust loops. Each helper call should be one-shot or explicitly bounded.

Sketch:

```rust
unsafe fn help_finishing_req<B: Cas2Backend>(
    list: &SpmcSpanList,
    req: &HelpRecord,
    held_span: &mut *mut SpanHeader,
    list_is_null: &mut bool,
) {
    let start = req.phase_pending_or_span.load(Ordering::Acquire);
    let start_req = EncodedReq(start);

    if !start_req.is_pending() {
        return;
    }

    let phase = start_req.phase();

    let now = EncodedReq(req.phase_pending_or_span.load(Ordering::Acquire));
    if !now.is_pending() || now.phase() != phase {
        return;
    }

    if held_span.is_null() {
        match list.try_pop_head_once::<B>() {
            TryPop::Span(s) => *held_span = s,
            TryPop::Empty => {
                *list_is_null = true;
                return;
            }
            TryPop::Failed => return,
        }
    }

    let expected = EncodedReq::pending(phase).0;
    let desired = EncodedReq::done_with_span(*held_span).0;

    if req
        .phase_pending_or_span
        .compare_exchange(
            expected,
            desired,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
    {
        *held_span = core::ptr::null_mut();
    }
}
```

Then implement:

```rust
unsafe fn spanlists_acquire_span<
    B: Cas2Backend,
    const N: usize,
    const C: usize,
>(
    allocator: &WfSpanAllocator<N, C>,
    heap_id: usize,
    size_class: usize,
) -> *mut SpanHeader;
```

Required flow:

```text
1. Reclaim any span already stored in this thread's HelpRecord.
2. Publish a new pending request.
3. Help at most H other pending requests.
4. Traverse at most P public span-lists.
5. Finish this thread's own request.
6. If no span is acquired, clear the pending request.
7. Save cur_query and helping_pos for the next call.
```

Acceptance criteria:

```text
help_count < H
help_query < P
CAS2 attempts are counted
a request can be completed by another thread
the one-request-two-spans case is tested
a span left in a help record is reclaimed on the next acquire
```

---

# 12. Milestone 7: Per-thread heap

## Types

```rust
pub struct ThreadHeap<const C: usize> {
    local_spans: [LocalSpanList; C],
    public_spans: [SpmcSpanList; C],
    cur_query: [AtomicUsize; C],
    helping_pos: [AtomicUsize; C],
    local_span_count: AtomicUsize,
}

pub struct ThreadToken {
    id: usize,
}
```

Core allocator API should require an explicit `ThreadToken`.

```rust
impl ThreadRegistry {
    pub fn register_current(&self) -> Option<ThreadToken>;

    pub unsafe fn token_from_cpu_id(&self, cpu_id: usize) -> ThreadToken;
}
```

For `std`, a wrapper may use TLS later:

```rust
thread_local! {
    static THREAD_TOKEN: ThreadToken = ...;
}
```

But for RTOS/kernel-style use, explicit tokens are better.

Acceptance criteria:

```text
registration fails after MAX_THREADS
token id is always less than N
no_std mode does not depend on TLS
std feature may provide a thread-local wrapper
```

---

# 13. Milestone 8: Allocation path

Implement `alloc_with_token`.

Allocation flow:

```text
1. Convert Layout to size class.
2. Try the current thread heap's local non-empty span.
3. Pop from the local free-list.
4. If the local free-list is empty, commit local_free_count to free_count via FAA.
5. Try to reclaim the remote free-list.
6. If the span is truly empty, discard it.
7. If no local span is available, acquire a span from other heaps' SPMC span-lists.
8. If no non-empty span is available, acquire a full/raw span.
9. If using the fixed backend and no span is available, return null.
```

The first implementation should use a fixed pre-provisioned heap:

```rust
pub struct FixedSpanPool {
    base: NonNull<u8>,
    span_count: usize,
    raw_spans: RawSpanPool,
}
```

Do not use `mmap` or OS allocation in the wait-free path for the prototype.

Acceptance criteria:

```text
local allocation avoids synchronization where possible
slow path calls bounded span acquisition
exhaustion returns null
every path updates StepCounter
no unbounded retry loop exists
```

---

# 14. Milestone 9: Deallocation path

Implement `dealloc_with_token`.

```rust
pub unsafe fn dealloc_with_token(
    &self,
    ptr: *mut u8,
    layout: Layout,
    token: ThreadToken,
) {
    let span = span_from_ptr(ptr);
    let owner = (*span).owner.load(Ordering::Acquire);

    if owner == token.id {
        dealloc_local(span, ptr);
    } else {
        dealloc_remote(span, ptr, token);
    }
}
```

Remote deallocation:

```rust
unsafe fn dealloc_remote(
    span: *mut SpanHeader,
    ptr: *mut u8,
    token: ThreadToken,
) {
    let block = block_from_payload(ptr);

    (*span).remote.remote_free.push(block);

    let old = (*span)
        .remote
        .free_count
        .fetch_add(1, Ordering::AcqRel);

    if old == 0 {
        // The span was discarded and ownerless.
        try_claim_discarded_span(span, token);
    }
}
```

Acceptance criteria:

```text
owner-thread deallocation uses the local free-list
remote deallocation uses MPSC push + FAA
old == 0 reclaim path is tested
debug builds detect double free if practical
release builds document double free as caller UB
deallocation has no unbounded loop
```

---

# 15. Milestone 10: Span state machine

Translate the paper’s span state diagram into a Rust enum and invariant checker.

```rust
#[repr(usize)]
pub enum SpanState {
    Raw = 0,
    FullLocal = 1,
    NonEmptyLocal = 2,
    FullPublic = 3,
    NonEmptyPublic = 4,
    Discarded = 5,
}
```

Do not rely only on the enum as the source of truth. The real state is determined by:

```text
owner
local free count q
global free count g
maximum block count m
public/local list membership
```

Important invariants:

```text
Raw span has no owner and no initialized size class.
Full span has q + g == m.
Non-empty span has 0 < q + g < m.
Discarded span has no owner.
An owned span belongs to at most one local list.
A public span-list span is public/ownerless.
An allocated block appears in no free-list.
```

Acceptance criteria:

```text
debug_assert_invariants() passes after tests
debug-only list membership tracking exists
public/local double-membership is detected
```

---

# 16. Milestone 11: Memory footprint accounting

wfspan intentionally trades memory footprint for bounded execution time. So the implementation must measure that tradeoff.

Implement:

```rust
pub struct AllocatorStats {
    allocated_spans: AtomicUsize,
    raw_spans: AtomicUsize,
    public_spans: AtomicUsize,
    local_spans: AtomicUsize,
    discarded_spans: AtomicUsize,
    remote_blocked_spans: AtomicUsize,
    help_record_spans: AtomicUsize,
    max_observed_spans: AtomicUsize,
}
```

Compute the theoretical additional memory footprint bound from:

```text
N: thread count
C: size class count
S: span size
P: span-list query limit
```

The paper’s bound is approximately:

```text
A(N) = (N + (ceil(N / P) + N - 1) * (N - 1)) * C * S
```

Acceptance criteria:

```text
theoretical_extra_bound() is implemented
observed span count is tracked
help-record-retained spans are counted
remote-blocked spans are counted
stress tests report footprint statistics
```

---

# 17. Milestone 12: GlobalAlloc wrapper

Only after the core allocator works, add:

```rust
pub struct GlobalWfSpanAllocator<const N: usize, const C: usize> {
    inner: WfSpanAllocator<N, C>,
}

unsafe impl<const N: usize, const C: usize> GlobalAlloc
    for GlobalWfSpanAllocator<N, C>
{
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match self.inner.current_thread_token() {
            Some(token) => self.inner.alloc_with_token(layout, token),
            None => core::ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if let Some(token) = self.inner.current_thread_token() {
            self.inner.dealloc_with_token(ptr, layout, token)
        }
    }
}
```

Global allocator restrictions:

```text
no Box
no Vec
no String
no format!
no println!
no Mutex/RwLock
no recursive allocation
no panicking
```

---

# 18. Milestone 13: Test plan

## Sequential tests

```text
initialize one span
allocate until exhaustion
free all blocks
allocate again
test all size classes
test alignment handling
test unsupported large allocation
test span_from_ptr
```

## MPSC remote free-list tests

```text
many producers push remote blocks
owner reclaims remote list
producer stops after SWAP before linking next
UNLINKED stops bounded consumption
no block is lost
```

## SPMC span-list tests

```text
owner enqueue
multiple consumers one-shot pop
empty detection
CAS2 failure path
ABA/version increment
```

## Helping tests

```text
thread A publishes request, thread B completes it
thread A observes completed request
one request can result in two spans
extra span remains in HelpRecord
HelpRecord span is reclaimed on next acquire
H and P bounds are respected
```

## Concurrent allocator smoke tests

```text
N threads local alloc/free
producer alloc, consumer free
remote-free-heavy workload
mixed-size workload
exhaustion under contention
forced slow producer after MPSC SWAP
forced slow consumer after SPMC pop before HelpRecord CAS
```

## Loom model tests

Use tiny configurations:

```text
N = 2
C = 1
SPAN_SIZE = small
blocks_per_span = 2
K = 1
H = 1
P = 2
```

Check:

```text
same block is never allocated twice
free block is not lost
allocated block is not in any free-list
HelpRecord span remains recoverable
span is not in public and local lists at once
```

---

# 19. Milestone 14: Benchmark plan

Do not benchmark throughput only. wfspan’s value is bounded latency.

Benchmarks:

```text
single-thread alloc/free
N-thread local alloc/free
remote-free-heavy workload
asymmetric allocation pattern
mixed size distribution
exhaustion behavior
forced contention on SPMC span acquisition
worst-case memory footprint pattern
```

Metrics:

```text
ops/sec
p50 latency
p90 latency
p99 latency
p99.9 latency
p99.99 latency
maximum observed latency
maximum CAS2 attempts per allocation
maximum help steps per allocation
maximum query count per allocation
maximum heap span count
help-record-retained spans
remote-blocked spans
```

Compare against:

```text
system allocator
mimalloc
snmalloc
TLSF-like allocator if available
Mutex-protected allocator
```

---

# 20. Hard rules for the coding agent

## Forbidden

```text
unbounded loop { compare_exchange ... }
unbounded while CAS fails { retry }
Treiber stack substitution for the SPMC helping protocol
Vec/Box/String/format!/println! in allocator core
std::sync::Mutex
std::sync::RwLock
parking_lot
OS allocation in the wait-free path
turning the span-list into an ordinary linearizable queue
leaking spans stored in HelpRecord
unsafe blocks without Safety comments
```

## Allowed bounded loops

Only loops with explicit static bounds are allowed:

```text
for i in 0..N
for i in 0..C
for i in 0..P
for i in 0..H
for i in 0..blocks_per_span
```

---

# 21. Biggest implementation risk

The biggest risk is that the coding agent accidentally implements the SPMC span-list as an ordinary lock-free queue.

Bad:

```rust
loop {
    let old = head.load(Ordering::Acquire);
    let new = compute(old);

    if head
        .compare_exchange(
            old,
            new,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
    {
        return item;
    }
}
```

That is lock-free, not wait-free.

wfspan needs this shape instead:

```rust
match try_pop_head_once() {
    TryPop::Span(s) => return s,

    TryPop::Empty => {
        move_to_next_heap();
    }

    TryPop::Failed => {
        publish_request();
        help_bounded();
        finish_own_request_bounded();
    }
}
```

---

# 22. Agent prompt

```text
Implement a Rust prototype of wfspan-style wait-free dynamic memory management.

Read docs/wfspan-model.md first and keep the implementation consistent with the model.

Core requirements:
- Rust.
- no_std-friendly core.
- Optional std test harness.
- Optional GlobalAlloc wrapper only after the core allocator works.
- Explicit ThreadToken-based API first.
- Fixed pre-provisioned heap backend first.
- No OS allocation on the wait-free path.
- No unbounded CAS retry loops.
- No Mutex/RwLock in allocator core.
- No Vec/Box/String/format!/println! in allocator core.
- Every public alloc/dealloc path must update StepCounter.
- Every unsafe block must have a Safety comment.
- Every loop must have a static bound: N, C, P, H, or blocks_per_span.

Implement milestones in this exact order:

1. Documentation
   - docs/wfspan-model.md
   - docs/invariants.md
   - docs/progress.md
   - docs/memory-footprint.md
   - docs/unsafe-audit.md

2. Sequential span allocator
   - SpanHeader
   - Block
   - LocalFreeList
   - span_from_ptr
   - init_span
   - local allocate/free tests

3. Size classes and fixed heap
   - size_to_class
   - class_to_size
   - fixed span pool
   - initialize raw spans

4. Atomic backend
   - HeadWord { ptr, version }
   - Cas2Backend trait
   - x86_64 cmpxchg16b backend or cfg-gated 128-bit backend
   - loom/test backend
   - ABA/version tests

5. MPSC remote free-list
   - push via SWAP then link next
   - reclaim via SWAP to null
   - UNLINKED sentinel
   - producer-halted tests

6. SPMC span-list
   - owner-only enqueue with release store
   - try_pop_head_once with exactly one CAS2/LLSC attempt
   - Empty/Failed/Span results
   - no retry loop

7. Helping protocol
   - HelpRecord encoded in AtomicUsize
   - HelpTable[N][C]
   - help_finishing_req
   - spanlists_acquire_span
   - bounded H/P loops
   - tests for assisted completion and one-request-two-spans case

8. Full alloc/dealloc
   - alloc_with_token
   - dealloc_with_token
   - owner/local free path
   - remote free path
   - discarded span reclaim
   - K-limited local spans
   - public SPMC span publishing

9. Stats and invariant checker
   - StepCounter
   - AllocatorStats
   - debug_assert_invariants
   - theoretical memory footprint bound

10. GlobalAlloc wrapper
   - feature = "global"
   - no reentrant allocation
   - return null on unsupported/exhausted allocation

11. Tests and benchmarks
   - sequential
   - remote-free
   - SPMC
   - helping
   - concurrent smoke
   - loom small model
   - Miri sequential
   - WCET-like latency benchmark
   - memory footprint stress benchmark

The implementation must preserve wfspan's intended design:
- Per-thread heaps.
- Per-size-class local span-list plus public SPMC span-list.
- Span-local MPSC remote free-list.
- Non-linearizable but bounded memory behavior.
- Allocation bounded by spanlists_acquire_span.
- Deallocation O(1) bounded.
```

The most important implementation order is:

```text
remote_mpsc.rs
spmc_span_list.rs
help_record.rs
allocator.rs
```

The allocator itself is not the hardest part. The hard part is preventing the wait-free list and helping protocol from silently degrading into an ordinary lock-free CAS-loop design.

