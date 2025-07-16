use bytesize::ByteSize;
use memory_stats::memory_stats;
use std::{
    alloc::{GlobalAlloc, Layout},
    fmt::Display,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};
use thousands::Separable;

/// Atomic used to track the maximum amount of memory allocated by the program.
static GLOBAL_PEAK: AtomicUsize = AtomicUsize::new(0);

/// Atomic used to track the number of allocations
static ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Atomic used to track total number of bytes allocated
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);

/// Atomic used to track total number of bytes deallocated
static DEALLOCATED: AtomicUsize = AtomicUsize::new(0);

/// Collect memory metrics for the process and from the allocator.
pub fn metrics() -> MemoryMetrics {
    let (physical_mem, virtual_mem) = memory_stats()
        .map(|stats| (Some(stats.physical_mem), Some(stats.virtual_mem)))
        .unwrap_or((None, None));
    MemoryMetrics {
        when: Instant::now(),
        allocated: allocated(),
        deallocated: deallocated(),
        peak: peak(),
        alloc_calls: alloc_calls(),
        physical_mem,
        virtual_mem,
    }
}

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

    /// Collect memory metrics for the process and from the allocator.
    pub fn metrics(&self) -> MemoryMetrics {
        metrics()
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
/// Aggregate of memory metrics.
#[must_use]
pub struct MemoryMetrics {
    /// When the measurement happened.
    pub when: Instant,

    /// Total number of bytes allocated so far.
    pub allocated: usize,

    /// Total number of bytes deallocated so far.
    pub deallocated: usize,

    /// Number of alloc calls.
    pub alloc_calls: usize,

    /// Peak memory usage in bytes.
    ///
    /// Note: The peak memory usage can be reset, this is used to measure
    /// the memore usage in a span of time.
    pub peak: usize,

    /// The current physical memory usage.
    ///
    /// See [memory_stats] for details
    ///
    /// [memory_stats]: https://lib.rs/crates/memory-stats
    pub physical_mem: Option<usize>,

    /// The current virtual memory usage.
    ///
    /// See [memory_stats] for details
    ///
    /// [memory_stats]: https://lib.rs/crates/memory-stats
    pub virtual_mem: Option<usize>,
}

impl Display for MemoryMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, " peak=")?;
        format_bytes_usize(f, self.peak)?;

        write!(f, " in_use=")?;
        format_bytes_usize(f, self.allocated.saturating_sub(self.deallocated))?;

        if f.alternate() {
            write!(f, " physical_mem=")?;
            if let Some(physical_mem) = self.physical_mem {
                format_bytes_usize(f, physical_mem)?;
            };

            write!(f, " virtual_mem=")?;
            if let Some(virtual_mem) = self.virtual_mem {
                format_bytes_usize(f, virtual_mem)?;
            };

            write!(f, " allocated=")?;
            format_bytes_usize(f, self.allocated)?;

            write!(f, " deallocated=")?;
            format_bytes_usize(f, self.deallocated)?;

            write!(f, " alloc_calls=")?;
            f.write_str(&self.alloc_calls.separate_with_commas())?;
        }

        Ok(())
    }
}

/// Difference of two [MemoryMetrics] measurements.
pub struct MetricsSpan {
    /// The current physical memory usage.
    pub physical_mem: Option<usize>,

    /// The current virtual memory usage.
    pub virtual_mem: Option<usize>,

    /// The current total number of bytes allocated.
    pub allocated: usize,

    /// The current total number of bytes deallocated.
    pub deallocated: usize,

    /// The current total number alloc calls.
    pub alloc_calls: usize,

    /// The number of bytes in use at the end of the span.
    pub in_use: usize,

    /// How much the physical mem changed for this span of time
    pub physical_mem_diff: Option<isize>,

    /// How much the virtual mem changed for this span of time
    pub virtual_mem_diff: Option<isize>,

    /// How many bytes were allocted for this span of time
    pub allocated_diff: usize,

    /// How many bytes were deallocted for this span of time
    pub deallocated_diff: usize,

    /// How alloc calls were done for this span of time.
    pub alloc_calls_diff: usize,

    /// The observed peak memory usage for this span of time.
    ///
    /// NOTE: The peak metric must be reset before the collection of
    /// [AllocatorMetrics] for this to be an accurate description of
    /// the span.
    pub peak: usize,

    /// The duration of the measurement.
    pub elapsed: Duration,
}

impl MetricsSpan {
    pub fn from_measurements(old: MemoryMetrics, new: MemoryMetrics) -> Self {
        let allocated = new.allocated;
        let deallocated = new.deallocated;
        let peak = new.peak;
        let in_use = new.allocated.saturating_sub(deallocated);
        let alloc_calls = new.alloc_calls;
        let allocated_diff = new.allocated - old.allocated;
        let deallocated_diff = new.deallocated - old.deallocated;
        let alloc_calls_diff = new.alloc_calls - old.alloc_calls;

        let elapsed = new.when.duration_since(old.when);

        let physical_mem_diff = match (old.physical_mem, new.physical_mem) {
            (Some(old), Some(new)) => {
                let new = isize::try_from(new).expect("diff must fit in an isize");
                let old = isize::try_from(old).expect("diff must fit in an isize");
                Some(new - old)
            }
            _ => None,
        };
        let virtual_mem_diff = match (old.virtual_mem, new.virtual_mem) {
            (Some(old), Some(new)) => {
                let new = isize::try_from(new).expect("diff must fit in an isize");
                let old = isize::try_from(old).expect("diff must fit in an isize");
                Some(new - old)
            }
            _ => None,
        };

        Self {
            physical_mem: new.physical_mem,
            virtual_mem: new.virtual_mem,
            allocated,
            deallocated,
            alloc_calls,
            peak,
            physical_mem_diff,
            virtual_mem_diff,
            allocated_diff,
            deallocated_diff,
            alloc_calls_diff,
            in_use,
            elapsed,
        }
    }
}

impl Display for MetricsSpan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "elapsed={:?}", self.elapsed)?;

        write!(f, " peak=")?;
        format_bytes_usize(f, self.peak)?;

        write!(f, " in_use=")?;
        format_bytes_usize(f, self.in_use)?;

        if f.alternate() {
            write!(f, " physical_mem=")?;
            if let Some(physical_mem) = self.physical_mem {
                format_bytes_usize(f, physical_mem)?;
            };

            write!(f, " virtual_mem=")?;
            if let Some(virtual_mem) = self.virtual_mem {
                format_bytes_usize(f, virtual_mem)?;
            };

            write!(f, " physical_mem_diff=")?;
            if let Some(physical_mem_diff) = self.physical_mem_diff {
                format_bytes_isize(f, physical_mem_diff)?;
            };

            write!(f, " virtual_mem_diff=")?;
            if let Some(virtual_mem_diff) = self.virtual_mem_diff {
                format_bytes_isize(f, virtual_mem_diff)?;
            };

            write!(f, " allocated=")?;
            format_bytes_usize(f, self.allocated)?;

            write!(f, " deallocated=")?;
            format_bytes_usize(f, self.deallocated)?;

            write!(f, " alloc_calls=")?;
            f.write_str(&self.alloc_calls.separate_with_commas())?;

            write!(f, " allocated_diff=")?;
            format_bytes_usize(f, self.allocated_diff)?;

            write!(f, " deallocated_diff=")?;
            format_bytes_usize(f, self.deallocated_diff)?;

            write!(f, " alloc_calls_diff=")?;
            f.write_str(&self.alloc_calls_diff.separate_with_commas())?;
        }

        Ok(())
    }
}

fn format_bytes_usize(f: &mut std::fmt::Formatter<'_>, v: usize) -> std::fmt::Result {
    write!(
        f,
        "{}",
        ByteSize::b(v.try_into().expect("Should fit in a u64")).display()
    )
}

fn format_bytes_isize(f: &mut std::fmt::Formatter<'_>, v: isize) -> std::fmt::Result {
    let prefix = if v.is_negative() { "-" } else { "" };
    let formatter = ByteSize::b(v.abs().try_into().expect("Should fit in a u64")).display();
    write!(f, "{prefix}{formatter}")
}
