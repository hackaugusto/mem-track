/// A memory allocator which tracks memory's high water mark.
///
/// This is very similar to jemalloc's `thread.peak.read` and `thread.peak.reset` control options. With
/// the benefit of working with any of Rust's allocators, and the option of observing the data for all
/// threads.
use std::{
    alloc::{GlobalAlloc, Layout},
    cell::Cell,
    mem,
    sync::{
        LazyLock, RwLock, RwLockReadGuard,
        atomic::{AtomicUsize, Ordering},
    },
    thread::{self, ThreadId},
};

use crossbeam_utils::CachePadded;

use crate::guard::StateGuard;

// Type description:
// - Vec: Each entry corresponds to one thread state.
// - CachePadded: Pad the data to prevent false sharing.
// - BytesInUse: The actual metrics.
type SharedData = Vec<CachePadded<BytesInUse>>;

// Type description:
// - LazyLock: Used to wrap a `SharedData` and expose it as a static.
// - RwLock: To synchronize collecting the data and updating it.
static THREADS: LazyLock<RwLock<SharedData>> = LazyLock::new(|| Default::default());

// NOTE: The global allocator is not allowed to use an static with a destructor. see rust-lang/rust#126948
thread_local! {
    // Cache for the position of [BytesInUse] in the global array.
    static CACHED_POSITION: Cell<usize> = Cell::new(0);
}

fn init_bytes() -> RwLockReadGuard<'static, SharedData> {
    loop {
        let pos = CACHED_POSITION.get();
        {
            let lock = THREADS
                .read()
                .expect("Should never panic while holding the lock");

            if let Some(data) = lock.get(pos) {
                if data.thread_id == thread::current().id() {
                    return lock;
                }
            }
        }

        {
            let mut lock = THREADS
                .write()
                .expect("Should never panic while holding the lock");

            let bytes = BytesInUse {
                allocated: Default::default(),
                deallocated: Default::default(),
                peak: Default::default(),
                num_alloc_calls: Default::default(),
                thread_id: thread::current().id(),
            };
            let pos = lock.len();
            CACHED_POSITION.set(pos);
            lock.push(CachePadded::new(bytes));
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

/// Resets the peak for all threads.
///
/// This function will take the existing metrics and reset the global vector. This will invalidate
/// the cached position of all threads and result in re-allocation of all the metrics. After this
/// initial contention the position cache will be hydrated and valid and contention should be back to
/// a minimum.
///
/// This operation cleans the vector to free up memory of exited threads, because the global allocator
/// can not use thread local destructors.
pub fn global_reset() -> SharedData {
    let _guard = StateGuard::new();

    let empty = Vec::new();
    let mut threads = THREADS
        .write()
        .expect("Should never panic while holding the lock");
    mem::replace(threads.as_mut(), empty)
}

/// Returns the peak of this thread.
pub fn thread_peak() -> usize {
    let _guard = StateGuard::new();

    let lock = init_bytes();
    lock[CACHED_POSITION.get()].peak.load(Ordering::Relaxed)
}

/// Resets the peak of this thread.
pub fn thread_reset_peak() {
    let _guard = StateGuard::new();

    let lock = init_bytes();
    let bytes = &lock[CACHED_POSITION.get()];
    let allocated = bytes.allocated.load(Ordering::Relaxed);
    let deallocated = bytes.deallocated.load(Ordering::Relaxed);
    let peak = allocated.saturating_sub(deallocated);
    bytes.peak.store(peak, Ordering::Relaxed);
}

pub struct BytesInUse {
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

    /// The thread to which this data corresponds to.
    thread_id: ThreadId,
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
    pub fn reset(&self) -> SharedData {
        global_reset()
    }
}

unsafe impl<T: GlobalAlloc> GlobalAlloc for BytesInUseTracker<T> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ret = unsafe { self.inner.alloc(layout) };

        if !ret.is_null() {
            if let Some(_guard) = StateGuard::track() {
                let lock = init_bytes();
                let bytes = &lock[CACHED_POSITION.get()];

                let size = layout.size();
                let old_allocated = bytes.allocated.fetch_add(size, Ordering::Relaxed);
                // It's okay to add without synchronization because this thread is the only one changing the value
                let new_allocated = usize::try_from(old_allocated + size).unwrap_or(0);
                bytes.peak.fetch_max(new_allocated, Ordering::Relaxed);
                bytes.num_alloc_calls.fetch_add(1, Ordering::Relaxed);
            }
        }
        ret
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe {
            self.inner.dealloc(ptr, layout);
        }
        if let Some(_guard) = StateGuard::track() {
            let lock = init_bytes();
            let bytes = &lock[CACHED_POSITION.get()];

            bytes
                .deallocated
                .fetch_add(layout.size(), Ordering::Relaxed);
        }
    }
}
