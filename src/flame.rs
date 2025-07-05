use std::{
    alloc::{GlobalAlloc, Layout},
    cell::Cell,
    collections::{BTreeMap, btree_map::Entry},
    env,
    hash::{DefaultHasher, Hash, Hasher},
    io, mem,
    ops::DerefMut,
    sync::{
        LazyLock, Mutex, MutexGuard, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use addr2line::{Frame, Loader};
use arrayvec::ArrayVec;
use findshlibs::{Bias, Segment, SharedLibrary, TargetSharedLibrary};
use gimli::Reader;

use crate::guard::StateGuard;

// Type description:
// - &'static: The pinned reference to the thread flame data.
// - Mutex: To allow swap'ing the flame graph data
// - FlameGraph: The actual data.
type ThreadDataRef = &'static Mutex<AllocationData>;

// Data tracked to generate the frame graph
type AllocationData = BTreeMap<BacktraceId, AllocationSite>;

// A flame graph maps a stack trace to a counter / metrics.
//
// The is the result of hashing the actual backtrace.
type FlameGraph = Vec<(Backtrace, Metrics)>;

// Type used to defined a backtrace
type Backtrace = ArrayVec<usize, 64>;

// Value used to represent an unknwon function name
const UNKNOWN: &str = "??";

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

// NOTE: The global allocator is not allowed to use an static with a destructor. see rust-lang/rust#126948
thread_local! {
    // Cache for the position of [BytesInUse] in the global array.
    // Cell: Provides interior mutability
    // Option: Used to identify the initialisation and push the reference to the global THREADS.
    // ThreadDataRef: The thread data.
    static CACHED_REF: Cell<Option<ThreadDataRef>> = Cell::new(None);
}

fn init_thread_data() -> MutexGuard<'static, AllocationData> {
    loop {
        match CACHED_REF.get() {
            Some(data) => {
                return data
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

impl From<&[usize]> for BacktraceId {
    fn from(value: &[usize]) -> Self {
        let mut hasher = DefaultHasher::new();
        for frame in value {
            hasher.write_usize(*frame)
        }
        BacktraceId(hasher.finish())
    }
}

#[derive(Debug)]
struct AllocationSite {
    /// Number of allocations done at this call site.
    alloc_calls: usize,

    /// Number of bytes allocated at this call site.
    bytes_allocated: usize,

    /// The corresponding [Backtrace] for this site.
    backtrace: ArrayVec<usize, 64>,
}

#[derive(Debug)]
pub struct Metrics {
    /// Number of allocations done at this call site.
    pub alloc_calls: usize,

    /// Number of bytes allocated at this call site.
    pub bytes_allocated: usize,
}

impl AllocationSite {
    /// Creates a new [AllocationSite] with the given [Backtrace].
    fn with_backtrace(backtrace: ArrayVec<usize, 64>) -> AllocationSite {
        AllocationSite {
            alloc_calls: 0,
            bytes_allocated: 0,
            backtrace,
        }
    }
}

/// Extract global metric data to generate a flame graph.
fn global_extract_flame_data(mut data: Vec<AllocationData>) -> FlameGraph {
    let mut result = data.pop().unwrap_or_default();

    for thread in data.into_iter() {
        for (key, value) in thread.into_iter() {
            match result.entry(key) {
                Entry::Vacant(vacant_entry) => {
                    vacant_entry.insert(value);
                }
                Entry::Occupied(occupied_entry) => {
                    let inner = occupied_entry.into_mut();
                    let alloc_calls = value.alloc_calls;
                    let bytes_allocated = value.bytes_allocated;
                    inner.alloc_calls += alloc_calls;
                    inner.bytes_allocated += bytes_allocated;
                }
            };
        }
    }
    result
        .into_iter()
        .map(|(_key, value)| {
            let metrics = Metrics {
                alloc_calls: value.alloc_calls,
                bytes_allocated: value.bytes_allocated,
            };

            (value.backtrace, metrics)
        })
        .collect()
}

/// Enables flame graph collection
pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

/// Disables flame graph collection and return existing graphs.
pub fn disable() -> FlameGraph {
    ENABLED.store(false, Ordering::Relaxed);

    global_flame_graph()
}

/// Extract global metric data to generate a flame graph for the number of [GlobalAlloc::alloc] calls.
pub fn global_flame_graph() -> FlameGraph {
    let _guard = StateGuard::new();

    let threads = {
        let threads = THREADS
            .read()
            .expect("Should never panic while holding the lock");

        let mut graphs = Vec::with_capacity(threads.len());
        graphs.resize_with(threads.len(), || AllocationData::default());

        for (pos, thread) in threads.iter().enumerate() {
            let mut data = thread
                .lock()
                .expect("Should never panic while holding the lock");
            mem::swap(data.deref_mut(), &mut graphs[pos]);
        }

        graphs
    };

    global_extract_flame_data(threads)
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
    pub fn global_flame_graph(&self) -> FlameGraph {
        global_flame_graph()
    }

    /// Enables flame graph collection
    pub fn enable(&self) {
        enable()
    }

    /// Disables flame graph collection and return existing graphs.
    pub fn disable(&self) -> FlameGraph {
        disable()
    }
}

struct LoaderDetails {
    /// The loader corresponding to a shared library.
    loader: Loader,

    /// The difference among the actual and the stated virtual memory addresses.
    bias: Bias,
}

struct SegmentDetails {
    /// The actual virtual memory address of this segment
    avma: usize,

    /// The idx of its corresponding loader
    loader: usize,
}

struct Details {
    loaders: Vec<LoaderDetails>,
    segments: Vec<SegmentDetails>,
}

fn get_loaders() -> Details {
    let mut loaders = Vec::new();
    let mut segments = Vec::new();

    TargetSharedLibrary::each(|shlib| match Loader::new(shlib.name()) {
        Ok(loader) => {
            let loader_pos = loaders.len();
            loaders.push(LoaderDetails {
                loader,
                bias: shlib.virtual_memory_bias(),
            });

            for segment in shlib.segments().filter(|s| s.is_code()) {
                let avma = segment.actual_virtual_memory_address(shlib);
                segments.push(SegmentDetails {
                    avma: avma.into(),
                    loader: loader_pos,
                });
            }
        }
        Err(_err) => {}
    });
    segments.sort_by_key(|seg| seg.avma);

    Details { loaders, segments }
}

/// Writes the frame name to the formatter `f`.
fn write_frame<R: Reader>(
    f: &mut impl io::Write,
    frame: Frame<'_, R>,
    loader: &Loader,
    svma: u64,
) -> io::Result<()> {
    match frame.function {
        Some(function) => match function.demangle() {
            Ok(name) => {
                write!(f, "{}", name)
            }
            Err(_err) => match function.raw_name() {
                Ok(name) => write!(f, "{}", name),
                Err(_err) => write!(f, "{}", UNKNOWN),
            },
        },
        None => match loader.find_symbol(svma) {
            Some(symbol) => write!(f, "{}", symbol),
            None => write!(f, "{}", UNKNOWN),
        },
    }
}

/// Writes the function names corresponding to the instruction pointer.
///
/// Given an advertised virtual memory address and debug info loaders this function
/// resolves the names corresponding to the `avma` and write it to `f`.
fn resolve_and_write_frame(
    f: &mut impl io::Write,
    avma: usize,
    details: &Details,
) -> io::Result<()> {
    let segment = match details.segments.binary_search_by_key(&avma, |seg| seg.avma) {
        Ok(v) => v,
        Err(v) => v - 1,
    };
    let loader = &details.loaders[details.segments[segment].loader];

    let svma = u64::try_from(avma.wrapping_sub(loader.bias.0)).unwrap();
    let mut frames = match loader.loader.find_frames(svma) {
        Ok(frames) => frames,
        Err(_err) => {
            write!(f, "{:#}", avma)?;
            return Ok(());
        }
    };

    match frames.next() {
        Ok(Some(frame)) => write_frame(f, frame, &loader.loader, svma)?,
        Ok(None) => {
            write!(f, "{:#}", avma)?;
            return Ok(());
        }
        Err(_err) => write!(f, "{:#}", avma)?,
    }

    loop {
        write!(f, ";")?;

        match frames.next() {
            Ok(Some(frame)) => write_frame(f, frame, &loader.loader, svma)?,
            Ok(None) => break,
            Err(_err) => write!(f, "{}", UNKNOWN)?,
        }
    }

    Ok(())
}

/// Resolve and write the backtrace to the formatter
fn write_backtrace(f: &mut impl io::Write, trace: &Backtrace) -> io::Result<()> {
    let details = get_loaders();
    let mut iter = trace.iter();

    if let Some(avma) = iter.next() {
        resolve_and_write_frame(f, *avma, &details)?;
    }

    for avma in iter {
        write!(f, ";")?;
        resolve_and_write_frame(f, *avma, &details)?;
    }

    Ok(())
}

/// Format a flame graph document.
pub fn format_flame_graph<'a, F, D>(
    f: &mut impl io::Write,
    mut flamegraph: D,
    metric: F,
) -> io::Result<()>
where
    D: DoubleEndedIterator<Item = &'a (Backtrace, Metrics)>,
    F: Fn(&Metrics) -> usize,
{
    let last = flamegraph.next_back();

    for (backtrace, metrics) in flamegraph {
        write_backtrace(f, &backtrace)?;
        write!(f, " {}\n", metric(&metrics))?;
    }

    if let Some((backtrace, metrics)) = last {
        write_backtrace(f, &backtrace)?;
        write!(f, " {}", metric(&metrics))?;
    }

    Ok(())
}

unsafe impl<T: GlobalAlloc> GlobalAlloc for FlameAlloc<T> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ret = unsafe { self.inner.alloc(layout) };
        if !ret.is_null() {
            if let Some(_guard) = StateGuard::track() {
                if ENABLED.load(Ordering::Relaxed) {
                    let mut backtrace = ArrayVec::<usize, 64>::default();
                    unsafe {
                        let new_size = libc::backtrace(
                            backtrace.as_mut_ptr().cast(),
                            backtrace.capacity().try_into().unwrap(),
                        );
                        backtrace.set_len(new_size.try_into().unwrap());
                    };

                    let id = BacktraceId::from(backtrace.as_slice());

                    let mut flame_graph = init_thread_data();
                    let allocation_site = flame_graph
                        .entry(id)
                        .or_insert(AllocationSite::with_backtrace(backtrace));
                    allocation_site.alloc_calls += 1;
                    allocation_site.bytes_allocated += layout.size();
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
