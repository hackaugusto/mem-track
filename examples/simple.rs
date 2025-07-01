use std::{
    alloc::System,
    thread::{self, sleep},
    time::Duration,
};

use mem_track::peak::{BytesInUseTracker, global_reset, thread_peak};

#[global_allocator]
static ALLOCATOR: BytesInUseTracker<System> = BytesInUseTracker::init(System);

fn main() {
    let mut handles = Vec::new();
    for id in 1..5 {
        let handle = thread::spawn(move || -> () {
            let mut data: Vec<usize> = Vec::with_capacity(1024);

            loop {
                for item in 0..255 {
                    data.push(item);
                }

                println!("{id}: {} {}", thread_peak(), ALLOCATOR.peak());
                sleep(Duration::from_millis(id * 300));
            }
        });
        handles.push(handle);
    }

    let mut i = 0;
    loop {
        i += 1;
        println!("global: {}", ALLOCATOR.peak());
        sleep(Duration::from_millis(1_000));

        if i % 10 == 1 {
            global_reset();
        }
    }
}
