// All metrics for the S3-FIFO crate

use metriken::*;

// hash table related
#[metric(
    name = "s3fifo_hash_tag_collision",
    description = "number of partial hash collisions"
)]
pub static HASH_TAG_COLLISION: Counter = Counter::new();

#[metric(
    name = "s3fifo_hash_insert",
    description = "number of inserts into the hash table"
)]
pub static HASH_INSERT: Counter = Counter::new();

#[metric(
    name = "s3fifo_hash_insert_ex",
    description = "number of hash table inserts which failed, likely due to capacity"
)]
pub static HASH_INSERT_EX: Counter = Counter::new();

#[metric(
    name = "s3fifo_hash_remove",
    description = "number of hash table entries which have been removed"
)]
pub static HASH_REMOVE: Counter = Counter::new();

#[metric(
    name = "s3fifo_hash_lookup",
    description = "total number of lookups against the hash table"
)]
pub static HASH_LOOKUP: Counter = Counter::new();

// item related
#[metric(
    name = "s3fifo_item_insert",
    description = "number of items inserted"
)]
pub static ITEM_INSERT: Counter = Counter::new();

#[metric(
    name = "s3fifo_item_replace",
    description = "number of items replaced"
)]
pub static ITEM_REPLACE: Counter = Counter::new();

#[metric(
    name = "s3fifo_item_delete",
    description = "number of items deleted"
)]
pub static ITEM_DELETE: Counter = Counter::new();

#[metric(
    name = "s3fifo_item_evict",
    description = "number of items evicted"
)]
pub static ITEM_EVICT: Counter = Counter::new();

#[metric(
    name = "s3fifo_item_expire",
    description = "number of items expired"
)]
pub static ITEM_EXPIRE: Counter = Counter::new();

#[metric(
    name = "s3fifo_item_promote",
    description = "number of items promoted from small to main queue"
)]
pub static ITEM_PROMOTE: Counter = Counter::new();

#[metric(
    name = "s3fifo_item_reinsert",
    description = "number of items reinserted at tail of main queue"
)]
pub static ITEM_REINSERT: Counter = Counter::new();

#[metric(
    name = "s3fifo_item_current",
    description = "current number of live items"
)]
pub static ITEM_CURRENT: Gauge = Gauge::new();

#[metric(
    name = "s3fifo_item_current_bytes",
    description = "current number of live bytes for storing items"
)]
pub static ITEM_CURRENT_BYTES: Gauge = Gauge::new();

#[metric(
    name = "s3fifo_ghost_hit",
    description = "number of ghost queue hits during insertion"
)]
pub static GHOST_HIT: Counter = Counter::new();
