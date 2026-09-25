//! Synchronization primitives with optional model-checking backends.
//!
//! Three backends, selected by feature:
//! - default: `std::sync` (production)
//! - `loom`: exhaustive bounded model checking with weak-memory modeling
//! - `shuttle`: randomized scheduling under sequential consistency
//!
//! `loom` takes precedence when both model-checking features are enabled
//! (which happens under `--all-features`); see `build.rs` for the shared
//! `model_checking` cfg.

#[cfg(not(model_checking))]
pub use std::sync::atomic::{AtomicI32, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};

#[cfg(feature = "loom")]
pub use loom::sync::atomic::{AtomicI32, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};

#[cfg(all(feature = "shuttle", not(feature = "loom")))]
pub use shuttle::sync::atomic::{AtomicI32, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};

#[cfg(not(model_checking))]
pub use std::sync::Mutex;

#[cfg(feature = "loom")]
pub use loom::sync::Mutex;

#[cfg(all(feature = "shuttle", not(feature = "loom")))]
pub use shuttle::sync::Mutex;

#[cfg(not(model_checking))]
pub use std::sync::MutexGuard;

#[cfg(feature = "loom")]
pub use loom::sync::MutexGuard;

#[cfg(all(feature = "shuttle", not(feature = "loom")))]
pub use shuttle::sync::MutexGuard;

/// The segment free and spare queues: FIFO, multi-producer, multi-consumer.
///
/// In production a lock-free crossbeam `Injector`. A model checker cannot see
/// inside it -- its atomics are crossbeam's, not the backend's -- so any path
/// that reserves or returns a segment, which is every eviction path, could not
/// run under one. Under `model_checking` it is a backend `Mutex` around a
/// `VecDeque` with the same three operations and the same FIFO order, so the
/// model schedules every push and steal. What that model cannot check is the
/// `Injector`'s own lock-freedom, which is crossbeam's to verify.
#[cfg(not(model_checking))]
pub(crate) type SegmentQueue = crossbeam_deque::Injector<u32>;

#[cfg(model_checking)]
pub(crate) struct SegmentQueue(Mutex<std::collections::VecDeque<u32>>);

#[cfg(model_checking)]
impl SegmentQueue {
    pub(crate) fn new() -> Self {
        Self(Mutex::new(std::collections::VecDeque::new()))
    }

    pub(crate) fn push(&self, id: u32) {
        self.0.lock().unwrap().push_back(id);
    }

    /// `Injector::steal`'s contract minus `Retry`, which only a lock-free
    /// queue losing a race produces.
    pub(crate) fn steal(&self) -> crossbeam_deque::Steal<u32> {
        match self.0.lock().unwrap().pop_front() {
            Some(id) => crossbeam_deque::Steal::Success(id),
            None => crossbeam_deque::Steal::Empty,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

/// Number of randomized schedules each shuttle model explores.
///
/// Overridable via `SHUTTLE_ITERS` for deeper local soaks; the default is
/// sized so the whole shuttle suite stays in CI-friendly territory while
/// still being far past where the seeded bugs reproduced (the verify-ABA
/// spike failed in tens of schedules).
#[cfg(all(test, feature = "shuttle", not(feature = "loom")))]
pub(crate) fn shuttle_iters(default: usize) -> usize {
    match std::env::var("SHUTTLE_ITERS") {
        Ok(v) => v
            .parse()
            // A set-but-malformed override must fail loudly: falling back
            // silently would report a "deeper soak" that never ran.
            .unwrap_or_else(|_| panic!("SHUTTLE_ITERS must be a number, got {v:?}")),
        Err(_) => default,
    }
}
