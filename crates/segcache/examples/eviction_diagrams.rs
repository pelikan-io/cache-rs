//! Segcache eviction-strategy diagrams, derived from this repo's source.
//!
//! Regenerate: `cargo run -p segcache --example eviction_diagrams`
//!
//! Every drawn claim is asserted against the source files below and the run
//! aborts on drift; all geometry is bounds-checked into each figure's
//! viewBox. Output: `docs/diagrams/eviction-{policies,merge,s3fifo}.svg`,
//! stamped with the last commit that touched `crates/segcache/src`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

const KEEP: &str = "#2a78d6"; // selected / survivor / promoted
const DROP: &str = "#eb6834"; // dropped / pruned
const INK: &str = "#1a2433"; // committed-render ink (self-contained SVGs)

/// (path, needle, meaning) — exact substrings the diagrams depend on.
const CLAIMS: &[(&str, &str, &str)] = &[
    (
        "crates/segcache/src/eviction/policy.rs",
        "pub enum Policy",
        "the eight policies live on one enum",
    ),
    (
        "crates/segcache/src/eviction/policy.rs",
        "S3Fifo {",
        "S3-FIFO is a first-class policy",
    ),
    (
        "crates/segcache/src/eviction/mod.rs",
        "max(lhs.create_at(), lhs.merge_at())",
        "Fifo age = later of create and last merge",
    ),
    (
        "crates/segcache/src/eviction/mod.rs",
        "lhs.create_at() + lhs.ttl()",
        "Cte ranks by absolute expiry time",
    ),
    (
        "crates/segcache/src/eviction/mod.rs",
        "lhs.live_bytes().cmp(&rhs.live_bytes())",
        "Util ranks by live bytes",
    ),
    (
        "crates/segcache/src/eviction/mod.rs",
        "Policy::Fifo | Policy::Cte | Policy::Util",
        "only Fifo/Cte/Util rerank",
    ),
    (
        "crates/segcache/src/segments/segments.rs",
        "if ttl_buckets.expire(hashtable, self) > 0",
        "expired segments are freed before any policy runs",
    ),
    (
        "crates/segcache/src/segments/segments.rs",
        "return ttl_bucket.head();",
        "RandomFifo evicts the sampled bucket's head",
    ),
    (
        "crates/segcache/src/segments/segments.rs",
        "if chain_len < 3",
        "merge needs a chain of at least 3 evictable segments",
    ),
    (
        "crates/segcache/src/segments/segments.rs",
        "merge_evict_fallback_drop(start, ttl_bucket, hashtable)",
        "no spare -> drop the chain head whole",
    ),
    (
        "crates/segcache/src/segments/segments.rs",
        "cutoff = cand.prune(hashtable, cutoff, target_ratio);",
        "merge prunes items below a frequency cutoff",
    ),
    (
        "crates/segcache/src/segments/segments.rs",
        "self.claim_for_drain(cand_id)",
        "candidates are claimed Sealed->Draining before mutation",
    ),
    (
        "crates/segcache/src/segments/segments.rs",
        "self.publish_dest_sealed(spare_id);",
        "the filled spare is published Relinking->Sealed",
    ),
    (
        "crates/segcache/src/segments/segments.rs",
        "if ratio > target_ratio {",
        "compaction fires below the 1/compact occupancy watermark",
    ),
    (
        "crates/segcache/src/segments/segments.rs",
        "find_oldest_seg_in_pool(ttl_buckets, SegmentPool::Admission)",
        "S3-FIFO evicts from the admission pool",
    ),
    (
        "crates/segcache/src/segments/segments.rs",
        "find_oldest_seg_in_pool(ttl_buckets, SegmentPool::Main)",
        "S3-FIFO falls back to the main pool",
    ),
    (
        "crates/segcache/src/segments/segments.rs",
        "self.s3fifo_ghost_remaining(seg_id, hashtable);",
        "freq==0 admission items are recorded in the ghost queue",
    ),
    (
        "crates/segcache/src/segments/segments.rs",
        "fn s3fifo_promote_from",
        "freq>0 items are promoted by copy",
    ),
    (
        "crates/segcache/src/segments/segments.rs",
        "pub(crate) fn ghost_contains",
        "inserts consult the ghost queue",
    ),
    (
        "crates/segcache/src/segments/segments.rs",
        "(segments as f64 * admission_ratio).round() as u32",
        "admission pool sized by admission_ratio",
    ),
    (
        "crates/segcache/src/segments/header.rs",
        "self.state().is_evictable() && self.ref_count() == 0",
        "can_evict = evictable state and no reader pins",
    ),
    (
        "crates/segcache/src/eviction/ghost.rs",
        "queue.pop_front()",
        "ghost queue is a bounded FIFO",
    ),
];

/// Format a coordinate the way the SVGs are committed: integers bare,
/// fractional values as-is.
fn num(v: f64) -> String {
    if v == v.trunc() {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[derive(Clone, Copy)]
struct RectOpts {
    stroke: &'static str,
    dash: Option<&'static str>,
    rx: f64,
    fill: &'static str,
    sw: f64,
    opacity: Option<f64>,
}

impl Default for RectOpts {
    fn default() -> Self {
        Self {
            stroke: "currentColor",
            dash: None,
            rx: 6.0,
            fill: "none",
            sw: 1.3,
            opacity: None,
        }
    }
}

#[derive(Clone, Copy)]
struct TextOpts {
    size: f64,
    anchor: &'static str,
    color: &'static str,
    weight: Option<u32>,
    mono: bool,
    opacity: Option<f64>,
}

impl Default for TextOpts {
    fn default() -> Self {
        Self {
            size: 12.0,
            anchor: "middle",
            color: "currentColor",
            weight: None,
            mono: false,
            opacity: None,
        }
    }
}

#[derive(Clone, Copy)]
struct LineOpts {
    color: &'static str,
    arrow: bool,
    dash: Option<&'static str>,
    sw: f64,
}

impl Default for LineOpts {
    fn default() -> Self {
        Self {
            color: "currentColor",
            arrow: true,
            dash: None,
            sw: 1.3,
        }
    }
}

struct Svg {
    w: f64,
    h: f64,
    body: String,
    elems: usize,
}

impl Svg {
    fn new(w: f64, h: f64) -> Self {
        let mut body = String::new();
        let _ = writeln!(
            body,
            "<svg viewBox=\"0 0 {} {}\" role=\"img\" xmlns=\"http://www.w3.org/2000/svg\" \
             fill=\"none\" stroke-linejoin=\"round\">",
            num(w),
            num(h)
        );
        for (id, fill) in [
            ("MID", "currentColor"),
            ("MID-keep", KEEP),
            ("MID-drop", DROP),
        ] {
            let open = if id == "MID" { "<defs>" } else { "" };
            let close = if id == "MID-drop" { "</defs>\n" } else { "" };
            let _ = write!(
                body,
                "{open}<marker id=\"{id}\" viewBox=\"0 0 10 10\" refX=\"9\" refY=\"5\" \
                 markerWidth=\"6.5\" markerHeight=\"6.5\" orient=\"auto-start-reverse\">\
                 <path d=\"M0 0 L10 5 L0 10 z\" fill=\"{fill}\" stroke=\"none\"/></marker>{close}",
            );
        }
        Self {
            w,
            h,
            body,
            elems: 0,
        }
    }

    fn bound(&mut self, left: f64, top: f64, right: f64, bottom: f64) {
        assert!(
            0.0 <= left
                && left <= right
                && right <= self.w
                && 0.0 <= top
                && top <= bottom
                && bottom <= self.h,
            "element out of bounds: ({left},{top})..({right},{bottom}) in {}x{}",
            self.w,
            self.h
        );
        self.elems += 1;
    }

    fn rect(&mut self, x: f64, y: f64, w: f64, h: f64, o: RectOpts) {
        self.bound(x, y, x + w, y + h);
        let dash = o
            .dash
            .map(|d| format!(" stroke-dasharray=\"{d}\""))
            .unwrap_or_default();
        let opacity = o
            .opacity
            .map(|v| format!(" fill-opacity=\"{v}\""))
            .unwrap_or_default();
        let _ = writeln!(
            self.body,
            "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"{}\" fill=\"{}\"{opacity} \
             stroke=\"{}\" stroke-width=\"{}\"{dash}/>",
            num(x),
            num(y),
            num(w),
            num(h),
            num(o.rx),
            o.fill,
            o.stroke,
            num(o.sw)
        );
    }

    fn text(&mut self, x: f64, y: f64, s: &str, o: TextOpts) {
        let est = s.chars().count() as f64 * o.size * 0.6;
        let left = match o.anchor {
            "middle" => x - est / 2.0,
            "end" => x - est,
            _ => x,
        };
        self.bound(left.max(0.0), y - o.size, (left + est).min(self.w), y + 3.0);
        let family = if o.mono {
            "ui-monospace,monospace"
        } else {
            "inherit"
        };
        let weight = o
            .weight
            .map(|v| format!(" font-weight=\"{v}\""))
            .unwrap_or_default();
        let opacity = o
            .opacity
            .map(|v| format!(" opacity=\"{v}\""))
            .unwrap_or_default();
        let _ = writeln!(
            self.body,
            "<text x=\"{}\" y=\"{}\" text-anchor=\"{}\" font-size=\"{}\" font-family=\"{family}\" \
             fill=\"{}\" stroke=\"none\"{weight}{opacity}>{}</text>",
            num(x),
            num(y),
            o.anchor,
            num(o.size),
            o.color,
            esc(s)
        );
    }

    fn line(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, o: LineOpts) {
        self.bound(x1.min(x2), y1.min(y2), x1.max(x2), y1.max(y2));
        let marker_id = match o.color {
            "currentColor" => "MID",
            KEEP => "MID-keep",
            DROP => "MID-drop",
            other => panic!("no arrow marker for color {other}"),
        };
        let marker = if o.arrow {
            format!(" marker-end=\"url(#{marker_id})\"")
        } else {
            String::new()
        };
        let dash = o
            .dash
            .map(|d| format!(" stroke-dasharray=\"{d}\""))
            .unwrap_or_default();
        let _ = writeln!(
            self.body,
            "<line x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"{}\" stroke-width=\"{}\"{marker}{dash}/>",
            num(x1),
            num(y1),
            num(x2),
            num(y2),
            o.color,
            num(o.sw)
        );
    }

    fn done(mut self) -> (String, usize) {
        self.body.push_str("</svg>\n");
        (self.body, self.elems)
    }
}

fn segment(s: &mut Svg, x: f64, y: f64, label: &str, sub: &str, tail: bool, pick: bool) {
    let (w, h) = (112.0, 58.0);
    s.rect(
        x,
        y,
        w,
        h,
        RectOpts {
            dash: if tail { Some("5 4") } else { None },
            ..Default::default()
        },
    );
    s.text(
        x + w / 2.0,
        y + 22.0,
        label,
        TextOpts {
            size: 12.5,
            weight: Some(600),
            ..Default::default()
        },
    );
    s.text(
        x + w / 2.0,
        y + 40.0,
        sub,
        TextOpts {
            size: 10.5,
            opacity: Some(0.75),
            ..Default::default()
        },
    );
    if tail {
        s.text(
            x + w / 2.0,
            y + h - 6.0,
            "write tail · Live",
            TextOpts {
                size: 9.5,
                opacity: Some(0.65),
                ..Default::default()
            },
        );
    }
    if pick {
        s.rect(
            x,
            y,
            w,
            h,
            RectOpts {
                stroke: KEEP,
                sw: 2.2,
                ..Default::default()
            },
        );
    }
}

fn callout(s: &mut Svg, x: f64, y: f64, tx: f64, ty: f64, name: &str, why: &str) {
    s.text(
        x,
        y,
        name,
        TextOpts {
            size: 11.5,
            color: KEEP,
            weight: Some(700),
            ..Default::default()
        },
    );
    s.text(
        x,
        y + 13.0,
        why,
        TextOpts {
            size: 10.0,
            opacity: Some(0.75),
            ..Default::default()
        },
    );
    s.line(
        x,
        y + 18.0,
        tx,
        ty,
        LineOpts {
            color: KEEP,
            sw: 1.6,
            ..Default::default()
        },
    );
}

fn note(s: &mut Svg, x: f64, y: f64, msg: &str, size: f64) {
    s.text(
        x,
        y,
        msg,
        TextOpts {
            size,
            anchor: "start",
            ..Default::default()
        },
    );
}

fn stamp(s: &mut Svg, y: f64, commit: &str) {
    s.text(
        960.0,
        y,
        &format!("derived from cache-rs @ {commit}"),
        TextOpts {
            size: 9.5,
            anchor: "end",
            opacity: Some(0.55),
            mono: true,
            ..Default::default()
        },
    );
}

fn divider(s: &mut Svg, y: f64) {
    s.line(
        20.0,
        y,
        960.0,
        y,
        LineOpts {
            arrow: false,
            dash: Some("2 4"),
            sw: 0.8,
            ..Default::default()
        },
    );
}

// ── Figure 1: shared substrate + the five whole-segment pickers ────
fn fig_policies(commit: &str) -> (String, usize) {
    let mut f = Svg::new(980.0, 545.0);
    const GX: [f64; 5] = [150.0, 276.0, 402.0, 528.0, 654.0];
    let rows: [(f64, &str, usize); 3] = [
        (72.0, "TTL ~5 m", 5),
        (208.0, "TTL ~1 h", 4),
        (344.0, "TTL ~6 h", 3),
    ];
    for (y, label, n) in rows {
        f.text(
            20.0,
            y + 33.0,
            label,
            TextOpts {
                anchor: "start",
                weight: Some(600),
                ..Default::default()
            },
        );
        f.text(
            20.0,
            y + 49.0,
            "bucket",
            TextOpts {
                size: 10.0,
                anchor: "start",
                opacity: Some(0.6),
                ..Default::default()
            },
        );
        for i in 0..n - 1 {
            f.line(
                GX[i] + 112.0,
                y + 29.0,
                GX[i + 1],
                y + 29.0,
                LineOpts::default(),
            );
        }
        f.text(
            GX[0] - 8.0,
            y + 16.0,
            "head",
            TextOpts {
                size: 9.5,
                anchor: "end",
                opacity: Some(0.6),
                ..Default::default()
            },
        );
    }
    segment(
        &mut f,
        GX[0],
        72.0,
        "S1",
        "age 120 s · live 85 %",
        false,
        true,
    );
    segment(
        &mut f,
        GX[1],
        72.0,
        "S2",
        "age 90 s · live 70 %",
        false,
        false,
    );
    segment(
        &mut f,
        GX[2],
        72.0,
        "S3",
        "age 40 s · live 15 %",
        false,
        true,
    );
    segment(
        &mut f,
        GX[3],
        72.0,
        "S4",
        "age 20 s · live 90 %",
        false,
        false,
    );
    segment(&mut f, GX[4], 72.0, "S5", "filling", true, false);
    segment(
        &mut f,
        GX[0],
        208.0,
        "S6",
        "age 300 s · live 95 %",
        false,
        true,
    );
    segment(
        &mut f,
        GX[1],
        208.0,
        "S7",
        "age 150 s · live 60 %",
        false,
        false,
    );
    segment(
        &mut f,
        GX[2],
        208.0,
        "S8",
        "age 60 s · live 80 %",
        false,
        true,
    );
    segment(&mut f, GX[3], 208.0, "S9", "filling", true, false);
    segment(
        &mut f,
        GX[0],
        344.0,
        "S10",
        "age 200 s · live 75 %",
        false,
        true,
    );
    segment(
        &mut f,
        GX[1],
        344.0,
        "S11",
        "age 20 s · live 90 %",
        false,
        false,
    );
    segment(&mut f, GX[2], 344.0, "S12", "filling", true, false);
    let c = GX[0] + 56.0;
    let u = GX[2] + 56.0;
    callout(
        &mut f,
        c,
        28.0,
        c,
        70.0,
        "Cte",
        "min(create_at + ttl): expires in 180 s",
    );
    callout(
        &mut f,
        u,
        28.0,
        u,
        70.0,
        "Util",
        "min(live_bytes): 85 % dead space",
    );
    callout(
        &mut f,
        c,
        164.0,
        c,
        206.0,
        "Fifo",
        "max(create_at, merge_at) oldest overall",
    );
    callout(
        &mut f,
        u,
        164.0,
        u,
        206.0,
        "Random",
        "uniform over evictable segments",
    );
    callout(
        &mut f,
        c,
        300.0,
        c,
        342.0,
        "RandomFifo",
        "random readable seg -> its bucket's head",
    );
    divider(&mut f, 430.0);
    note(&mut f, 20.0, 452.0, "Shared, before any policy runs:  expire() frees whole expired segments first — eviction is the fallback.", 11.5);
    note(&mut f, 20.0, 472.0, "can_evict = evictable state && ref_count == 0 — the Live write tail (dashed) and reader-pinned segments are never selected.", 11.5);
    note(&mut f, 20.0, 492.0, "Fifo / Cte / Util rerank a sorted segment list (at most once per second); Random / RandomFifo sample statelessly.", 11.5);
    note(&mut f, 20.0, 512.0, "All five drop the selected segment whole: every item in it is evicted in one step.  Policy::None skips selection — inserts fail when full.", 11.5);
    stamp(&mut f, 532.0, commit);
    f.done()
}

// ── Figure 2: merge eviction ───────────────────────────────────────
fn fig_merge(commit: &str) -> (String, usize) {
    let mut f = Svg::new(980.0, 500.0);
    f.text(
        192.0,
        44.0,
        "bucket head",
        TextOpts {
            size: 10.5,
            anchor: "end",
            opacity: Some(0.65),
            ..Default::default()
        },
    );
    f.rect(
        150.0,
        88.0,
        140.0,
        74.0,
        RectOpts {
            dash: Some("5 4"),
            rx: 8.0,
            ..Default::default()
        },
    );
    f.text(
        220.0,
        112.0,
        "spare segment",
        TextOpts {
            size: 12.5,
            weight: Some(600),
            ..Default::default()
        },
    );
    for (y, s) in [(130.0, "Relinking:"), (144.0, "readable, not evictable")] {
        f.text(
            220.0,
            y,
            s,
            TextOpts {
                size: 10.5,
                opacity: Some(0.75),
                ..Default::default()
            },
        );
    }
    f.text(
        220.0,
        176.0,
        "1  reserve spare + head-insert",
        TextOpts {
            size: 10.5,
            ..Default::default()
        },
    );
    let cands = [330.0, 505.0, 680.0];
    for (i, &x) in cands.iter().enumerate() {
        f.rect(
            x,
            88.0,
            140.0,
            74.0,
            RectOpts {
                rx: 8.0,
                ..Default::default()
            },
        );
        f.text(
            x + 70.0,
            106.0,
            &format!("candidate {}", ["A", "B", "C"][i]),
            TextOpts {
                weight: Some(600),
                ..Default::default()
            },
        );
        for j in 0..3u8 {
            let bx = x + 16.0 + f64::from(j) * 38.0;
            for (row_y, color) in [(116.0, KEEP), (138.0, DROP)] {
                f.rect(
                    bx,
                    row_y,
                    26.0,
                    16.0,
                    RectOpts {
                        stroke: color,
                        fill: color,
                        opacity: Some(0.28),
                        rx: 2.0,
                        sw: 1.1,
                        ..Default::default()
                    },
                );
            }
        }
        f.text(
            x + 70.0,
            176.0,
            "2  claim Sealed -> Draining",
            TextOpts {
                size: 10.5,
                ..Default::default()
            },
        );
    }
    for (x1, x2) in [
        (290.0, 330.0),
        (470.0, 505.0),
        (645.0, 680.0),
        (820.0, 862.0),
    ] {
        f.line(x1, 125.0, x2, 125.0, LineOpts::default());
    }
    f.text(
        905.0,
        129.0,
        "rest of chain",
        TextOpts {
            size: 10.5,
            opacity: Some(0.65),
            ..Default::default()
        },
    );
    for (i, &x) in cands.iter().enumerate() {
        let (lane_y, drop_x) = (74.0 - i as f64 * 10.0, 248.0 - i as f64 * 24.0);
        let keep = LineOpts {
            color: KEEP,
            arrow: false,
            sw: 1.5,
            ..Default::default()
        };
        f.line(x + 40.0, 88.0, x + 40.0, lane_y, keep);
        f.line(x + 40.0, lane_y, drop_x, lane_y, keep);
        f.line(
            drop_x,
            lane_y,
            drop_x,
            86.0,
            LineOpts {
                arrow: true,
                ..keep
            },
        );
    }
    f.text(
        430.0,
        36.0,
        "4  copy survivors into the spare (append + Release-CAS republish)",
        TextOpts {
            size: 11.0,
            color: KEEP,
            anchor: "start",
            ..Default::default()
        },
    );
    for &x in &cands {
        f.line(
            x + 70.0,
            196.0,
            x + 70.0,
            236.0,
            LineOpts {
                color: DROP,
                sw: 1.5,
                ..Default::default()
            },
        );
    }
    f.text(
        30.0,
        248.0,
        "3  prune: freq < cutoff -> dropped",
        TextOpts {
            size: 11.0,
            color: DROP,
            anchor: "start",
            ..Default::default()
        },
    );
    f.text(
        30.0,
        264.0,
        "(marked deleted - no bytes move)",
        TextOpts {
            size: 10.0,
            color: DROP,
            anchor: "start",
            opacity: Some(0.85),
            ..Default::default()
        },
    );
    for &x in &cands {
        f.line(
            x + 108.0,
            196.0,
            x + 108.0,
            292.0,
            LineOpts {
                sw: 1.1,
                ..Default::default()
            },
        );
    }
    f.rect(
        300.0,
        300.0,
        560.0,
        44.0,
        RectOpts {
            rx: 8.0,
            ..Default::default()
        },
    );
    f.text(
        580.0,
        320.0,
        "5  finalize drained candidate -> unlink from chain -> recycle to free pool",
        TextOpts {
            size: 11.5,
            ..Default::default()
        },
    );
    f.text(
        580.0,
        336.0,
        "(a reader-pinned candidate is condemned to its last reader instead)",
        TextOpts {
            size: 10.0,
            opacity: Some(0.7),
            ..Default::default()
        },
    );
    f.text(
        580.0,
        370.0,
        "6  publish spare: Relinking -> Sealed - it remains the bucket head",
        TextOpts {
            size: 11.0,
            ..Default::default()
        },
    );
    divider(&mut f, 404.0);
    note(&mut f, 20.0, 426.0, "start: random TTL bucket -> its next_to_merge cursor · needs >= 3 evictable chained segments · no spare -> fallback: drop chain head whole", 11.0);
    note(&mut f, 20.0, 446.0, "stops when: max segments merged · spare reaches stop_ratio · candidate unevictable · drain claim lost", 11.0);
    note(&mut f, 20.0, 466.0, "compaction sub-mode (from remove_at, occupancy < 1/compact): same copy machinery, no pruning, skips instead of dropping when no spare", 11.0);
    stamp(&mut f, 488.0, commit);
    f.done()
}

// ── Figure 3: S3-FIFO ──────────────────────────────────────────────
fn fig_s3fifo(commit: &str) -> (String, usize) {
    let mut f = Svg::new(980.0, 470.0);
    let bold = |size| TextOpts {
        size,
        weight: Some(600),
        ..Default::default()
    };
    let faint = |size| TextOpts {
        size,
        opacity: Some(0.7),
        ..Default::default()
    };
    f.rect(
        30.0,
        70.0,
        130.0,
        54.0,
        RectOpts {
            rx: 8.0,
            ..Default::default()
        },
    );
    f.text(95.0, 93.0, "insert", bold(12.5));
    f.text(95.0, 110.0, "new key", faint(10.5));
    f.rect(
        250.0,
        40.0,
        210.0,
        120.0,
        RectOpts {
            rx: 8.0,
            ..Default::default()
        },
    );
    f.text(355.0, 62.0, "admission pool", bold(12.5));
    f.text(355.0, 78.0, "~admission_ratio of segments", faint(10.0));
    for i in 0..3u8 {
        f.rect(
            268.0 + f64::from(i) * 60.0,
            92.0,
            48.0,
            30.0,
            RectOpts {
                rx: 4.0,
                sw: 1.1,
                ..Default::default()
            },
        );
    }
    f.text(355.0, 142.0, "evicted first: oldest segment", faint(10.0));
    f.rect(
        640.0,
        40.0,
        230.0,
        120.0,
        RectOpts {
            rx: 8.0,
            ..Default::default()
        },
    );
    f.text(755.0, 62.0, "main pool", bold(12.5));
    f.text(755.0, 78.0, "remaining segments", faint(10.0));
    for i in 0..3u8 {
        f.rect(
            660.0 + f64::from(i) * 64.0,
            92.0,
            52.0,
            30.0,
            RectOpts {
                rx: 4.0,
                sw: 1.1,
                ..Default::default()
            },
        );
    }
    f.text(755.0, 142.0, "evicted second: CLOCK sweep", faint(10.0));
    f.rect(
        250.0,
        330.0,
        210.0,
        60.0,
        RectOpts {
            dash: Some("5 4"),
            rx: 8.0,
            ..Default::default()
        },
    );
    f.text(355.0, 355.0, "ghost queue", bold(12.5));
    f.text(355.0, 372.0, "bounded FIFO of key hashes", faint(10.0));
    f.line(160.0, 84.0, 250.0, 84.0, LineOpts::default());
    f.text(
        205.0,
        76.0,
        "miss",
        TextOpts {
            size: 10.0,
            ..Default::default()
        },
    );
    let keep = LineOpts {
        color: KEEP,
        sw: 1.6,
        ..Default::default()
    };
    let drop = LineOpts {
        color: DROP,
        sw: 1.6,
        ..Default::default()
    };
    f.line(460.0, 84.0, 640.0, 84.0, keep);
    f.text(
        550.0,
        76.0,
        "freq > 0: promote (copy)",
        TextOpts {
            size: 10.5,
            color: KEEP,
            ..Default::default()
        },
    );
    f.line(355.0, 160.0, 355.0, 330.0, drop);
    let drop_text = |y, s: &str, f: &mut Svg| {
        f.text(
            368.0,
            y,
            s,
            TextOpts {
                size: 10.5,
                color: DROP,
                anchor: "start",
                ..Default::default()
            },
        );
    };
    drop_text(250.0, "freq == 0: drop item,", &mut f);
    drop_text(265.0, "record key hash", &mut f);
    f.line(
        95.0,
        124.0,
        95.0,
        300.0,
        LineOpts {
            arrow: false,
            ..Default::default()
        },
    );
    f.line(
        95.0,
        300.0,
        640.0,
        300.0,
        LineOpts {
            arrow: false,
            ..Default::default()
        },
    );
    f.line(640.0, 300.0, 640.0, 164.0, LineOpts::default());
    f.text(
        560.0,
        292.0,
        "ghost hit: skip admission, insert to main",
        TextOpts {
            size: 10.5,
            ..Default::default()
        },
    );
    f.line(
        250.0,
        344.0,
        108.0,
        310.0,
        LineOpts {
            dash: Some("3 4"),
            sw: 1.1,
            ..Default::default()
        },
    );
    f.text(
        150.0,
        290.0,
        "consulted on insert",
        TextOpts {
            size: 9.5,
            opacity: Some(0.7),
            ..Default::default()
        },
    );
    f.rect(
        640.0,
        330.0,
        230.0,
        60.0,
        RectOpts {
            rx: 8.0,
            ..Default::default()
        },
    );
    f.text(755.0, 355.0, "fresh main segment", bold(12.0));
    f.text(755.0, 372.0, "second chance for freq > 0", faint(10.0));
    f.line(720.0, 160.0, 720.0, 330.0, keep);
    f.text(
        733.0,
        250.0,
        "freq > 0: copy",
        TextOpts {
            size: 10.5,
            color: KEEP,
            anchor: "start",
            ..Default::default()
        },
    );
    f.line(830.0, 160.0, 830.0, 240.0, drop);
    drop_text(205.0, "freq == 0:", &mut f);
    drop_text(220.0, "dropped", &mut f);
    divider(&mut f, 410.0);
    note(&mut f, 20.0, 432.0, "One-hit wonders die in admission; a ghost hit is the proof of a second request, earning direct main placement.", 11.0);
    note(&mut f, 20.0, 452.0, "Promotion and second-chance copies reuse the merge relink machinery: copy bytes, then Release-CAS the hashtable location.", 11.0);
    stamp(&mut f, 464.0, commit);
    f.done()
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("git must be runnable to stamp the diagrams");
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8(out.stdout)
        .expect("git output is utf-8")
        .trim()
        .to_string()
}

fn emit(root: &Path, name: &str, svg: String) {
    // Committed doc render: explicit ink on a white ground so the figure is
    // self-contained wherever the markdown is viewed.
    let svg = svg.replace("currentColor", INK).replacen(
        "</defs>",
        "</defs><rect width=\"100%\" height=\"100%\" fill=\"white\"/>",
        1,
    );
    let path = root.join("docs/diagrams").join(name);
    std::fs::write(&path, svg).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

fn main() {
    let root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("example lives two levels below the workspace root")
        .to_path_buf();

    let dirty = git(&root, &["status", "--short", "crates/segcache/src/"]);
    assert!(
        dirty.is_empty(),
        "crates/segcache is dirty; refusing to stamp:\n{dirty}"
    );
    // Stamp with the last commit that touched the sources the claims are
    // asserted against, so docs-only commits cannot stale the stamp.
    let commit = git(
        &root,
        &["log", "-1", "--format=%h", "--", "crates/segcache/src"],
    );

    for (path, needle, meaning) in CLAIMS {
        let source =
            std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("read {path}: {e}"));
        assert!(
            source.contains(needle),
            "CLAIM DRIFT: {path} no longer contains {needle:?} ({meaning}) — update the \
             claim table and the affected figure in this example"
        );
    }
    // Ordered claim: the admission pool is tried before main.
    let segments = std::fs::read_to_string(root.join("crates/segcache/src/segments/segments.rs"))
        .expect("segments.rs read for ordered claim");
    let admission = segments
        .find("find_oldest_seg_in_pool(ttl_buckets, SegmentPool::Admission)")
        .expect("admission needle verified above");
    let main_pool = segments
        .find("find_oldest_seg_in_pool(ttl_buckets, SegmentPool::Main)")
        .expect("main-pool needle verified above");
    assert!(
        admission < main_pool,
        "CLAIM DRIFT: admission-pool eviction no longer precedes main"
    );

    let mut elems = 0;
    for (name, (svg, count)) in [
        ("eviction-policies.svg", fig_policies(&commit)),
        ("eviction-merge.svg", fig_merge(&commit)),
        ("eviction-s3fifo.svg", fig_s3fifo(&commit)),
    ] {
        elems += count;
        emit(&root, name, svg);
    }
    println!(
        "claims: {} + 1 ordered — all hold at {commit}",
        CLAIMS.len()
    );
    println!("elements bounds-checked: {elems}");
}
