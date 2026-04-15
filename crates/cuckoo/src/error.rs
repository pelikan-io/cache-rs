// Copyright 2025 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

//! Errors returned by the cuckoo cache API.

use thiserror::Error;

#[derive(Error, Debug, PartialEq, Eq, Copy, Clone)]
/// Possible errors returned by the cuckoo cache API.
pub enum CuckooCacheError {
    #[error("item oversized ({size} bytes, max {max} bytes)")]
    ItemOversized { size: usize, max: usize },
    #[error("item not found")]
    NotFound,
    #[error("item is not numeric")]
    NotNumeric,
}
