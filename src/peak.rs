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
        atomic::{AtomicIsize, AtomicUsize, Ordering},
    },
    thread::{self, ThreadId},
};

// Type description:
// - RwLock: To synchronize collecting the data and updating it.
// - Vec: Each entry corresponds to one thread state.
// - BytesInUse: The actual metrics.
type SharedData = RwLock<Vec<BytesInUse>>;

// Type description:
// - LazyLock: Used to wrap a `SharedDAta` and expose it as a static.
static THREADS: LazyLock<SharedData> = LazyLock::new(|| Default::default());

/// Tracking must be disabled when running the tracker code itself.
///
/// This is because tracking the tracker would cause infinite recursion or cause
/// deadlock when changing the underlying containers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum State {
    CanTrack = 0,
    MustNotTrack = 1,
}

// NOTE: The global allocator is not allowed to use an static with a destructor. see rust-lang/rust#126948
thread_local! {
    // Cache for the position of [BytesInUse] in the global array.
    static CACHED_POSITION: Cell<usize> = Cell::new(0);

    // True if inside the allocator code, disable tracing to prevent infinite recursion.
    static STATE: Cell<State> = Cell::new(State::CanTrack);
}

/// Must be used for all public APIs (include the allocator itself).
///
/// This prevents dead locks and infinite recursion by forbidding running the tracking code multiple times.
struct StateGuard {
    old: State,
}

impl StateGuard {
    fn new() -> Self {
        let old = STATE.replace(State::MustNotTrack);
        Self { old }
    }
}

impl Drop for StateGuard {
    fn drop(&mut self) {
        STATE.set(self.old);
    }
}

fn init_bytes() -> RwLockReadGuard<'static, Vec<BytesInUse>> {
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
                in_use: Default::default(),
                peak: Default::default(),
                thread_id: thread::current().id(),
            };
            let pos = lock.len();
            CACHED_POSITION.set(pos);
            lock.push(bytes);
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
pub fn global_reset() -> Vec<BytesInUse> {
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
    let in_use = bytes.in_use.load(Ordering::Relaxed);
    let in_use = usize::try_from(in_use).unwrap_or(0);
    bytes.peak.store(in_use, Ordering::Relaxed);
}

/// Trace a memory allocation.
fn thread_alloc(size: usize) {
    let size = size.try_into().expect("allocation should fit in a isize");

    let lock = init_bytes();
    let bytes = &lock[CACHED_POSITION.get()];

    let old_allocated = bytes.in_use.fetch_add(size, Ordering::Relaxed);
    // It's okay to add without synchronization because this thread is the only one changing the value
    let new_allocated = usize::try_from(old_allocated + size).unwrap_or(0);
    bytes.peak.fetch_max(new_allocated, Ordering::Relaxed);
}

/// Trace a memory de-allocation.
fn thread_dealloc(size: usize) {
    let size = size.try_into().expect("allocation should fit in a isize");

    let lock = init_bytes();
    let bytes = &lock[CACHED_POSITION.get()];

    bytes.in_use.fetch_sub(size, Ordering::Relaxed);
}

pub struct BytesInUse {
    /// Current number of bytes in use.
    ///
    /// Will be lower than peak after a deallocation.
    /// May be negative when a type is Send and deallocated on a different thread.
    in_use: AtomicIsize,

    /// Current high water mark.
    ///
    /// Can be re-set.
    peak: AtomicUsize,

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
    pub fn reset(&mut self) -> Vec<BytesInUse> {
        global_reset()
    }
}

unsafe impl<T: GlobalAlloc> GlobalAlloc for BytesInUseTracker<T> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ret = unsafe { self.inner.alloc(layout) };

        if !ret.is_null() && STATE.get() == State::CanTrack {
            let _guard = StateGuard::new();
            thread_alloc(layout.size());
        }
        ret
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe {
            self.inner.dealloc(ptr, layout);
        }
        if STATE.get() == State::CanTrack {
            let _guard = StateGuard::new();
            thread_dealloc(layout.size());
        }
    }
}
