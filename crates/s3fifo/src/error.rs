//! Top-level errors that will be returned to a caller of this library.

use thiserror::Error;

#[derive(Error, Debug, PartialEq, Eq, Copy, Clone)]
/// Possible errors returned by the top-level API
pub enum S3FifoError {
    #[error("hashtable insert exception")]
    HashTableInsertEx,
    #[error("item oversized ({size:?} bytes)")]
    ItemOversized { size: usize },
    #[error("no free space")]
    NoFreeSpace,
    #[error("item exists")]
    Exists,
    #[error("item not found")]
    NotFound,
    #[error("item is not numeric")]
    NotNumeric,
}
