/// A memory allocator which tracks memory's high water mark.
///
/// This is very similar to jemalloc's `thread.peak.read` and `thread.peak.reset` control options. With
/// the benefit of working with any of Rust's allocators, and the option of observing the data for all
/// threads.
///
/// NOTE: For each thread created there will a small amount of memory allocated which is _never_ freed.
/// The size of the allocation corresponds to one [CachePadded<BytesInUse>] structure.
use std::{
    alloc::{GlobalAlloc, Layout},
    cell::Cell,
    ops::Deref,
    sync::{
        LazyLock, RwLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use crossbeam_utils::CachePadded;

use crate::guard::StateGuard;

// Type description:
// - &'static: the metric data is a global
// - CachePadded: Pad the data to prevent false sharing.
// - BytesInUse: The actual metrics.
type ThreadDataRef = &'static CachePadded<ThreadData>;

// Type description:
// - Vec: Each entry corresponds to one thread state.
// - ThreadData: A single thread metric data.
type SharedData = Vec<ThreadDataRef>;

// Type description:
// - LazyLock: Used to wrap a `SharedData` and expose it as a static.
// - RwLock: To synchronize collecting the data and updating it.
static THREADS: LazyLock<RwLock<SharedData>> = LazyLock::new(|| Default::default());

// NOTE: The global allocator is not allowed to use an static with a destructor. see rust-lang/rust#126948
thread_local! {
    // Cache for the position of [BytesInUse] in the global array.
    static CACHED_REF: Cell<Option<ThreadDataRef>> = Cell::new(None);
}

fn init_thread_data() -> ThreadDataRef {
    match CACHED_REF.get() {
        Some(bytes) => bytes,
        None => {
            let mut lock = THREADS
                .write()
                .expect("Should never panic while holding the lock");

            let bytes = ThreadData {
                allocated: Default::default(),
                deallocated: Default::default(),
                peak: Default::default(),
                num_alloc_calls: Default::default(),
            };
            let bytes = Box::leak(Box::new(CachePadded::new(bytes)));
            lock.push(bytes);

            CACHED_REF.set(Some(bytes));

            bytes
        }
    }
}

/// Returns the peak bytes usage for the whole program.
pub fn global_peak() -> usize {
    let _guard = StateGuard::new();

    let threads = THREADS
        .read()
        .expect("Should never panic while holding the lock");

    threads.iter().map(|v| v.peak.load(Ordering::Relaxed)).sum()
}

fn reset_peak(thread_data: ThreadDataRef) {
    let allocated = thread_data.allocated.load(Ordering::Relaxed);
    let deallocated = thread_data.deallocated.load(Ordering::Relaxed);
    let peak = allocated.saturating_sub(deallocated);
    thread_data.peak.store(peak, Ordering::Relaxed);
}

/// Resets the peak for all threads.
pub fn global_reset() -> Vec<BytesInUse> {
    let _guard = StateGuard::new();

    let threads = THREADS
        .read()
        .expect("Should never panic while holding the lock");
    let mut results = Vec::with_capacity(threads.len());

    for thread_data in threads.iter() {
        results.push(BytesInUse::from((*thread_data).deref()));
        reset_peak(thread_data);
    }

    results
}

/// Returns the peak of this thread.
pub fn thread_peak() -> usize {
    let _guard = StateGuard::new();

    init_thread_data().peak.load(Ordering::Relaxed)
}

/// Resets the peak of this thread.
pub fn thread_reset_peak() {
    let _guard = StateGuard::new();

    let thread_data = init_thread_data();
    reset_peak(thread_data)
}

struct ThreadData {
    /// Current number of bytes allocated.
    ///
    /// Can be smaller than deallocated if the thread drops Send objects it did
    /// not allocate.
    pub allocated: AtomicUsize,

    /// Current number of bytes deallocated.
    ///
    /// Can be greater than allocated if the thread drops Send objects it did
    /// not allocate.
    pub deallocated: AtomicUsize,

    /// Current high water mark.
    ///
    /// Can be re-set.
    pub peak: AtomicUsize,

    /// Number of alloc calls.
    pub num_alloc_calls: AtomicUsize,
}

pub struct BytesInUse {
    /// Current number of bytes allocated.
    ///
    /// Can be smaller than deallocated if the thread drops Send objects it did
    /// not allocate.
    pub allocated: usize,

    /// Current number of bytes deallocated.
    ///
    /// Can be greater than allocated if the thread drops Send objects it did
    /// not allocate.
    pub deallocated: usize,

    /// Current high water mark.
    ///
    /// Can be re-set.
    pub peak: usize,

    /// Number of alloc calls.
    pub num_alloc_calls: usize,
}

impl From<&ThreadData> for BytesInUse {
    fn from(value: &ThreadData) -> Self {
        BytesInUse {
            allocated: value.allocated.load(Ordering::Relaxed),
            deallocated: value.deallocated.load(Ordering::Relaxed),
            peak: value.peak.load(Ordering::Relaxed),
            num_alloc_calls: value.num_alloc_calls.load(Ordering::Relaxed),
        }
    }
}

/// A high water mark allocator tracking bytes in use.
///
/// This allocator maintains a high-water-mark with the number of bytes in use.
/// The high-water-mark can also be reset, useful for to identify which of a set
/// of batch jobs uses the most memory.
///
/// Note: This is not the same as the number of bytes allocated by the process,
/// since the underlying allocator `T` will request and manage allocation in
/// pages.
pub struct BytesInUseTracker<T> {
    inner: T,
}

impl<T> BytesInUseTracker<T> {
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

    /// Returns the peak bytes usage for the whole program.
    pub fn peak(&self) -> usize {
        global_peak()
    }

    /// Resets the peak of all threads.
    pub fn reset(&self) -> Vec<BytesInUse> {
        global_reset()
    }
}

unsafe impl<T: GlobalAlloc> GlobalAlloc for BytesInUseTracker<T> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ret = unsafe { self.inner.alloc(layout) };

        if !ret.is_null() {
            if let Some(_guard) = StateGuard::track() {
                let thread_data = init_thread_data();

                let size = layout.size();
                thread_data.allocated.fetch_add(size, Ordering::Relaxed);
                thread_data.peak.fetch_add(size, Ordering::Relaxed);
                thread_data.num_alloc_calls.fetch_add(1, Ordering::Relaxed);
            }
        }
        ret
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe {
            self.inner.dealloc(ptr, layout);
        }
        if let Some(_guard) = StateGuard::track() {
            let thread_data = init_thread_data();

            let size = layout.size();
            thread_data.deallocated.fetch_add(size, Ordering::Relaxed);
            thread_data.peak.fetch_sub(size, Ordering::Relaxed);
        }
    }
}
