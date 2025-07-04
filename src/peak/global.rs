/// Tracks the program's peak memory and current in use memory.
use std::{
    alloc::{GlobalAlloc, Layout},
    sync::atomic::{AtomicUsize, Ordering},
};

/// Atomic used to track the maximum amount of memory allocated by the program.
static GLOBAL_PEAK: AtomicUsize = AtomicUsize::new(0);

/// Atomic used to track the number of allocations
static ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Atomic used to track total number of bytes allocated
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);

/// Atomic used to track total number of bytes deallocated
static DEALLOCATED: AtomicUsize = AtomicUsize::new(0);

/// Returns the peak bytes usage for the whole program.
pub fn peak() -> usize {
    GLOBAL_PEAK.load(Ordering::Relaxed)
}

/// Returns the current number of bytes allocated.
///
/// This does not take into account padding added by the underlying allocator,
/// it only accounts for the memory requested and used by the application.
pub fn in_use() -> usize {
    // Saturating sub because this read is racing with the alloc/dealloc calls
    allocated().saturating_sub(deallocated())
}

/// Returns the current number of alloc calls.
pub fn alloc_calls() -> usize {
    ALLOC_CALLS.load(Ordering::Relaxed)
}

/// Returns the current number of bytes allocated.
///
/// This does not take into account padding added by the underlying allocator,
/// it only accounts for the memory requested and used by the application.
pub fn allocated() -> usize {
    ALLOCATED.load(Ordering::Relaxed)
}

/// Returns the current number of bytes deallocated.
pub fn deallocated() -> usize {
    DEALLOCATED.load(Ordering::Relaxed)
}

/// Resets the global peak to the current memory in use.
pub fn reset_peak() {
    GLOBAL_PEAK.store(in_use(), Ordering::Relaxed);
}

pub struct GlobalPeakTracker<T> {
    inner: T,
}

impl<T> GlobalPeakTracker<T> {
    /// Creates a new allocator.
    pub const fn init(inner: T) -> Self {
        Self { inner }
    }

    /// Returns a reference to the wrapped allocator.
    pub fn inner(&self) -> &T {
        &self.inner
    }

    /// Returns a mutable reference to the wrapped allocator.
    pub fn inner_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    /// Resets the global peak.
    pub fn reset_peak(&self) {
        reset_peak()
    }

    /// Returns the peak memory usage.
    pub fn peak(&self) -> usize {
        peak()
    }

    /// Returns the current number of alloc calls.
    pub fn alloc_calls(&self) -> usize {
        alloc_calls()
    }

    /// Returns the current number of bytes allocated.
    pub fn allocated(&self) -> usize {
        allocated()
    }

    /// Returns the current number of bytes deallocated.
    pub fn deallocated(&self) -> usize {
        deallocated()
    }

    /// Returns the current memory usage.
    pub fn in_use(&self) -> usize {
        in_use()
    }
}

unsafe impl<T: GlobalAlloc> GlobalAlloc for GlobalPeakTracker<T> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // NOTE: The code below doesn't need to use the `StateGuard` because it does no allocations
        let ret = unsafe { self.inner.alloc(layout) };

        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);

        if !ret.is_null() {
            let size = layout.size();
            ALLOCATED.fetch_add(size, Ordering::Relaxed);
            GLOBAL_PEAK.fetch_max(in_use(), Ordering::Relaxed);
        }
        ret
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // NOTE: The code below doesn't need to use the `StateGuard` because it does no allocations
        unsafe {
            self.inner.dealloc(ptr, layout);
        }
        let size = layout.size();
        DEALLOCATED.fetch_add(size, Ordering::Relaxed);
    }
}
