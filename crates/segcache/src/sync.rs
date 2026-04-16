//! Synchronization primitives with optional loom support.

#[cfg(not(feature = "loom"))]
pub use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "loom")]
pub use loom::sync::atomic::{AtomicU64, Ordering};
