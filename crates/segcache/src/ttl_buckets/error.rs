// Copyright 2021 Twitter, Inc.
// Copyright 2026 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

use thiserror::Error;

#[derive(Error, Debug)]
pub enum TtlBucketsError {
    #[error("item is oversized ({size:?} bytes)")]
    ItemOversized { size: usize },
    #[error("ttl bucket expansion failed, no free segments")]
    NoFreeSegments,
}
