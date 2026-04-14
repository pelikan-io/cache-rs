//! This crate implements the S3-FIFO cache eviction algorithm.
//!
//! S3-FIFO (Simple, Scalable caching with three Static FIFO queues) is a
//! cache eviction algorithm that uses three FIFO queues to achieve
//! state-of-the-art efficiency with low overhead. Most new items are
//! inserted into a small FIFO queue. Items that are accessed again are
//! promoted to a larger main FIFO queue. A ghost queue of recently evicted
//! item fingerprints allows quick re-admission upon reinsertion.
//!
//! A description of the design can be found here:
//! <https://s3fifo.com/>
//!
//! Goals:
//! * high-throughput item storage
//! * efficient cache eviction with low overhead
//! * scan resistance via quick demotion of one-hit wonders
//!
//! Non-goals:
//! * not designed for concurrent access
//!

// macro includes
#[macro_use]
extern crate log;

// external crate includes
use clocksource::coarse::Instant;

// submodules
mod builder;
mod error;
mod ghost;
mod hashtable;
mod item;
mod s3fifo;
mod slab;
mod value;

#[cfg(feature = "metrics")]
mod metrics;

// tests
#[cfg(test)]
mod tests;

// publicly exported items from submodules
pub use crate::s3fifo::S3Fifo;
pub use builder::Builder;
pub use error::S3FifoError;
pub use item::Item;
pub use value::Value;

// items from submodules which are imported for convenience to the crate level
pub(crate) use ghost::*;
pub(crate) use hashtable::*;
pub(crate) use item::*;
pub(crate) use slab::*;

#[cfg(feature = "metrics")]
pub(crate) use metrics::*;
