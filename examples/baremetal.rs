//! Bare-metal simulation: runtime CPU count with a static backing region.
//!
//! This example shows the pattern for no_std / bare-metal targets where:
//!  - `MAX_N` (the SoC hardware limit) is a compile-time constant.
//!  - The actual number of active CPU cores is discovered at runtime
//!    (e.g., by reading a hardware register or parsing a device tree).
//!  - Backing memory is a `static mut` aligned buffer — no heap allocator
//!    or OS memory service is needed before the allocator is initialized.
//!  - `token_from_raw(cpu_id)` maps each core's hardware ID directly to a
//!    token, avoiding the FAA-based `register_thread()` call.
//!
//! To port this to a real no_std target:
//!  - Replace `std::thread::spawn` with your RTOS task-spawn / secondary-core
//!    kick sequence (e.g., writing to a boot-address register on ARM).
//!  - Replace `println!` with your platform debug output, or remove it.
//!  - Replace the body of `detect_cpu_count()` with a read of MPIDR_EL1,
//!    the ACPI MADT, a Device Tree `/cpus` node, or similar.
//!  - Build with `--no-default-features` to drop the std dependency from
//!    the wf_alloc crate itself.
//!
//! Run with: `cargo run --example baremetal`

use core::alloc::Layout;

use wf_alloc::size_class::{blocks_per_span, class_to_size};
use wf_alloc::{
    HELP_BUDGET_H, LOCAL_SPAN_LIMIT_K, SPAN_SIZE, StepCounter, WfSpanAllocator,
};

// ── Compile-time constants ─────────────────────────────────────────────────────

/// Maximum CPU cores this SoC can have (fixed by the hardware specification).
const MAX_N: usize = 8;

/// Number of supported size classes: 16, 32, 64, …, 2048 bytes.
const C: usize = 8;

/// Number of spans reserved in the static backing region.
const NUM_SPANS: usize = 32;

// ── Static backing memory ──────────────────────────────────────────────────────
//
// `#[repr(align(65536))]` satisfies the SPAN_ALIGN requirement so that the
// span-header recovery mask (`ptr & !(SPAN_SIZE - 1)`) works correctly.
//
// In production, back this with a linker-script section (.heap) or a
// platform-provided physical memory region with the same alignment guarantee.

#[repr(align(65536))]
struct AlignedRegion([u8; NUM_SPANS * SPAN_SIZE]);

static mut REGION: AlignedRegion = AlignedRegion([0u8; NUM_SPANS * SPAN_SIZE]);

// ── Allocator ─────────────────────────────────────────────────────────────────
//
// `WfSpanAllocator::new()` is `const fn`, so this static is placed in .bss
// with no runtime constructor.  `ALLOC.init()` must be called once at boot
// before any core calls `alloc_with_token` or `dealloc_with_token`.

static ALLOC: WfSpanAllocator<MAX_N, C> = WfSpanAllocator::new();

// ── Runtime CPU detection ──────────────────────────────────────────────────────

/// Returns the number of CPU cores available on this boot.
///
/// Replace this body with a hardware-specific read:
/// - ARM64: read `MPIDR_EL1` and cluster topology registers
/// - x86:   `CPUID` leaf 0xB or ACPI MADT processor count
/// - RISC-V: `mhartid` enumeration or Device Tree `/cpus` child count
fn detect_cpu_count() -> usize {
    4 // Simulated: 4 out of MAX_N=8 cores are present on this board variant.
}

// ── Boot entry point ───────────────────────────────────────────────────────────

fn main() {
    let actual_n = detect_cpu_count();
    assert!(
        actual_n <= MAX_N,
        "hardware reported more CPUs ({actual_n}) than MAX_N ({MAX_N})"
    );

    // Initialize the allocator once at boot, before any core uses it.
    // Safety: called exactly once; both ALLOC and REGION are static and
    // therefore never move for the lifetime of the program.
    unsafe {
        ALLOC.init((&raw mut REGION.0).cast::<u8>(), NUM_SPANS * SPAN_SIZE);
    }

    println!(
        "Boot: {actual_n}/{MAX_N} CPUs active, {NUM_SPANS} spans ({} KiB) available",
        NUM_SPANS * SPAN_SIZE / 1024,
    );

    // Spawn one thread per active CPU core.
    // On bare-metal, replace std::thread::spawn with your secondary-core kick
    // (e.g., writing the entry-point address to a platform boot register).
    let mut handles = Vec::with_capacity(actual_n);
    for cpu_id in 0..actual_n {
        handles.push(std::thread::spawn(move || {
            core_main(cpu_id, actual_n);
        }));
    }

    // Wait for all simulated cores to finish.
    // On bare-metal you would instead spin on a shared atomic completion flag.
    for h in handles {
        h.join().unwrap();
    }

    println!("All {actual_n} cores finished, ok");
}

// ── Per-core entry point ───────────────────────────────────────────────────────

fn core_main(cpu_id: usize, active_cores: usize) {
    // Build a token directly from the CPU hardware ID.
    // This avoids the `register_thread()` FAA; the contract is that cpu_id is
    // unique per running core and never shared between two concurrent callers.
    //
    // Safety: cpu_id < MAX_N; each cpu_id is used by exactly one hardware core.
    let token = unsafe { ALLOC.registry.token_from_raw(cpu_id) };

    // Choose a size class that varies across cores to exercise multiple classes.
    let class = cpu_id % C;
    let layout = Layout::from_size_align(class_to_size(class), 8).unwrap();
    let bps = blocks_per_span(class_to_size(class));

    for _ in 0..200 {
        let mut step = StepCounter::new();
        // Safety: token is exclusively owned by this core.
        let p = unsafe { ALLOC.alloc_with_token_counted(layout, token, &mut step) };

        if p.is_null() {
            // The fixed pool is exhausted.  On bare-metal this is a valid
            // outcome — return an error to the caller or retry later.
            continue;
        }

        // Verify that this single allocation stayed within the wait-freedom
        // step bounds derived in the paper (N = active_cores here).
        step.assert_bounds(
            active_cores,
            HELP_BUDGET_H,
            active_cores,
            bps,
            LOCAL_SPAN_LIMIT_K,
        );

        // Write a recognizable pattern and read it back to confirm the block
        // is exclusively owned (no concurrent overwrite from another core).
        // Safety: the pointer is valid and exclusively owned until dealloc.
        unsafe {
            (p as *mut u64).write(cpu_id as u64);
            assert_eq!(
                (p as *const u64).read(),
                cpu_id as u64,
                "memory pattern corrupted — possible double-allocation"
            );
            ALLOC.dealloc_with_token(p, layout, token);
        }
    }
}
