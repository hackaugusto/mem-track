use std::{
    alloc::{GlobalAlloc, Layout},
    cell::Cell,
    collections::{BTreeMap, btree_map::Entry},
    env,
    fmt::Display,
    hash::{DefaultHasher, Hash, Hasher},
    mem,
    ops::DerefMut,
    sync::{
        LazyLock, Mutex, MutexGuard, RwLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use backtrace::{Backtrace, BacktraceSymbol};
use rustc_demangle::{Demangle, demangle};

use crate::guard::StateGuard;

// Type description:
// - &'static: The pinned reference to the thread flame data.
// - Mutex: To allow swap'ing the flame graph data
// - FlameGraph: The actual data.
type ThreadDataRef = &'static Mutex<FlameGraph>;

// A flame graph maps a stack trace to a counter / metrics.
//
// The is the result of hashing the actual backtrace.
type FlameGraph = BTreeMap<BacktraceId, AllocationSite>;

// A flame graph for a specific metric.
type Metrics = Vec<(&'static Backtrace, usize)>;

// Type description:
// - LazyLock: To expose the metric data as an static.
// - RwLock: To synchronize collecting the data and updating it.
// - Vec: Each entry corresponds to one thread state.
// - ThreadDataRef: A single thread flame graph.
static THREADS: LazyLock<RwLock<Vec<ThreadDataRef>>> = LazyLock::new(|| Default::default());

// Used to enable/disable flame graph generation at runtime.
static ENABLED: LazyLock<AtomicBool> = LazyLock::new(|| {
    let initial = env::var("FLAMEGRAPH")
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    AtomicBool::new(initial)
});

static BACKTRACE_CACHE: LazyLock<Mutex<BTreeMap<BacktraceId, &'static Backtrace>>> =
    LazyLock::new(|| Default::default());

// NOTE: The global allocator is not allowed to use an static with a destructor. see rust-lang/rust#126948
thread_local! {
    // Cache for the position of [BytesInUse] in the global array.
    // Cell: Provides interior mutability
    // Option: Used to identify the initialisation and push the reference to the global THREADS.
    // ThreadDataRef: The thread data.
    static CACHED_REF: Cell<Option<ThreadDataRef>> = Cell::new(None);
}

fn init_thread_data() -> MutexGuard<'static, FlameGraph> {
    loop {
        match CACHED_REF.get() {
            Some(flame_graph) => {
                return flame_graph
                    .lock()
                    .expect("Should never panic while holding the lock");
            }
            None => {}
        };

        let mut lock = THREADS
            .write()
            .expect("Should never panic while holding the lock");

        let flame_graph = Box::leak(Box::new(Mutex::new(Default::default())));
        lock.push(flame_graph);

        CACHED_REF.set(Some(flame_graph));
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
    let all_frames = backtrace.frames().iter().rev();
    let mut all_symbols = all_frames.flat_map(|frame| frame.symbols().iter());

    if let Some(symbol) = all_symbols.next() {
        f.write_fmt(format_args!("{}", symbol_to_name(symbol)))?;
    }

    for symbol in all_symbols {
        f.write_str(";")?;
        f.write_fmt(format_args!("{}", symbol_to_name(symbol)))?;
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
fn global_extract_flame_data<F>(data: &[FlameGraph], extractor: F) -> Metrics
where
    F: Fn(&AllocationSite) -> usize,
{
    let result = {
        let mut cache = BACKTRACE_CACHE
            .lock()
            .expect("Should never panic while holding the lock");

        let mut result = BTreeMap::new();
        for thread in data.iter() {
            for (key, value) in thread.iter() {
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

/// Enables flame graph collection
pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

/// Disables flame graph collection and return existing graphs.
pub fn disable() -> (AllocCallsFlameGraph, BytesAllocatedFlameGraph) {
    ENABLED.store(false, Ordering::Relaxed);

    flame_graphs()
}

/// Extract global metric data to generate a flame graph for the number of [GlobalAlloc::alloc] calls.
pub fn flame_graphs() -> (AllocCallsFlameGraph, BytesAllocatedFlameGraph) {
    let _guard = StateGuard::new();

    let threads = {
        let threads = THREADS
            .read()
            .expect("Should never panic while holding the lock");

        let mut graphs = Vec::with_capacity(threads.len());
        graphs.resize_with(threads.len(), || FlameGraph::default());

        for (pos, thread) in threads.iter().enumerate() {
            let mut data = thread
                .lock()
                .expect("Should never panic while holding the lock");
            mem::swap(data.deref_mut(), &mut graphs[pos]);
        }

        graphs
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

    /// Reset the flame graph data and return the corresponding graphs.
    pub fn flame_graphs(&self) -> (AllocCallsFlameGraph, BytesAllocatedFlameGraph) {
        flame_graphs()
    }

    /// Enables flame graph collection
    pub fn enable(&self) {
        enable()
    }

    /// Disables flame graph collection and return existing graphs.
    pub fn disable(&self) -> (AllocCallsFlameGraph, BytesAllocatedFlameGraph) {
        disable()
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
                if ENABLED.load(Ordering::Relaxed) {
                    let backtrace = Backtrace::new_unresolved();
                    let id = BacktraceId::from(&backtrace);

                    let mut flame_graph = init_thread_data();
                    let allocation_site = flame_graph
                        .entry(id)
                        .or_insert(AllocationSite::with_backtrace(backtrace));
                    allocation_site.alloc_calls.fetch_add(1, Ordering::Relaxed);
                    allocation_site
                        .bytes_allocated
                        .fetch_add(layout.size(), Ordering::Relaxed);
                }
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
