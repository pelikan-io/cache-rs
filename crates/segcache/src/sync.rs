//! Synchronization primitives with optional loom support.

#[cfg(not(feature = "loom"))]
pub use std::sync::atomic::{AtomicI32, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};

#[cfg(feature = "loom")]
pub use loom::sync::atomic::{AtomicI32, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};

// Unused until the hashtable's striped insert locks land; the allow is
// removed by that change.
#[cfg(not(feature = "loom"))]
#[allow(unused_imports)]
pub use std::sync::Mutex;

#[cfg(feature = "loom")]
#[allow(unused_imports)]
pub use loom::sync::Mutex;
