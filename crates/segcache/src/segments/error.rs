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
    #[error("segment size must be greater than item header overhead")]
    SegmentTooSmall,
    #[error("segment size must be a multiple of 8 bytes")]
    SegmentSizeUnaligned,
    #[error(
        "heap size ({heap_size}) must be a non-zero multiple of segment size ({segment_size})"
    )]
    InvalidHeapSize {
        heap_size: usize,
        segment_size: usize,
    },
    #[error(
        "heap requires {segments} segments, more than the {limit} a location's 20-bit segment id \
         can address; increase segment_size (or reduce heap_size)"
    )]
    TooManySegments { segments: usize, limit: usize },
    #[error("mmap allocation failed")]
    Mmap(#[from] std::io::Error),
}
