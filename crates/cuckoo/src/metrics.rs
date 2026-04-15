// Copyright 2025 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

//! Metrics for the cuckoo cache.

use metriken::*;

// operation counters

#[metric(
    name = "get",
    description = "number of get operations",
    metadata = { engine = "cuckoo" }
)]
pub static CUCKOO_GET: Counter = Counter::new();

#[metric(
    name = "get_key_hit",
    description = "number of get operations that found the key",
    metadata = { engine = "cuckoo" }
)]
pub static CUCKOO_GET_KEY_HIT: Counter = Counter::new();

#[metric(
    name = "get_key_miss",
    description = "number of get operations that did not find the key",
    metadata = { engine = "cuckoo" }
)]
pub static CUCKOO_GET_KEY_MISS: Counter = Counter::new();

#[metric(
    name = "insert",
    description = "number of insert operations",
    metadata = { engine = "cuckoo" }
)]
pub static CUCKOO_INSERT: Counter = Counter::new();

#[metric(
    name = "insert_ex",
    description = "number of insert operations that failed",
    metadata = { engine = "cuckoo" }
)]
pub static CUCKOO_INSERT_EX: Counter = Counter::new();

#[metric(
    name = "update",
    description = "number of in-place update operations on existing keys",
    metadata = { engine = "cuckoo" }
)]
pub static CUCKOO_UPDATE: Counter = Counter::new();

#[metric(
    name = "delete",
    description = "number of delete operations",
    metadata = { engine = "cuckoo" }
)]
pub static CUCKOO_DELETE: Counter = Counter::new();

#[metric(
    name = "displace",
    description = "number of item displacements during insertion",
    metadata = { engine = "cuckoo" }
)]
pub static CUCKOO_DISPLACE: Counter = Counter::new();

#[metric(
    name = "item_evict",
    description = "number of items evicted to make room for new items",
    metadata = { engine = "cuckoo" }
)]
pub static ITEM_EVICT: Counter = Counter::new();

#[metric(
    name = "item_expire",
    description = "number of items lazily expired during operations",
    metadata = { engine = "cuckoo" }
)]
pub static ITEM_EXPIRE: Counter = Counter::new();

// storage gauges

#[metric(
    name = "item_current",
    description = "current number of live items",
    metadata = { engine = "cuckoo" }
)]
pub static ITEM_CURRENT: Gauge = Gauge::new();

#[metric(
    name = "item_key_byte",
    description = "current total key bytes stored",
    metadata = { engine = "cuckoo" }
)]
pub static ITEM_KEY_BYTE: Gauge = Gauge::new();

#[metric(
    name = "item_val_byte",
    description = "current total value bytes stored",
    metadata = { engine = "cuckoo" }
)]
pub static ITEM_VAL_BYTE: Gauge = Gauge::new();

#[metric(
    name = "item_data_byte",
    description = "current total data bytes stored (key + value)",
    metadata = { engine = "cuckoo" }
)]
pub static ITEM_DATA_BYTE: Gauge = Gauge::new();
