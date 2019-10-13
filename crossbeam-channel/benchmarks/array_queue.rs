extern crate crossbeam;

use crossbeam::queue::ArrayQueue;
use std::thread;

mod message;

const MESSAGES: usize = 5_000_000;
const THREADS: usize = 4;

const SEQ_CAPACITY: usize = MESSAGES;
const SPSC_CAPACITY: usize = 1024;

fn seq() {
    let q = ArrayQueue::new(SEQ_CAPACITY);

    for i in 0..MESSAGES {
        q.push(message::new(i)).unwrap();
    }

    for _ in 0..MESSAGES {
        q.pop().unwrap();
    }
}

fn spsc() {
    let q = ArrayQueue::new(SPSC_CAPACITY);

    crossbeam::scope(|scope| {
        scope.spawn(|_| {
            let mut i = 0usize;
            while i < MESSAGES {
                if q.push(message::new(i)).is_ok() {
                    i += 1;
                } else {
                    thread::yield_now();
                }
            }
        });

        let mut i = 0usize;
        while i < MESSAGES {
            if let Ok(message) = q.pop() {
                debug_assert_eq!(i, message.0[0]);
                i += 1;
            } else {
                thread::yield_now();
            }
        }
    })
    .unwrap();
}

fn mpsc() {
    let q = ArrayQueue::new(MESSAGES);

    crossbeam::scope(|scope| {
        for _ in 0..THREADS {
            scope.spawn(|_| {
                for i in 0..MESSAGES / THREADS {
                    q.push(message::new(i)).unwrap();
                }
            });
        }

        for _ in 0..MESSAGES {
            loop {
                if q.pop().is_err() {
                    thread::yield_now();
                } else {
                    break;
                }
            }
        }
    })
    .unwrap();
}

fn mpmc() {
    let q = ArrayQueue::new(MESSAGES);

    crossbeam::scope(|scope| {
        for _ in 0..THREADS {
            scope.spawn(|_| {
                for i in 0..MESSAGES / THREADS {
                    q.push(message::new(i)).unwrap();
                }
            });
        }

        for _ in 0..THREADS {
            scope.spawn(|_| {
                for _ in 0..MESSAGES / THREADS {
                    loop {
                        if q.pop().is_err() {
                            thread::yield_now();
                        } else {
                            break;
                        }
                    }
                }
            });
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
                "Rust ArrayQueue",
                elapsed.as_secs() as f64 + elapsed.subsec_nanos() as f64 / 1e9
            );
        };
    }

    run!("bounded_mpmc", mpmc());
    run!("bounded_mpsc", mpsc());
    run!("bounded_seq", seq());
    run!("bounded_spsc", spsc());
}
