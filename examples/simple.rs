use std::{
    alloc::System,
    thread::{self, sleep},
    time::Duration,
};

use mem_track::flame::FlameAlloc;

#[global_allocator]
static ALLOCATOR: FlameAlloc<System> = FlameAlloc::init(System);

fn main() {
    let mut handles = Vec::new();
    for id in 1..5 {
        let handle = thread::spawn(move || -> () {
            let mut data: Vec<usize> = Vec::with_capacity(1024);

            loop {
                for item in 0..255 {
                    data.push(item);
                }

                sleep(Duration::from_millis(id * 300));
            }
        });
        handles.push(handle);
    }

    let mut i = 0;
    loop {
        i += 1;
        sleep(Duration::from_millis(1_000));

        if i % 10 == 1 {
            println!("{}", ALLOCATOR.flame_graphs().0);
        }
    }
}
