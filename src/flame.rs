use std::{
    alloc::{GlobalAlloc, Layout},
    cell::OnceCell,
    collections::BTreeMap,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use backtrace::Backtrace;

// Type description:
// - LazyLock: Used to wrap a `Mutex<Vec<_>>` and expose it as a static.
// - Mutex: Used to synchronize calls to `Vec<_>` by all threads.
// - Vec: Each entry corresponds to one thread state.
static THREADS: LazyLock<Mutex<Vec<SharedData>>> = LazyLock::new(|| Default::default());

// Type description:
// - Arc: Used to make the data send'able.
// - Mutex: allow concurrent reads / writes.
// - BTreeMap: the flame graph data.
type SharedData = Arc<Mutex<BTreeMap<BacktraceId, AllocationSite>>>;

thread_local! {
    // OnceCell: Used to guarantee the `SharedData` is cloned to the `THREADS` static only once.
    static ALLOCATION_SITES: OnceCell<SharedData> = OnceCell::new();
}

fn init_btree(once: &OnceCell<SharedData>) -> &SharedData {
    once.get_or_init(|| {
        let map = Arc::new(Mutex::new(BTreeMap::default()));
        let mut threads = THREADS
            .lock()
            .expect("Should never panic while holding the lock");
        threads.push(map.clone());
        map
    })
}

/// A wrapper struct to uniquely identify an allocation size.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct BacktraceId(Vec<usize>);

impl From<&Backtrace> for BacktraceId {
    fn from(value: &Backtrace) -> Self {
        let instruction_pointers = value
            .frames()
            .iter()
            .map(|frame| frame.ip() as usize)
            .collect();
        BacktraceId(instruction_pointers)
    }
}

#[derive(Debug)]
struct AllocationSite {
    /// Number of allocations done at this call site.
    alloc_calls: AtomicUsize,

    /// Number of bytes allocated at this call site.
    allocated: AtomicUsize,

    /// The corresponding [Backtrace] for this site.
    backtrace: Backtrace,
}
impl AllocationSite {
    fn with_backtrace(backtrace: Backtrace) -> AllocationSite {
        AllocationSite {
            alloc_calls: AtomicUsize::default(),
            allocated: AtomicUsize::default(),
            backtrace,
        }
    }
}

struct FlameAlloc<T> {
    inner: T,
}

unsafe impl<T: GlobalAlloc> GlobalAlloc for FlameAlloc<T> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ret = unsafe { self.inner.alloc(layout) };
        if !ret.is_null() {
            let backtrace = Backtrace::new_unresolved();
            let id = BacktraceId::from(&backtrace);

            ALLOCATION_SITES.with(|once| {
                let mut tree = init_btree(once)
                    .lock()
                    .expect("Should never panic while holding the lock");

                let allocation_site = tree
                    .entry(id)
                    .or_insert(AllocationSite::with_backtrace(backtrace));
                allocation_site.alloc_calls.fetch_add(1, Ordering::Relaxed);
                allocation_site
                    .allocated
                    .fetch_add(layout.size(), Ordering::Relaxed);
            })
        }
        ret
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe {
            self.inner.dealloc(ptr, layout);
        }
    }
}
