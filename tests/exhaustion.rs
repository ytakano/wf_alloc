//! Exhaustion behavior: null on empty pool, full recovery after frees.

use std::alloc::Layout;

use wf_alloc::WfSpanAllocator;
use wf_alloc::region::OwnedRegion;
use wf_alloc::size_class::blocks_per_span;
use wf_alloc::class_to_size;

const N: usize = 2;
const C: usize = 2;

#[test]
fn exhaust_free_exhaust_again() {
    let region = OwnedRegion::new(3);
    let alloc = Box::leak(Box::new(WfSpanAllocator::<N, C>::new()));
    // SAFETY: init once before sharing; leaked box never moves.
    unsafe { alloc.init(region.ptr(), region.len()) };
    let token = alloc.register_thread().unwrap();
    let bs = class_to_size(0);
    let layout = Layout::from_size_align(bs, 8).unwrap();
    let capacity = 3 * blocks_per_span(bs);

    for round in 0..3 {
        let mut ptrs = Vec::new();
        // Allocate until exhaustion; must serve the full capacity.
        loop {
            // SAFETY: registered token, single thread.
            let p = unsafe { alloc.alloc_with_token(layout, token) };
            if p.is_null() {
                break;
            }
            // Write a pattern to catch double-allocation.
            // SAFETY: p points to bs >= 8 writable bytes.
            unsafe { (p as *mut u64).write(round as u64 ^ p as u64) };
            ptrs.push(p);
        }
        assert_eq!(ptrs.len(), capacity, "round {round}: capacity shrank");

        for &p in &ptrs {
            // SAFETY: pattern written above must be intact.
            unsafe {
                assert_eq!((p as *const u64).read(), round as u64 ^ p as u64);
            }
            // SAFETY: freed once.
            unsafe { alloc.dealloc_with_token(p, layout, token) };
        }
    }
    // SAFETY: quiescent.
    unsafe { wf_alloc::verify::check_quiescent(alloc) };
}
