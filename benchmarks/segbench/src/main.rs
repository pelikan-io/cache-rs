//! Scalability bench: Arc<Segcache> shared across N threads.
//! Usage: segbench <threads> <write_pct> <dist: uniform|zipf> <warmup_s> <measure_s> [mode: base|stripe]
//!
//! mode=stripe emulates striped active tails through the public API: thread t
//! writes with TTL = 1000 + 8t seconds, landing in its own 8s-wide tier-1 TTL
//! bucket, so reservation CASes hit per-thread tail segments instead of one
//! shared tail. TTLs are ~17 min — no expiry occurs within a run.
//! Prints CSV: threads,write_pct,dist,mode,mops

use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};
use rand_distr::{Distribution, Zipf};
use segcache::{Policy, Segcache};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

const MB: usize = 1024 * 1024;
const NKEYS: usize = 1_000_000;
const VLEN: usize = 128;

static SPIN_PER_NS: std::sync::OnceLock<f64> = std::sync::OnceLock::new();

/// Calibrated busy-work: simulate non-storage per-op cost (parse, socket IO).
fn spin_ns(ns: u64) {
    if ns == 0 {
        return;
    }
    let per = *SPIN_PER_NS.get_or_init(|| {
        let n: u64 = 50_000_000;
        let t0 = Instant::now();
        let mut acc: u64 = 0x9E3779B9;
        for i in 0..n {
            acc = std::hint::black_box(acc.wrapping_mul(6364136223846793005).wrapping_add(i));
        }
        std::hint::black_box(acc);
        n as f64 / t0.elapsed().as_nanos() as f64
    });
    let iters = (ns as f64 * per) as u64;
    let mut acc: u64 = 0x9E3779B9;
    for i in 0..iters {
        acc = std::hint::black_box(acc.wrapping_mul(6364136223846793005).wrapping_add(i));
    }
    std::hint::black_box(acc);
}

fn key_bytes(i: usize) -> [u8; 16] {
    let mut k = [b'0'; 16];
    k[..8].copy_from_slice(b"segbench");
    k[8..].copy_from_slice(format!("{:08}", i).as_bytes());
    k
}

fn shard_cache(nshards: usize) -> Segcache {
    let shift = (nshards as u32).ilog2();
    let heap = (1024 * MB / nshards) / MB * MB; // whole segments per shard
    Segcache::builder()
        .heap_size(heap)
        .segment_size(MB as i32)
        .hash_power((22 - shift) as u8)
        .overflow_factor(1.0)
        .eviction(Policy::Merge {
            max: 8,
            merge: 4,
            compact: 2,
        })
        .build()
        .expect("build shard")
}

/// mode=shard: T threads, each owning a private cache and the keys with
/// idx % T == t (hash-spread, so zipf heads distribute like hash routing).
/// Upper bound for sharded single-writer: no routing cost, balanced load.
fn run_shard(threads: usize, write_pct: u32, dist: &str, warmup: Duration, measure: Duration) {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    let stop = Arc::new(AtomicBool::new(false));
    let measuring = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(threads + 1));
    let total_ops = Arc::new(AtomicU64::new(0));
    let value = vec![0xABu8; VLEN];
    let local_n = NKEYS / threads;

    let mut handles = Vec::new();
    for t in 0..threads {
        let (stop, measuring, barrier, total_ops, value) = (
            stop.clone(),
            measuring.clone(),
            barrier.clone(),
            total_ops.clone(),
            value.clone(),
        );
        let dist = dist.to_string();
        handles.push(std::thread::spawn(move || {
            let cache = shard_cache(threads);
            for r in 0..local_n {
                let idx = t + threads * r;
                cache
                    .insert(&key_bytes(idx), &value[..], None, Duration::ZERO)
                    .expect("prefill");
            }
            let mut rng = SmallRng::seed_from_u64(0x5A4D ^ ((t as u64) << 32) ^ t as u64);
            let zipf = Zipf::new(local_n as f64, 0.99).unwrap();
            let mut ops: u64 = 0;
            let mut start_snap: Option<u64> = None;
            barrier.wait();
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let r = if dist == "zipf" {
                    (zipf.sample(&mut rng) as usize - 1) % local_n
                } else {
                    rng.random_range(0..local_n)
                };
                let key = key_bytes(t + threads * r);
                if rng.random_range(0..100) < write_pct {
                    let _ = cache.insert(&key, &value[..], None, Duration::ZERO);
                } else {
                    let item = cache.get(&key);
                    std::hint::black_box(&item);
                }
                ops += 1;
                if ops.is_multiple_of(64) {
                    match (measuring.load(Ordering::Relaxed), start_snap) {
                        (true, None) => start_snap = Some(ops),
                        (false, Some(s)) => {
                            total_ops.fetch_add(ops - s, Ordering::Relaxed);
                            start_snap = None;
                        }
                        _ => {}
                    }
                }
            }
            if let Some(s) = start_snap {
                total_ops.fetch_add(ops - s, Ordering::Relaxed);
            }
        }));
    }
    barrier.wait();
    std::thread::sleep(warmup);
    measuring.store(true, Ordering::Relaxed);
    let t0 = Instant::now();
    std::thread::sleep(measure);
    let elapsed = t0.elapsed();
    measuring.store(false, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(50));
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        h.join().unwrap();
    }
    let mops = total_ops.load(Ordering::Relaxed) as f64 / elapsed.as_secs_f64() / 1e6;
    println!("{},{},{},shard,{:.3}", threads, write_pct, dist, mops);
}

/// mode=delegate: threads/2 workers route every op over bounded channels to
/// threads/2 owner threads (key idx % S), which execute on private shards and
/// reply. Workers keep a 64-deep pipeline. Global zipf → real hot-shard skew.
struct DelegateOpts {
    /// Requests per channel message (1 = unbatched).
    batch: usize,
    /// Owner threads; 0 means "half the thread budget".
    owners: usize,
    /// Calibrated per-op worker busy-work, modelling parse/socket cost.
    overhead_ns: u64,
}

fn run_delegate(
    threads: usize,
    write_pct: u32,
    dist: &str,
    warmup: Duration,
    measure: Duration,
    opts: DelegateOpts,
) {
    let DelegateOpts {
        batch,
        owners: owners_arg,
        overhead_ns,
    } = opts;
    use crossbeam_channel::bounded;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    let s_owners = if owners_arg > 0 {
        owners_arg
    } else {
        (threads / 2).max(1)
    };
    let w_workers = (threads - s_owners).max(1);
    let stop = Arc::new(AtomicBool::new(false));
    let measuring = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(w_workers + 1));
    let total_ops = Arc::new(AtomicU64::new(0));
    let value = vec![0xABu8; VLEN];

    // req: (worker_id, key_idx, is_write); resp: ()
    let mut req_tx = Vec::new();
    let mut req_rx = Vec::new();
    for _ in 0..s_owners {
        let (tx, rx) = bounded::<(u32, Vec<(u32, bool)>)>(256);
        req_tx.push(tx);
        req_rx.push(rx);
    }
    let mut resp_tx = Vec::new();
    let mut resp_rx = Vec::new();
    for _ in 0..w_workers {
        let (tx, rx) = bounded::<u32>(256);
        resp_tx.push(tx);
        resp_rx.push(rx);
    }

    let mut owners = Vec::new();
    for o in 0..s_owners {
        let rx = req_rx.remove(0);
        let resp_tx = resp_tx.clone();
        let stop = stop.clone();
        let value = value.clone();
        owners.push(std::thread::spawn(move || {
            let cache = shard_cache(s_owners);
            let mut i = o;
            while i < NKEYS {
                cache
                    .insert(&key_bytes(i), &value[..], None, Duration::ZERO)
                    .expect("prefill");
                i += s_owners;
            }
            loop {
                match rx.recv_timeout(Duration::from_millis(20)) {
                    Ok((w, reqs)) => {
                        let n = reqs.len() as u32;
                        for (idx, is_write) in reqs {
                            let key = key_bytes(idx as usize);
                            if is_write {
                                let _ = cache.insert(&key, &value[..], None, Duration::ZERO);
                            } else {
                                let item = cache.get(&key);
                                std::hint::black_box(&item);
                            }
                        }
                        let _ = resp_tx[w as usize].send(n);
                    }
                    Err(_) => {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                }
            }
        }));
    }

    let mut workers = Vec::new();
    for w in 0..w_workers {
        let req_tx = req_tx.clone();
        let resp = resp_rx.remove(0);
        let (stop, measuring, barrier, total_ops) = (
            stop.clone(),
            measuring.clone(),
            barrier.clone(),
            total_ops.clone(),
        );
        let dist = dist.to_string();
        workers.push(std::thread::spawn(move || {
            let mut rng = SmallRng::seed_from_u64(0xDE1E ^ ((w as u64) << 32) ^ w as u64);
            let zipf = Zipf::new(NKEYS as f64, 0.99).unwrap();
            let mut ops: u64 = 0;
            let mut inflight: u64 = 0;
            let mut start_snap: Option<u64> = None;
            barrier.wait();
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                while inflight < 256 {
                    // accumulate one batch per destination owner, then send
                    let mut per_owner: Vec<Vec<(u32, bool)>> = vec![Vec::new(); s_owners];
                    for _ in 0..batch * s_owners {
                        let idx = if dist == "zipf" {
                            (zipf.sample(&mut rng) as usize - 1) % NKEYS
                        } else {
                            rng.random_range(0..NKEYS)
                        };
                        let is_write = rng.random_range(0..100) < 50;
                        spin_ns(overhead_ns);
                        per_owner[idx % s_owners].push((idx as u32, is_write));
                    }
                    for (o, reqs) in per_owner.into_iter().enumerate() {
                        if reqs.is_empty() {
                            continue;
                        }
                        inflight += reqs.len() as u64;
                        if req_tx[o].send((w as u32, reqs)).is_err() {
                            break;
                        }
                    }
                }
                while let Ok(n) = resp.try_recv() {
                    inflight -= n as u64;
                    ops += n as u64;
                    {
                        match (measuring.load(Ordering::Relaxed), start_snap) {
                            (true, None) => start_snap = Some(ops),
                            (false, Some(s)) => {
                                total_ops.fetch_add(ops - s, Ordering::Relaxed);
                                start_snap = None;
                            }
                            _ => {}
                        }
                    }
                }
            }
            if let Some(s) = start_snap {
                total_ops.fetch_add(ops - s, Ordering::Relaxed);
            }
        }));
    }

    barrier.wait();
    std::thread::sleep(warmup);
    measuring.store(true, Ordering::Relaxed);
    let t0 = Instant::now();
    std::thread::sleep(measure);
    let elapsed = t0.elapsed();
    measuring.store(false, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(50));
    stop.store(true, Ordering::Relaxed);
    for h in workers {
        h.join().unwrap();
    }
    for h in owners {
        h.join().unwrap();
    }
    let mops = total_ops.load(Ordering::Relaxed) as f64 / elapsed.as_secs_f64() / 1e6;
    println!(
        "{},{},{},delegate{}-w{}o{}-x{},{:.3}",
        threads,
        write_pct,
        dist,
        if batch > 1 {
            format!("-b{}", batch)
        } else {
            String::new()
        },
        w_workers,
        s_owners,
        overhead_ns,
        mops
    );
}

/// mode=hybrid: ONE shared engine. Workers execute reads directly against it
/// and batch-route writes by key hash to S owner threads, which apply them
/// with per-owner TTL stripes (per-owner tails on the shared engine).
/// Reads never queue; only the write half pays the handoff.
fn run_hybrid(
    threads: usize,
    dist: &str,
    warmup: Duration,
    measure: Duration,
    batch: usize,
    owners_arg: usize,
    overhead_ns: u64,
) {
    use crossbeam_channel::bounded;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    let s_owners = if owners_arg > 0 {
        owners_arg
    } else {
        (threads / 4).max(1)
    };
    let w_workers = (threads - s_owners).max(1);
    let stop = Arc::new(AtomicBool::new(false));
    let measuring = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(w_workers + 1));
    let total_ops = Arc::new(AtomicU64::new(0));
    let value = vec![0xABu8; VLEN];

    let cache = Arc::new(
        Segcache::builder()
            .heap_size(1024 * MB)
            .segment_size(MB as i32)
            .hash_power(22)
            .overflow_factor(1.0)
            .eviction(Policy::Merge {
                max: 8,
                merge: 4,
                compact: 2,
            })
            .build()
            .expect("build"),
    );
    std::thread::scope(|sc| {
        for t in 0..8 {
            let cache = &cache;
            let value = &value;
            sc.spawn(move || {
                for i in (t..NKEYS).step_by(8) {
                    cache
                        .insert(&key_bytes(i), &value[..], None, Duration::ZERO)
                        .expect("prefill");
                }
            });
        }
    });

    let mut req_tx = Vec::new();
    let mut req_rx = Vec::new();
    for _ in 0..s_owners {
        let (tx, rx) = bounded::<(u32, Vec<u32>)>(256);
        req_tx.push(tx);
        req_rx.push(rx);
    }
    let mut resp_tx = Vec::new();
    let mut resp_rx = Vec::new();
    for _ in 0..w_workers {
        let (tx, rx) = bounded::<u32>(256);
        resp_tx.push(tx);
        resp_rx.push(rx);
    }

    let mut owners = Vec::new();
    for o in 0..s_owners {
        let rx = req_rx.remove(0);
        let resp_tx = resp_tx.clone();
        let stop = stop.clone();
        let value = value.clone();
        let cache = cache.clone();
        owners.push(std::thread::spawn(move || {
            let ttl = Duration::from_secs(1000 + 8 * o as u64);
            loop {
                match rx.recv_timeout(Duration::from_millis(20)) {
                    Ok((w, idxs)) => {
                        let n = idxs.len() as u32;
                        for idx in idxs {
                            let _ = cache.insert(&key_bytes(idx as usize), &value[..], None, ttl);
                        }
                        let _ = resp_tx[w as usize].send(n);
                    }
                    Err(_) => {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                }
            }
        }));
    }

    let mut workers = Vec::new();
    for w in 0..w_workers {
        let req_tx = req_tx.clone();
        let resp = resp_rx.remove(0);
        let (stop, measuring, barrier, total_ops) = (
            stop.clone(),
            measuring.clone(),
            barrier.clone(),
            total_ops.clone(),
        );
        let dist = dist.to_string();
        let cache = cache.clone();
        workers.push(std::thread::spawn(move || {
            let mut rng = SmallRng::seed_from_u64(0x4B1D ^ ((w as u64) << 32) ^ w as u64);
            let zipf = Zipf::new(NKEYS as f64, 0.99).unwrap();
            let mut ops: u64 = 0;
            let mut inflight: u64 = 0;
            let mut pending: Vec<Vec<u32>> = vec![Vec::new(); s_owners];
            let mut start_snap: Option<u64> = None;
            barrier.wait();
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                // closed loop: generate only while write backlog is bounded, so
                // reads cannot run ahead of unapplied writes
                let backlog: u64 = inflight + pending.iter().map(|v| v.len() as u64).sum::<u64>();
                if backlog < 1024 {
                    for _ in 0..(2 * batch) {
                        let idx = if dist == "zipf" {
                            (zipf.sample(&mut rng) as usize - 1) % NKEYS
                        } else {
                            rng.random_range(0..NKEYS)
                        };
                        spin_ns(overhead_ns);
                        if rng.random_range(0..100) < 50 {
                            pending[idx % s_owners].push(idx as u32);
                        } else {
                            let item = cache.get(&key_bytes(idx));
                            std::hint::black_box(&item);
                            ops += 1;
                        }
                    }
                    for o in 0..s_owners {
                        if pending[o].is_empty() {
                            continue;
                        }
                        let reqs = std::mem::take(&mut pending[o]);
                        inflight += reqs.len() as u64;
                        let _ = req_tx[o].send((w as u32, reqs));
                    }
                } else {
                    // saturated: yield the core to owners instead of hot-spinning
                    if let Ok(n) = resp.recv_timeout(Duration::from_micros(100)) {
                        inflight -= n as u64;
                        ops += n as u64;
                    }
                }
                while let Ok(n) = resp.try_recv() {
                    inflight -= n as u64;
                    ops += n as u64;
                }
                {
                    match (measuring.load(Ordering::Relaxed), start_snap) {
                        (true, None) => start_snap = Some(ops),
                        (false, Some(s)) => {
                            total_ops.fetch_add(ops - s, Ordering::Relaxed);
                            start_snap = None;
                        }
                        _ => {}
                    }
                }
            }
            if let Some(s) = start_snap {
                total_ops.fetch_add(ops - s, Ordering::Relaxed);
            }
        }));
    }

    barrier.wait();
    std::thread::sleep(warmup);
    measuring.store(true, Ordering::Relaxed);
    let t0 = Instant::now();
    std::thread::sleep(measure);
    let elapsed = t0.elapsed();
    measuring.store(false, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(50));
    stop.store(true, Ordering::Relaxed);
    for h in workers {
        h.join().unwrap();
    }
    for h in owners {
        h.join().unwrap();
    }
    let mops = total_ops.load(Ordering::Relaxed) as f64 / elapsed.as_secs_f64() / 1e6;
    println!(
        "{},50,{},hybrid-b{}-w{}o{}-x{},{:.3}",
        threads, dist, batch, w_workers, s_owners, overhead_ns, mops
    );
}

/// mode=part: partitioned write-side as a composite — P engine instances
/// behind one routing function (partition = idx % P). Every thread reads AND
/// writes every partition directly (multi-writer per partition, no ownership,
/// no queues). Measures the cost/benefit of the partitioned data layout
/// itself: per-partition tails, free pools, eviction, 1/P-sized heaps.
fn run_part(
    threads: usize,
    write_pct: u32,
    dist: &str,
    warmup: Duration,
    measure: Duration,
    nparts: usize,
) {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    let parts: Arc<Vec<Segcache>> = Arc::new((0..nparts).map(|_| shard_cache(nparts)).collect());
    let value = vec![0xABu8; VLEN];
    std::thread::scope(|sc| {
        for t in 0..8 {
            let parts = &parts;
            let value = &value;
            sc.spawn(move || {
                for i in (t..NKEYS).step_by(8) {
                    parts[i % nparts]
                        .insert(&key_bytes(i), &value[..], None, Duration::ZERO)
                        .expect("prefill");
                }
            });
        }
    });

    let stop = Arc::new(AtomicBool::new(false));
    let measuring = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(threads + 1));
    let total_ops = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for t in 0..threads {
        let (parts, stop, measuring, barrier, total_ops, value) = (
            parts.clone(),
            stop.clone(),
            measuring.clone(),
            barrier.clone(),
            total_ops.clone(),
            value.clone(),
        );
        let dist = dist.to_string();
        handles.push(std::thread::spawn(move || {
            let mut rng = SmallRng::seed_from_u64(0x9A27 ^ ((t as u64) << 32) ^ t as u64);
            let zipf = Zipf::new(NKEYS as f64, 0.99).unwrap();
            let mut ops: u64 = 0;
            let mut start_snap: Option<u64> = None;
            barrier.wait();
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let idx = if dist == "zipf" {
                    (zipf.sample(&mut rng) as usize - 1) % NKEYS
                } else {
                    rng.random_range(0..NKEYS)
                };
                let key = key_bytes(idx);
                let cache = &parts[idx % nparts];
                if rng.random_range(0..100) < write_pct {
                    let _ = cache.insert(&key, &value[..], None, Duration::ZERO);
                } else {
                    let item = cache.get(&key);
                    std::hint::black_box(&item);
                }
                ops += 1;
                if ops.is_multiple_of(64) {
                    match (measuring.load(Ordering::Relaxed), start_snap) {
                        (true, None) => start_snap = Some(ops),
                        (false, Some(s)) => {
                            total_ops.fetch_add(ops - s, Ordering::Relaxed);
                            start_snap = None;
                        }
                        _ => {}
                    }
                }
            }
            if let Some(s) = start_snap {
                total_ops.fetch_add(ops - s, Ordering::Relaxed);
            }
        }));
    }
    barrier.wait();
    std::thread::sleep(warmup);
    measuring.store(true, Ordering::Relaxed);
    let t0 = Instant::now();
    std::thread::sleep(measure);
    let elapsed = t0.elapsed();
    measuring.store(false, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(50));
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        h.join().unwrap();
    }
    let mops = total_ops.load(Ordering::Relaxed) as f64 / elapsed.as_secs_f64() / 1e6;
    println!(
        "{},{},{},part-p{},{:.3}",
        threads, write_pct, dist, nparts, mops
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let threads: usize = args[1].parse().unwrap();
    let write_pct: u32 = args[2].parse().unwrap();
    let dist = args[3].clone();
    let warmup = Duration::from_secs(args[4].parse().unwrap());
    let measure = Duration::from_secs(args[5].parse().unwrap());
    let mode = args.get(6).cloned().unwrap_or_else(|| "base".into());
    match mode.as_str() {
        "shard" => return run_shard(threads, write_pct, &dist, warmup, measure),
        "part" => {
            let nparts: usize = args.get(7).map(|p| p.parse().unwrap()).unwrap_or(16);
            return run_part(threads, write_pct, &dist, warmup, measure, nparts);
        }
        "delegate" => {
            let batch: usize = args.get(7).map(|b| b.parse().unwrap()).unwrap_or(1);
            let owners: usize = args.get(8).map(|o| o.parse().unwrap()).unwrap_or(0);
            let overhead: u64 = args.get(9).map(|o| o.parse().unwrap()).unwrap_or(0);
            return run_delegate(
                threads,
                write_pct,
                &dist,
                warmup,
                measure,
                DelegateOpts {
                    batch,
                    owners,
                    overhead_ns: overhead,
                },
            );
        }
        "hybrid" => {
            let batch: usize = args.get(7).map(|b| b.parse().unwrap()).unwrap_or(16);
            let owners: usize = args.get(8).map(|o| o.parse().unwrap()).unwrap_or(0);
            let overhead: u64 = args.get(9).map(|o| o.parse().unwrap()).unwrap_or(0);
            return run_hybrid(threads, &dist, warmup, measure, batch, owners, overhead);
        }
        _ => {}
    }
    let stripe = mode == "stripe";
    // mode=shuf: identical to base, but prefill writes keys in a shuffled
    // order so a zipf head is spread over many segments instead of landing
    // in the one segment that index-ordered prefill packs keys 0..7280 into.
    // Isolates segment locality (per-segment reader pin counters) from the
    // key distribution itself.
    let shuf = mode == "shuf";

    let cache = Arc::new(
        Segcache::builder()
            .heap_size(1024 * MB)
            .segment_size(MB as i32)
            .hash_power(22)
            .overflow_factor(1.0)
            .eviction(Policy::Merge {
                max: 8,
                merge: 4,
                compact: 2,
            })
            .build()
            .expect("build"),
    );

    let value = vec![0xABu8; VLEN];

    let order: Vec<usize> = if shuf {
        use rand::seq::SliceRandom;
        let mut v: Vec<usize> = (0..NKEYS).collect();
        v.shuffle(&mut SmallRng::seed_from_u64(0xC0FFEE));
        v
    } else {
        Vec::new()
    };

    // prefill from multiple threads (bounded by insert speed, not part of measurement)
    std::thread::scope(|s| {
        for t in 0..8 {
            let cache = &cache;
            let value = &value;
            let order = &order;
            s.spawn(move || {
                let ttl = if stripe {
                    Duration::from_secs(1000 + 8 * t as u64)
                } else {
                    Duration::ZERO
                };
                for i in (t..NKEYS).step_by(8) {
                    let k = if shuf { order[i] } else { i };
                    cache
                        .insert(&key_bytes(k), &value[..], None, ttl)
                        .expect("prefill insert");
                }
            });
        }
    });

    let stop = Arc::new(AtomicBool::new(false));
    let measuring = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(threads + 1));
    let total_ops = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for t in 0..threads {
        let cache = cache.clone();
        let stop = stop.clone();
        let measuring = measuring.clone();
        let barrier = barrier.clone();
        let total_ops = total_ops.clone();
        let value = value.clone();
        let dist = dist.clone();
        handles.push(std::thread::spawn(move || {
            let ttl = if stripe {
                Duration::from_secs(1000 + 8 * t as u64)
            } else {
                Duration::ZERO
            };
            let mut rng = SmallRng::seed_from_u64(0x5E6CAFE ^ ((t as u64) << 32) ^ t as u64);
            let zipf = Zipf::new(NKEYS as f64, 0.99).unwrap();
            let mut ops: u64 = 0;
            let mut start_snap: Option<u64> = None;
            barrier.wait();
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let idx = if dist == "zipf" {
                    (zipf.sample(&mut rng) as usize - 1) % NKEYS
                } else {
                    rng.random_range(0..NKEYS)
                };
                let key = key_bytes(idx);
                if rng.random_range(0..100) < write_pct {
                    let _ = cache.insert(&key, &value[..], None, ttl);
                } else {
                    let item = cache.get(&key);
                    std::hint::black_box(&item);
                }
                ops += 1;
                // sample the measuring flag every 64 ops to keep overhead low
                if ops.is_multiple_of(64) {
                    match (measuring.load(Ordering::Relaxed), start_snap) {
                        (true, None) => start_snap = Some(ops),
                        (false, Some(s)) => {
                            total_ops.fetch_add(ops - s, Ordering::Relaxed);
                            start_snap = None;
                        }
                        _ => {}
                    }
                }
            }
            if let Some(snap) = start_snap {
                total_ops.fetch_add(ops - snap, Ordering::Relaxed);
            }
        }));
    }

    barrier.wait();
    std::thread::sleep(warmup);
    measuring.store(true, Ordering::Relaxed);
    let t0 = Instant::now();
    std::thread::sleep(measure);
    let elapsed = t0.elapsed();
    measuring.store(false, Ordering::Relaxed);
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        h.join().unwrap();
    }

    let mops = total_ops.load(Ordering::Relaxed) as f64 / elapsed.as_secs_f64() / 1e6;
    println!(
        "{},{},{},{},{:.3}",
        threads,
        write_pct,
        dist,
        if stripe {
            "stripe"
        } else if shuf {
            "shuf"
        } else {
            "base"
        },
        mops
    );
}
