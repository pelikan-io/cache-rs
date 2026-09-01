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
