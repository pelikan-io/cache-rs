// Copyright 2025 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

//! Metrics for the cuckoo cache.

use metriken::*;

// operation counters

#[metric(name = "cuckoo_get", description = "number of get operations")]
pub static CUCKOO_GET: Counter = Counter::new();

#[metric(
    name = "cuckoo_get_key_hit",
    description = "number of get operations that found the key"
)]
pub static CUCKOO_GET_KEY_HIT: Counter = Counter::new();

#[metric(
    name = "cuckoo_get_key_miss",
    description = "number of get operations that did not find the key"
)]
pub static CUCKOO_GET_KEY_MISS: Counter = Counter::new();

#[metric(name = "cuckoo_insert", description = "number of insert operations")]
pub static CUCKOO_INSERT: Counter = Counter::new();

#[metric(
    name = "cuckoo_insert_ex",
    description = "number of insert operations that failed"
)]
pub static CUCKOO_INSERT_EX: Counter = Counter::new();

#[metric(
    name = "cuckoo_update",
    description = "number of in-place update operations on existing keys"
)]
pub static CUCKOO_UPDATE: Counter = Counter::new();

#[metric(name = "cuckoo_delete", description = "number of delete operations")]
pub static CUCKOO_DELETE: Counter = Counter::new();

#[metric(
    name = "cuckoo_displace",
    description = "number of item displacements during insertion"
)]
pub static CUCKOO_DISPLACE: Counter = Counter::new();

#[metric(
    name = "item_evict",
    description = "number of items evicted to make room for new items"
)]
pub static ITEM_EVICT: Counter = Counter::new();

#[metric(
    name = "item_expire",
    description = "number of items lazily expired during operations"
)]
pub static ITEM_EXPIRE: Counter = Counter::new();

// storage gauges

#[metric(name = "item_current", description = "current number of live items")]
pub static ITEM_CURRENT: Gauge = Gauge::new();

#[metric(
    name = "item_key_byte",
    description = "current total key bytes stored"
)]
pub static ITEM_KEY_BYTE: Gauge = Gauge::new();

#[metric(
    name = "item_val_byte",
    description = "current total value bytes stored"
)]
pub static ITEM_VAL_BYTE: Gauge = Gauge::new();

#[metric(
    name = "item_data_byte",
    description = "current total data bytes stored (key + value)"
)]
pub static ITEM_DATA_BYTE: Gauge = Gauge::new();
