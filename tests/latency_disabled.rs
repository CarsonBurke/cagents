//! Dedicated binary isolates the allocator probe from the application tests.
//! Servo owns the process allocator, so this probe runs in the isolated CLI,
//! host, and browserless UI lanes rather than the integrated-browser lane.
//! The opt-in latency harness intentionally uses an allocating recorder.
#![cfg(not(any(feature = "integrated-browser", feature = "latency-harness")))]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

use cagents::performance::{Action, InteractionLatency};

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

struct CountingAllocator;

fn count_allocation() {
    if TRACK.try_with(Cell::get).unwrap_or(false) {
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
    }
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count_allocation();
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count_allocation();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count_allocation();
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[test]
fn disabled_recorder_constructs_commits_and_abandons_without_allocations_or_clocks() {
    ALLOCATIONS.with(|count| count.set(0));
    TRACK.with(|track| track.set(true));
    // Exercise the same selected type and default construction as the app,
    // including any allocation introduced by its factory or feature gating.
    let mut recorder = std::hint::black_box(InteractionLatency::default());
    assert_eq!(std::mem::size_of::<InteractionLatency>(), 0);
    assert_eq!(recorder.pending_generation(), None);
    let mut clock_reads = 0;
    for action in [Action::CardFocus, Action::PaletteOpen, Action::PaletteQuery] {
        let result = recorder.commit_with_clock(std::hint::black_box(action), || {
            clock_reads += 1;
            std::time::Instant::now()
        });
        assert_eq!(std::hint::black_box(result), None);
        assert_eq!(recorder.commit(std::hint::black_box(action)), None);
        assert_eq!(recorder.pending_generation(), None);
        recorder.abandon_pending();
        assert_eq!(recorder.pending_generation(), None);
    }
    TRACK.with(|track| track.set(false));
    assert_eq!(clock_reads, 0);
    assert_eq!(ALLOCATIONS.with(Cell::get), 0);
}
