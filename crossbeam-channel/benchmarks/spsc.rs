extern crate crossbeam;

use crossbeam::queue;
use std::thread;

mod message;

const MESSAGES: usize = 5_000_000;

const SEQ_CAPACITY: usize = MESSAGES;
const SPSC_CAPACITY: usize = 1024;

fn seq() {
    let (mut p, mut c) = queue::spsc::with_capacity(SEQ_CAPACITY);

    for i in 0..MESSAGES {
        p.try_push(message::new(i)).unwrap();
    }

    for _ in 0..MESSAGES {
        c.try_pop().unwrap();
    }
}

fn spsc() {
    let (mut p, mut c) = queue::spsc::with_capacity(SPSC_CAPACITY);

    crossbeam::scope(|scope| {
        scope.spawn(move |_| {
            let mut i = 0usize;
            while i < MESSAGES {
                if p.try_push(message::new(i)).is_ok() {
                    i += 1;
                } else {
                    thread::yield_now();
                }
            }
        });

        let mut i = 0usize;
        while i < MESSAGES {
            if let Ok(message) = c.try_pop() {
                debug_assert_eq!(i, message.0[0]);
                i += 1;
            } else {
                thread::yield_now();
            }
        }
    })
    .unwrap();
}

fn main() {
    macro_rules! run {
        ($name:expr, $f:expr) => {
            let now = ::std::time::Instant::now();
            $f;
            let elapsed = now.elapsed();
            println!(
                "{:25} {:15} {:7.3} sec",
                $name,
                "Rust spsc",
                elapsed.as_secs() as f64 + elapsed.subsec_nanos() as f64 / 1e9
            );
        };
    }

    run!("bounded_seq", seq());
    run!("bounded_spsc", spsc());
}
