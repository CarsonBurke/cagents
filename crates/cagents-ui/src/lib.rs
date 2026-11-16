//! The desktop application: windows, views, widgets and the state behind them.
//! The shared library `cagents` holds what the CLI and the daemon also use.

// Native hot patches keep thread-local storage in unchanged libraries.
// Both roots include one module list; application types are never mixed.
include!("modules.rs");

cagents::test_binary_setup!();

/// Live heap bytes for measurements that ask what a structure really holds,
/// such as what a hidden chat keeps warm. Counting is off unless a
/// measurement turns it on, so ordinary tests pay one relaxed load per
/// allocation. The count is process-wide: read it from a measurement run
/// alone.
#[cfg(all(test, not(feature = "integrated-browser")))]
pub(crate) mod test_heap {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

    static COUNTING: AtomicBool = AtomicBool::new(false);
    static LIVE: AtomicIsize = AtomicIsize::new(0);

    pub(crate) struct Counting;

    fn count(bytes: isize) {
        if COUNTING.load(Ordering::Relaxed) {
            LIVE.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    // SAFETY: every call is forwarded unchanged to the system allocator.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let pointer = unsafe { System.alloc(layout) };
            if !pointer.is_null() {
                count(layout.size() as isize);
            }
            pointer
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let pointer = unsafe { System.alloc_zeroed(layout) };
            if !pointer.is_null() {
                count(layout.size() as isize);
            }
            pointer
        }
        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            unsafe { System.dealloc(pointer, layout) };
            count(-(layout.size() as isize));
        }
        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            let moved = unsafe { System.realloc(pointer, layout, size) };
            if !moved.is_null() {
                count(size as isize - layout.size() as isize);
            }
            moved
        }
    }

    /// Start counting from zero. Frees of memory allocated before this are
    /// counted too, so compare two readings taken while counting.
    pub(crate) fn start() {
        LIVE.store(0, Ordering::Relaxed);
        COUNTING.store(true, Ordering::Relaxed);
    }

    pub(crate) fn live() -> isize {
        LIVE.load(Ordering::Relaxed)
    }

    pub(crate) fn stop() {
        COUNTING.store(false, Ordering::Relaxed);
    }
}

#[cfg(all(test, not(feature = "integrated-browser")))]
#[global_allocator]
static TEST_HEAP: test_heap::Counting = test_heap::Counting;
