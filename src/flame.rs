use std::{
    alloc::{GlobalAlloc, Layout},
    borrow::Cow,
    collections::{BTreeMap, HashMap, btree_map::Entry, hash_map},
    env,
    hash::{DefaultHasher, Hash, Hasher},
    io,
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use addr2line::{Frame, Loader};
use arrayvec::ArrayVec;
use findshlibs::{Bias, Segment, SharedLibrary, TargetSharedLibrary};
use gimli::Reader;

use crate::guard::StateGuard;

// Data tracked to generate the frame graph
type AllocationData = BTreeMap<BacktraceId, AllocationSite>;

// A flame graph maps a stack trace to a counter / metrics.
type FlameGraph = Vec<(Backtrace, Metrics)>;

// Type used to defined a backtrace
type Backtrace = ArrayVec<usize, 64>;

// Value used to represent an unknwon function name
const UNKNOWN: &str = "??";

struct PeakData {
    active_allocs: HashMap<usize, (usize, BacktraceId)>,
    current_flame: AllocationData,
    peak_flame: AllocationData,
    current_usage: usize,
    peak_usage: usize,
    dirty: bool,
}

impl Default for PeakData {
    fn default() -> Self {
        Self {
            active_allocs: HashMap::default(),
            current_flame: AllocationData::default(),
            peak_flame: AllocationData::default(),
            current_usage: 0,
            peak_usage: 0,
            dirty: false,
        }
    }
}

static PEAK_DATA: LazyLock<Mutex<PeakData>> = LazyLock::new(|| Mutex::new(PeakData::default()));

// Used to enable/disable flame graph generation at runtime.
static ENABLED: LazyLock<AtomicBool> = LazyLock::new(|| {
    let initial = env::var("FLAMEGRAPH_STARTS_ENABLED")
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    AtomicBool::new(initial)
});

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

#[derive(Debug, Clone)]
struct AllocationSite {
    /// Number of allocations done at this call site.
    alloc_calls: usize,

    /// Number of bytes allocated at this call site.
    bytes_allocated: usize,

    /// The corresponding [Backtrace] for this site.
    backtrace: ArrayVec<usize, 64>,
}

#[derive(Debug, Clone)]
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


/// Checks if the flame graph generation is enabled.
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
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

    let data = PEAK_DATA
        .lock()
        .expect("Should never panic while holding the lock");

    let source = if data.dirty {
        &data.current_flame
    } else {
        &data.peak_flame
    };

    source
        .iter()
        .map(|(_key, value)| {
            let metrics = Metrics {
                alloc_calls: value.alloc_calls,
                bytes_allocated: value.bytes_allocated,
            };

            (value.backtrace.clone(), metrics)
        })
        .collect()
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

    /// Checks if the flame graph generation is enabled.
    pub fn is_enabled(&self) -> bool {
        is_enabled()
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
    avma_start: usize,

    /// The avma end of this segment
    avma_end: usize,

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
                let avma_start = usize::from(segment.actual_virtual_memory_address(shlib));
                let avma_end = avma_start + segment.len();

                segments.push(SegmentDetails {
                    avma_start,
                    avma_end,
                    loader: loader_pos,
                });
            }
        }
        Err(_err) => {}
    });
    segments.sort_by_key(|seg| seg.avma_start);

    Details { loaders, segments }
}

/// Returns a reference to the frame's name.
fn frame_to_symbol<'frame, R: Reader>(frame: &'frame Frame<'_, R>) -> Cow<'frame, str> {
    match frame.function.as_ref() {
        Some(function) => match function.demangle() {
            // symbol lifetime is bound to the frame, which is a stack variable in the caller
            Ok(symbol) => symbol,
            Err(_err) => match function.raw_name() {
                Ok(symbol) => symbol,
                Err(_err) => Cow::Borrowed(UNKNOWN),
            },
        },
        None => Cow::Borrowed(UNKNOWN),
    }
}

/// Resolve the function names corresponding to the instruction pointer.
///
/// Given an advertised virtual memory address and debug info loaders this function
/// resolves the names corresponding to the `avma` and write it to `f`.
///
/// NOTE: A single `avma` address may correspond to multiple frames due to inlining,
/// this function will return a `;` string with the frame names in that case.
fn resolve_frame(avma: usize, details: &Details) -> Cow<'static, str> {
    // This function returns a Cow because:
    //
    // - It wont need to allocate when the symbol is unknown
    // - It can reuse allocations from demangled symbols
    //
    // Unfortunately this can't reference the DWARF symbols directly, since the
    // available APIs seem to borrow from local variables.

    let segment = match details
        .segments
        .binary_search_by_key(&avma, |seg| seg.avma_start)
    {
        Ok(v) => v,
        Err(v) => v - 1,
    };
    let segment = &details.segments[segment];

    // The address is not contained in any of the known segments.
    if avma > segment.avma_end {
        return Cow::Borrowed(UNKNOWN);
    }

    let loader = &details.loaders[segment.loader];

    let svma = u64::try_from(avma.wrapping_sub(loader.bias.0)).unwrap();
    let mut frames = match loader.loader.find_frames(svma) {
        Ok(frames) => frames,
        Err(_err) => {
            return Cow::Owned(format!("{:#}", svma));
        }
    };

    let mut resolved = Vec::with_capacity(8);
    loop {
        match frames.next() {
            Ok(Some(frame)) => {
                resolved.push(frame);
            }
            Ok(None) => {
                if resolved.is_empty() {
                    // If no frame was found, fallback to resolving the symbol
                    let symbol = loader
                        .loader
                        .find_symbol(svma)
                        .map(|v| addr2line::demangle_auto(Cow::Borrowed(v), None));

                    if let Some(symbol) = symbol {
                        // symbol lifetime is bound to the loader, reuse
                        // allocation if demangling was performed
                        return Cow::Owned(symbol.into_owned());
                    } else {
                        return Cow::Borrowed(UNKNOWN);
                    }
                }
                break;
            }
            Err(_err) => {
                return Cow::Borrowed(UNKNOWN);
            }
        }
    }

    if resolved.len() == 0 {
        // This shouldn't happen, the iterator would have returned Ok(None) and
        // it would be handled above.
        return Cow::Borrowed(UNKNOWN);
    } else if resolved.len() == 1 {
        let frame = resolved.pop().unwrap();
        Cow::Owned(frame_to_symbol(&frame).into_owned())
    } else {
        let mut iter = resolved.iter();
        let frame = iter.next().unwrap(); // length checked above
        let mut result = frame_to_symbol(frame).into_owned();
        for frame in iter {
            result.push_str(";");
            result.push_str(frame_to_symbol(frame).as_ref());
        }
        Cow::Owned(result)
    }
}

/// Resolve and write the backtrace to the formatter
fn write_backtrace(
    details: &Details,
    f: &mut impl io::Write,
    trace: &Backtrace,
    symbols_cache: &mut HashMap<usize, Cow<'static, str>>,
) -> io::Result<()> {
    let mut iter = trace.iter();

    if let Some(avma) = iter.next() {
        match symbols_cache.entry(*avma) {
            hash_map::Entry::Occupied(occupied_entry) => {
                write!(f, "{}", occupied_entry.get().as_ref())?;
            }
            hash_map::Entry::Vacant(vacant_entry) => {
                let symbols = resolve_frame(*avma, &details);
                write!(f, "{}", symbols.as_ref())?;
                vacant_entry.insert(symbols);
            }
        }
    }

    for avma in iter {
        match symbols_cache.entry(*avma) {
            hash_map::Entry::Occupied(occupied_entry) => {
                write!(f, ";{}", occupied_entry.get().as_ref())?;
            }
            hash_map::Entry::Vacant(vacant_entry) => {
                let symbols = resolve_frame(*avma, &details);
                write!(f, ";{}", symbols.as_ref())?;
                vacant_entry.insert(symbols);
            }
        }
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
    F: Fn(&Metrics) -> Option<usize>,
{
    let details = get_loaders();
    let last = flamegraph.next_back();
    let mut symbols_cache = HashMap::new();

    for (backtrace, metrics) in flamegraph {
        if let Some(value) = metric(&metrics) {
            write_backtrace(&details, f, &backtrace, &mut symbols_cache)?;
            write!(f, " {}\n", value)?;
        }
    }

    if let Some((backtrace, metrics)) = last {
        if let Some(value) = metric(&metrics) {
            write_backtrace(&details, f, &backtrace, &mut symbols_cache)?;
            write!(f, " {}", value)?;
        }
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

                    let mut data = PEAK_DATA
                        .lock()
                        .expect("Should never panic while holding the lock");
                    let size = layout.size();
                    data.active_allocs.insert(ret as usize, (size, id));

                    let allocation_site = data
                        .current_flame
                        .entry(id)
                        .or_insert(AllocationSite::with_backtrace(backtrace));
                    allocation_site.alloc_calls += 1;
                    allocation_site.bytes_allocated += size;

                    data.current_usage += size;
                    if data.current_usage > data.peak_usage {
                        data.peak_usage = data.current_usage;
                        data.dirty = true;
                    }
                }
            }
        }
        ret
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if let Some(_guard) = StateGuard::track() {
            if ENABLED.load(Ordering::Relaxed) {
                let mut data = PEAK_DATA
                    .lock()
                    .expect("Should never panic while holding the lock");

                if data.dirty {
                    data.peak_flame = data.current_flame.clone();
                    data.dirty = false;
                }

                if let Some((size, id)) = data.active_allocs.remove(&(ptr as usize)) {
                    data.current_usage -= size;
                    if let Entry::Occupied(mut entry) = data.current_flame.entry(id) {
                        let site = entry.get_mut();
                        site.alloc_calls -= 1;
                        site.bytes_allocated -= size;
                        if site.alloc_calls == 0 {
                            entry.remove();
                        }
                    }
                }
            }
        }

        unsafe {
            self.inner.dealloc(ptr, layout);
        }
    }
}
