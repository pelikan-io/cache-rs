//! Error types for segment operations.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum SegmentsError {
    #[error("invalid segment id")]
    BadSegmentId,
    #[error("item relink failure during compaction")]
    RelinkFailure,
    #[error("no segments available for eviction")]
    NoEvictableSegments,
    #[error("eviction failed")]
    EvictFailure,
}
