## Memory tracker

This is a small library providing memory allocator wrappers to track memory usage.

## Features

- Measure allocation's high water mark per thread and globally. This can be reset
  to support processes with stages and identify highest allocation stages.
- Produce flame graphs for the number of allocations and bytes allocated per thread
  and globally.
- Support for multiple platforms with a pure Rust wrapper of global allocators. 

## Other tools and notes

LD_PRELOAD tools are linux only:

- [bytehound](https://github.com/koute/bytehound) tracks indivudal allocations with timestamp
  information, and contains a custom UI to visualize and query the data.
- [heaptrack](https://github.com/KDE/heaptrack) ligthweight alternative to valgrind's massif,
  has a GUI to visualize the data, roughly same functionality.

Interpreter based tools:

- [valgrind](https://valgrind.org) (port: [valgrind-macos](https://github.com/LouisBrunner/valgrind-macos)),
  much more advanced tooling, this tool is similar to massif. Limited to architectures the interpreter supports.
  - [dhat](https://lib.rs/crates/dhat) this rust library is also limited to the supported architectures of valgrind.

Allocators with metrics:

- [talc](https://crates.io/crates/talc) custom allocator with metrics.
- [tikv-jemallocator](https://crates.io/crates/tikv-jemallocator)
  [tikv-jemallco-sys](https://crates.io/crates/tikv-jemalloc-sys)
  [tikv-jemalloc-ctl](https://crates.io/crates/tikv-jemalloc-ctl) jemalloc specific, doesn't support flame graphs.
- [trallocator](https://github.com/xgroleau/trallocator)
- [alloc-track](https://github.com/Protryon/alloc-track)
- [leaktracer](https://github.com/veeso/leaktracer)

MacOS:

- [Instruments](https://developer.apple.com/tutorials/instruments) doesn't do well with long traces.

CPU focused:

- [measureme](https://lib.rs/crates/measureme) cpu / instruction count focused.
- [coz](https://github.com/plasma-umass/coz) throughput / latency focused.
- [samply](https://github.com/mstange/samply) sampling flame graph integrated with https://profiler.firefox.com.
- [pprof-rs](https://github.com/tikv/pprof-rs) sampling cpu profiler.

Other tools:

- [tracy](https://github.com/wolfpld/tracy) + [rust_tracy_client](https://github.com/nagisa/rust_tracy_client),
  visualization tool, requires collectors.

Unwind libraries:

- [mini-backtrace](https://crates.io/crates/mini-backtrace), wraps llvm-libunwind and allows to recover the instruction pointers.
  need additional work to retrieve the function names [needs to configure include paths to find headers]
- [framehop](https://crates.io/crates/framehop) library used by samply.
- [unwind](https://crates.io/crates/unwind) wrapper around gnu's libunwind [needs libunwind-generic dependency].
- [libunwind-rs](https://crates.io/crates/libunwind-rs) another wraper around gnu's libunwind.
- [unwinding](https://crates.io/crates/unwinding) implements stack unwind, doesnt seem to be useful for inspection [compilation error, missing mod].
- [vex-libunwind](https://crates.io/crates/vex-libunwind), lack safe api for get_proc_info [compilation errors with constants]
