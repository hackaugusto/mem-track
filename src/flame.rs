use std::{
    alloc::{GlobalAlloc, Layout},
    cell::Cell,
    collections::{BTreeMap, btree_map::Entry},
    fmt::Display,
    hash::{DefaultHasher, Hash, Hasher},
    mem,
    ops::DerefMut,
    sync::{
        LazyLock, Mutex, RwLock, RwLockReadGuard,
        atomic::{AtomicUsize, Ordering},
    },
    thread::{self, ThreadId},
};

use backtrace::{Backtrace, BacktraceSymbol};
use crossbeam_utils::CachePadded;
use rustc_demangle::{Demangle, demangle};

use crate::guard::StateGuard;

// Type description:
// - Vec: Each entry corresponds to one thread state.
// - CachePadded: Pad the data to prevent false sharing.
// - Mutex: to allow for interion mutation.
// - ThreadData: the flame graph data.
type SharedData = Vec<CachePadded<Mutex<ThreadData>>>;

type FlameGraph = BTreeMap<BacktraceId, AllocationSite>;
type Metrics = Vec<(&'static Backtrace, usize)>;

// Type description:
// - LazyLock: Used to wrap a `Mutex<Vec<_>>` and expose it as a static.
// - RwLock: To synchronize collecting the data and updating it.
static THREADS: LazyLock<RwLock<SharedData>> = LazyLock::new(|| Default::default());

static BACKTRACE_CACHE: LazyLock<Mutex<BTreeMap<BacktraceId, &'static Backtrace>>> =
    LazyLock::new(|| Default::default());

// NOTE: The global allocator is not allowed to use an static with a destructor. see rust-lang/rust#126948
thread_local! {
    // Cache for the position of allocation map in the global array.
    static CACHED_POSITION: Cell<usize> = Cell::new(0);
}

fn init_btree() -> RwLockReadGuard<'static, SharedData> {
    loop {
        let pos = CACHED_POSITION.get();
        {
            let lock = THREADS
                .read()
                .expect("Should never panic while holding the lock");

            if let Some(data) = lock.get(pos) {
                let thread_id = data
                    .lock()
                    .expect("Should never panic while holding the lock")
                    .thread_id;

                if thread_id == thread::current().id() {
                    return lock;
                }
            }
        }

        {
            let mut lock = THREADS
                .write()
                .expect("Should never panic while holding the lock");

            let flame_graph = ThreadData {
                flame: Default::default(),
                thread_id: thread::current().id(),
            };
            let pos = lock.len();
            CACHED_POSITION.set(pos);
            lock.push(CachePadded::new(Mutex::new(flame_graph)));
        }
    }
}

/// A wrapper struct to uniquely identify an allocation size.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BacktraceId(u64);

/// Flame graph for the number of alloc calls.
pub struct AllocCallsFlameGraph {
    metrics: Metrics,
}

/// Flame graph for the number of bytes allocated.
pub struct BytesAllocatedFlameGraph {
    metrics: Metrics,
}

fn symbol_to_name(symbol: &BacktraceSymbol) -> Demangle {
    let name = symbol.name().map(|n| n.as_str()).flatten().unwrap_or("");
    demangle(name)
}

/// Format a single entry in a flame graph.
fn format_entry(
    f: &mut std::fmt::Formatter<'_>,
    backtrace: &'static Backtrace,
    metric: usize,
) -> std::fmt::Result {
    for frame in backtrace.frames().iter().rev() {
        let mut symbols = frame.symbols().iter();

        if let Some(symbol) = symbols.next() {
            f.write_fmt(format_args!("{}", symbol_to_name(symbol)))?;
        }

        for symbol in symbols {
            f.write_str(";")?;
            f.write_fmt(format_args!("{}", symbol_to_name(symbol)))?;
        }
    }
    f.write_str(" ")?;
    f.write_fmt(format_args!("{}", metric))
}

/// Format a flame graph document.
fn format_flame_graph(
    data: &[(&'static Backtrace, usize)],
    f: &mut std::fmt::Formatter<'_>,
) -> std::fmt::Result {
    let mut iter = data.iter();
    let last = iter.next_back();
    for (backtrace, metric) in data {
        format_entry(f, backtrace, *metric)?;
        f.write_str("\n")?;
    }
    if let Some((backtrace, metric)) = last {
        format_entry(f, backtrace, *metric)?;
    }

    Ok(())
}

impl Display for AllocCallsFlameGraph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        format_flame_graph(&self.metrics, f)
    }
}

impl Display for BytesAllocatedFlameGraph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        format_flame_graph(&self.metrics, f)
    }
}

struct ThreadData {
    /// Tracks each individual allocation size
    flame: FlameGraph,

    /// The thread to which this data corresponds to.
    thread_id: ThreadId,
}

impl From<&Backtrace> for BacktraceId {
    fn from(value: &Backtrace) -> Self {
        let mut hasher = DefaultHasher::new();
        for frame in value.frames().iter() {
            hasher.write_u64(frame.ip() as u64)
        }
        BacktraceId(hasher.finish())
    }
}

#[derive(Debug)]
pub struct AllocationSite {
    /// Number of allocations done at this call site.
    alloc_calls: AtomicUsize,

    /// Number of bytes allocated at this call site.
    bytes_allocated: AtomicUsize,

    /// The corresponding [Backtrace] for this site.
    backtrace: Backtrace,
}

impl AllocationSite {
    /// Creates a new [AllocationSite] with the given [Backtrace].
    fn with_backtrace(backtrace: Backtrace) -> AllocationSite {
        AllocationSite {
            alloc_calls: AtomicUsize::default(),
            bytes_allocated: AtomicUsize::default(),
            backtrace,
        }
    }
}

/// Extract global metric data to generate a flame graph.
fn global_extract_flame_data<F>(data: &[CachePadded<Mutex<ThreadData>>], extractor: F) -> Metrics
where
    F: Fn(&AllocationSite) -> usize,
{
    let result = {
        let mut cache = BACKTRACE_CACHE
            .lock()
            .expect("Should never panic while holding the lock");

        let mut result = BTreeMap::new();
        for thread in data.iter() {
            let lock = thread
                .lock()
                .expect("Should never panic while holding the lock");

            for (key, value) in lock.flame.iter() {
                let alloc_calls = extractor(value);
                match result.entry(*key) {
                    Entry::Vacant(vacant_entry) => {
                        cache.entry(*key).or_insert_with(|| {
                            let mut backtrace = Box::new(value.backtrace.clone());
                            backtrace.resolve();
                            Box::leak(backtrace)
                        });
                        let backtrace_resolved = cache.get(key).expect("Initialised above");

                        vacant_entry.insert((*backtrace_resolved, alloc_calls));
                    }
                    Entry::Occupied(occupied_entry) => {
                        occupied_entry.into_mut().1 += alloc_calls;
                    }
                };
            }
        }
        result
    };

    result.into_values().collect()
}

/// Extract global metric data to generate a flame graph for the number of [GlobalAlloc::alloc] calls.
pub fn global_alloc_calls() -> AllocCallsFlameGraph {
    let _guard = StateGuard::new();

    let threads = THREADS
        .read()
        .expect("Should never panic while holding the lock");
    let metrics =
        global_extract_flame_data(&threads, |value| value.alloc_calls.load(Ordering::Relaxed));

    AllocCallsFlameGraph { metrics }
}

/// Extract global metric data to generate a flame graph for the number of bytes allocated via the global allocator.
pub fn global_bytes_allocated() -> BytesAllocatedFlameGraph {
    let _guard = StateGuard::new();

    let threads = THREADS
        .read()
        .expect("Should never panic while holding the lock");
    let metrics = global_extract_flame_data(&threads, |value| {
        value.bytes_allocated.load(Ordering::Relaxed)
    });

    BytesAllocatedFlameGraph { metrics }
}

/// Extract global metric data to generate a flame graph for the number of [GlobalAlloc::alloc] calls.
pub fn global_reset() -> (AllocCallsFlameGraph, BytesAllocatedFlameGraph) {
    let _guard = StateGuard::new();

    let threads = {
        let empty = Vec::new();
        let mut threads = THREADS
            .write()
            .expect("Should never panic while holding the lock");
        mem::replace(threads.as_mut(), empty)
    };

    let metrics =
        global_extract_flame_data(&threads, |value| value.alloc_calls.load(Ordering::Relaxed));
    let alloc_calls = AllocCallsFlameGraph { metrics };

    let metrics = global_extract_flame_data(&threads, |value| {
        value.bytes_allocated.load(Ordering::Relaxed)
    });
    let bytes_allocated = BytesAllocatedFlameGraph { metrics };

    (alloc_calls, bytes_allocated)
}

pub struct FlameAlloc<T> {
    inner: T,
}

impl<T> FlameAlloc<T> {
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

    /// Returns the data for number of alloc calls.
    pub fn alloc_calls(&self) -> AllocCallsFlameGraph {
        global_alloc_calls()
    }

    /// Returns the data for number of bytes allocated.
    pub fn bytes_allocated(&self) -> BytesAllocatedFlameGraph {
        global_bytes_allocated()
    }

    /// Reset the flame graph data and return the corresponding graphs.
    pub fn global_reset(&self) -> (AllocCallsFlameGraph, BytesAllocatedFlameGraph) {
        global_reset()
    }

    /// Empties the cache of resolved backtraces.
    pub fn empty_backtrace_cache(&self) {
        let empty = Default::default();
        let mut cache = BACKTRACE_CACHE
            .lock()
            .expect("Should never panic while holding the lock");
        let _ = mem::replace(cache.deref_mut(), empty);
    }
}

unsafe impl<T: GlobalAlloc> GlobalAlloc for FlameAlloc<T> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ret = unsafe { self.inner.alloc(layout) };
        if !ret.is_null() {
            if let Some(_guard) = StateGuard::track() {
                let backtrace = Backtrace::new_unresolved();
                let id = BacktraceId::from(&backtrace);

                let lock = init_btree();
                let mut flame_graph = lock[CACHED_POSITION.get()]
                    .lock()
                    .expect("Should never panic while holding the lock");

                let allocation_site = flame_graph
                    .flame
                    .entry(id)
                    .or_insert(AllocationSite::with_backtrace(backtrace));
                allocation_site.alloc_calls.fetch_add(1, Ordering::Relaxed);
                allocation_site
                    .bytes_allocated
                    .fetch_add(layout.size(), Ordering::Relaxed);
            }
        }
        ret
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe {
            self.inner.dealloc(ptr, layout);
        }
    }
}
