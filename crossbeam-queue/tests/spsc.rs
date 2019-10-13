extern crate crossbeam_queue;
extern crate crossbeam_utils;
extern crate rand;

use crossbeam_queue::spsc::{self, TryPushError};
use crossbeam_utils::thread::scope;
use rand::{thread_rng, Rng};
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn smoke() {
    let (mut p, mut c) = spsc::with_capacity(1);

    assert!(p.try_push(7).is_ok());
    assert_eq!(Ok(7), c.try_pop());

    assert!(p.try_push(8).is_ok());
    assert_eq!(Err(TryPushError::Full(9)), p.try_push(9));
    assert_eq!(Ok(8), c.try_pop());
    assert!(c.try_pop().is_err());
}

#[test]
fn capacity() {
    for i in 0..10 {
        let (p, c) = spsc::with_capacity::<i32>(i);
        assert_eq!(p.capacity(), i);
        assert_eq!(c.capacity(), i);
    }
}

#[test]
fn parallel() {
    const COUNT: usize = 100_000;

    let (mut p, mut c) = spsc::with_capacity(3);

    scope(|s| {
        s.spawn(move |_| {
            for i in 0..COUNT {
                loop {
                    if let Ok(x) = c.try_pop() {
                        assert_eq!(x, i);
                        break;
                    }
                }
            }
            assert!(c.try_pop().is_err());
        });

        s.spawn(move |_| {
            for i in 0..COUNT {
                while p.try_push(i).is_err() {}
            }
        });
    })
    .unwrap();
}

#[test]
fn drops() {
    const RUNS: usize = 100;

    static DROPS: AtomicUsize = AtomicUsize::new(0);

    #[derive(Debug, PartialEq)]
    struct DropCounter;

    impl Drop for DropCounter {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::SeqCst);
        }
    }

    let mut rng = thread_rng();

    for _ in 0..RUNS {
        let steps = rng.gen_range(0, 10_000);
        let additional = rng.gen_range(0, 50);

        DROPS.store(0, Ordering::SeqCst);
        let (mut p, mut c) = spsc::with_capacity(50);

        let mut p = scope(|s| {
            s.spawn(move |_| {
                for _ in 0..steps {
                    while c.try_pop().is_err() {}
                }
            });

            s.spawn(move |_| {
                for _ in 0..steps {
                    while p.try_push(DropCounter).is_err() {
                        DROPS.fetch_sub(1, Ordering::SeqCst);
                    }
                }
                p
            })
            .join()
            .unwrap()
        })
        .unwrap();

        for _ in 0..additional {
            assert!(p.try_push(DropCounter).is_ok());
        }

        assert_eq!(DROPS.load(Ordering::SeqCst), steps);
        drop(p);
        assert_eq!(DROPS.load(Ordering::SeqCst), steps + additional);
    }
}
