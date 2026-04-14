// Copyright 2021 Twitter, Inc.
// Copyright 2026 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

//! Possible errors returned by segment operations.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum SegmentsError {
    #[error("bad segment id")]
    BadSegmentId,
    #[error("item relink failure")]
    RelinkFailure,
    #[error("no evictable segments")]
    NoEvictableSegments,
    #[error("evict failure")]
    EvictFailure,
}
