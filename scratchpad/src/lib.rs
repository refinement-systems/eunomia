//! Scratchpad: Loom and Shuttle concurrency-testing demonstrations.
//!
//! These tools are the spec §6 concurrency verification tier; `ipc/` is the
//! intended first real target once its blocking send/recv are implemented.
//! This crate shows the usage patterns for both.
//!
//! Run Shuttle (randomised scheduler):   cargo test -p scratchpad
//! Run Loom (exhaustive deterministic):  RUSTFLAGS="--cfg loom" cargo test -p scratchpad

// ── Loom tests (exhaustive interleaving exploration) ─────────────────────────
//
// loom is a dev-dep, so its types are only accessible inside #[cfg(test)].
// The slot type is defined here with loom::sync primitives so loom can track
// every acquire/release and explore all valid orderings exhaustively.
// loom::model() runs the closure under every valid interleaving and fails on
// any assertion violation.

#[cfg(all(test, loom))]
mod loom_tests {
    use loom::sync::{Arc, Mutex};
    use loom::thread;

    struct Slot<T> {
        inner: Mutex<Option<T>>,
    }

    impl<T: Send> Slot<T> {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                inner: Mutex::new(None),
            })
        }
        fn put(&self, val: T) -> bool {
            let mut g = self.inner.lock().unwrap();
            if g.is_none() {
                *g = Some(val);
                true
            } else {
                false
            }
        }
        fn take(&self) -> Option<T> {
            self.inner.lock().unwrap().take()
        }
    }

    #[test]
    fn two_writers_one_slot() {
        loom::model(|| {
            let slot = Slot::new();
            let s1 = slot.clone();
            let s2 = slot.clone();

            let t1 = thread::spawn(move || s1.put(1u32));
            let t2 = thread::spawn(move || s2.put(2u32));

            let w1 = t1.join().unwrap();
            let w2 = t2.join().unwrap();

            // Exactly one writer wins the race across every possible interleaving.
            assert!(w1 ^ w2, "expected exactly one put() to succeed");
            assert!(slot.take().is_some(), "slot should hold the winning value");
        });
    }
}

// ── Shuttle tests (randomised scheduler) ─────────────────────────────────────
//
// Shuttle doesn't intercept std imports; tests use shuttle::sync/thread
// directly.  check_random runs the closure under 200 randomly-chosen
// schedules — complementary to Loom's exhaustive (but bounded) exploration.

#[cfg(test)]
mod shuttle_tests {
    use shuttle::sync::{Arc, Mutex};
    use shuttle::thread;

    #[test]
    fn two_writers_one_slot() {
        shuttle::check_random(
            || {
                let slot: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
                let s1 = slot.clone();
                let s2 = slot.clone();

                let t1 = thread::spawn(move || {
                    let mut g = s1.lock().unwrap();
                    if g.is_none() {
                        *g = Some(1);
                        true
                    } else {
                        false
                    }
                });
                let t2 = thread::spawn(move || {
                    let mut g = s2.lock().unwrap();
                    if g.is_none() {
                        *g = Some(2);
                        true
                    } else {
                        false
                    }
                });

                let w1 = t1.join().unwrap();
                let w2 = t2.join().unwrap();

                assert!(w1 ^ w2, "expected exactly one put() to succeed");
                assert!(slot.lock().unwrap().is_some());
            },
            200,
        );
    }
}
