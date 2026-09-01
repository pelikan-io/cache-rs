//! Generate `docs/diagrams/handoff-dataflow.svg`: read/write path divergence in
//! the hybrid handoff architecture (direct reads, delegated writes).
//!
//! The chart draws a REFERENCE thread architecture, not anything this
//! repository implements. cache-rs ships a storage engine and owns no threads:
//! workers, owner threads, batching, and reply release all belong to the server
//! built on top of it. Only the engine band at the bottom of the chart is code
//! that lives here.
//!
//! Single-use chart per the architecture-diagram skill: geometry emitted
//! directly, default visual language, source-asserted against the working model
//! (segbench `mode=hybrid`), stamped with the commit it was derived from.
//!
//! Run: `cargo run --release --bin handoff_diagram`

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---- visual language (skill defaults) ----
const PINK: &str = "#FBB4AE";
const BLUE: &str = "#B3CDE3";
const GREEN: &str = "#CCEBC5";
const GRAY: &str = "#F2F2F2";
const INK: &str = "#222222";
const MUTED: &str = "#555555";
const SANS: &str = "Helvetica, Arial, sans-serif";
const MONO: &str = "DejaVu Sans Mono, Menlo, monospace";
const W: f64 = 1180.0;
const H: f64 = 700.0;

/// The chart draws claims about the harness. If the harness stops embodying
/// one, the chart is stale and this refuses to regenerate it.
const CLAIMS: &[(&str, &str)] = &[
    ("fn run_hybrid", "hybrid mode exists"),
    (
        "cache.get(&key_bytes(idx))",
        "workers execute reads directly on the shared engine",
    ),
    (
        "pending[idx % s_owners].push(idx as u32)",
        "writes buffered per destination owner (store buffer)",
    ),
    (
        "req_tx[o].send((w as u32, reqs))",
        "writes batch-delegated over bounded channels",
    ),
    (
        "cache.insert(&key_bytes(idx as usize), &value[..], None, ttl)",
        "owner applies writes to the shared engine",
    ),
    (
        "Duration::from_secs(1000 + 8 * o as u64)",
        "per-owner TTL stripe = private tail",
    ),
    (
        "resp_tx[w as usize].send(n)",
        "owner acks applied batches back to the worker",
    ),
    (
        "if backlog < 1024",
        "closed loop: reads cannot run ahead of unapplied writes unboundedly",
    ),
];

#[derive(Default)]
struct Canvas {
    elements: Vec<String>,
    /// Every boxed element, checked against the canvas before writing.
    bounds: Vec<(f64, f64, f64, f64)>,
}

/// Optional text styling. Defaults match the Python original's keyword defaults.
struct Text<'a> {
    size: f64,
    anchor: &'a str,
    family: &'a str,
    italic: bool,
    weight: &'a str,
    fill: &'a str,
}

/// Optional rectangle styling; defaults match a plain ink-stroked box.
struct Style<'a> {
    fill: &'a str,
    stroke: &'a str,
    sw: f64,
    dash: &'a str,
    rx: f64,
}

impl Default for Style<'_> {
    fn default() -> Self {
        Self {
            fill: "none",
            stroke: INK,
            sw: 1.2,
            dash: "",
            rx: 0.0,
        }
    }
}

impl Default for Text<'_> {
    fn default() -> Self {
        Self {
            size: 13.0,
            anchor: "start",
            family: SANS,
            italic: false,
            weight: "",
            fill: INK,
        }
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

impl Canvas {
    fn text(&mut self, x: f64, y: f64, s: &str, t: Text) {
        let italic = if t.italic {
            "font-style=\"italic\" "
        } else {
            ""
        };
        let weight = if t.weight.is_empty() {
            String::new()
        } else {
            format!("font-weight=\"{}\" ", t.weight)
        };
        self.elements.push(format!(
            "<text x=\"{}\" y=\"{}\" font-family=\"{}\" font-size=\"{}\" \
             text-anchor=\"{}\" {italic}{weight}fill=\"{}\">{}</text>",
            x,
            y,
            t.family,
            t.size,
            t.anchor,
            t.fill,
            esc(s)
        ));
    }

    fn rect(&mut self, x: f64, y: f64, w: f64, h: f64, s: Style) {
        let d = if s.dash.is_empty() {
            String::new()
        } else {
            format!(" stroke-dasharray=\"{}\"", s.dash)
        };
        self.elements.push(format!(
            "<rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" fill=\"{}\" \
             stroke=\"{}\" stroke-width=\"{}\" rx=\"{}\"{d}/>",
            s.fill, s.stroke, s.sw, s.rx
        ));
    }

    /// A rectangle that also participates in the canvas bounds check.
    fn box_(&mut self, x: f64, y: f64, w: f64, h: f64, s: Style) {
        self.rect(x, y, w, h, s);
        self.bounds.push((x, y, x + w, y + h));
    }

    fn vline_arrow(&mut self, x: f64, y1: f64, y2: f64, sw: f64) {
        let up = y2 < y1;
        let end = y2 + if up { 8.0 } else { -8.0 };
        self.elements.push(format!(
            "<line x1=\"{x}\" y1=\"{y1}\" x2=\"{x}\" y2=\"{end}\" stroke=\"{INK}\" stroke-width=\"{sw}\"/>"
        ));
        let tip = y2;
        let base = y2 + if up { 10.0 } else { -10.0 };
        self.elements.push(format!(
            "<path d=\"M{},{base} L{},{base} L{x},{tip} Z\" fill=\"{INK}\"/>",
            x - 5.0,
            x + 5.0
        ));
    }

    fn badge(&mut self, x: f64, y: f64, label: &str) {
        self.elements.push(format!(
            "<circle cx=\"{x}\" cy=\"{y}\" r=\"12\" fill=\"white\" stroke=\"{INK}\" stroke-width=\"1.3\"/>"
        ));
        self.text(
            x,
            y + 4.0,
            label,
            Text {
                size: 11.0,
                anchor: "middle",
                weight: "bold",
                ..Default::default()
            },
        );
    }

    /// Returns the chip's width so callers can lay chips out in a row.
    fn chip(&mut self, x: f64, y: f64, s: &str, fill: &str) -> f64 {
        let w = 16.0 + s.chars().count() as f64 * 7.6;
        self.rect(
            x,
            y,
            w,
            22.0,
            Style {
                fill,
                sw: 0.8,
                rx: 4.0,
                ..Default::default()
            },
        );
        self.text(
            x + 8.0,
            y + 15.5,
            s,
            Text {
                size: 14.0,
                ..Default::default()
            },
        );
        w
    }

    fn queue_glyph(&mut self, x: f64, ymid: f64) {
        for i in 0..4 {
            self.rect(
                x - 24.0 + i as f64 * 12.0,
                ymid - 6.0,
                11.0,
                12.0,
                Style {
                    fill: "white",
                    sw: 1.0,
                    ..Default::default()
                },
            );
        }
    }
}

fn git_commit(dir: &Path) -> String {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn main() {
    let pkg = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src_path = pkg.join("src/main.rs");
    // repo root: benchmarks/segbench/../../
    let out_dir = pkg.join("../../docs/diagrams");

    // ---- ground truth: assert the harness still embodies the drawn claims ----
    let src = std::fs::read_to_string(&src_path)
        .unwrap_or_else(|e| panic!("{}: {e}", src_path.display()));
    let missing: Vec<_> = CLAIMS.iter().filter(|(p, _)| !src.contains(p)).collect();
    if !missing.is_empty() {
        for (p, why) in missing {
            eprintln!("DRIFT: pattern {p:?} absent — chart claim no longer holds: {why}");
        }
        std::process::exit(1);
    }

    let commit = git_commit(&pkg);
    let mut c = Canvas::default();

    // ---- layout ----
    c.text(
        40.0,
        34.0,
        "Hybrid handoff — read/write path divergence",
        Text {
            size: 20.0,
            weight: "bold",
            ..Default::default()
        },
    );
    c.text(
        W - 40.0,
        34.0,
        &format!("reference architecture · modeled by segbench mode=hybrid @ {commit}"),
        Text {
            size: 12.0,
            anchor: "end",
            fill: MUTED,
            ..Default::default()
        },
    );
    c.text(
        40.0,
        52.0,
        "thread model belongs to the server built on segcache; cache-rs ships the engine band only",
        Text {
            size: 12.0,
            italic: true,
            fill: MUTED,
            ..Default::default()
        },
    );

    let (lx, lw) = (40.0, 1100.0);
    // client (external: italic + dashed)
    c.box_(
        lx,
        60.0,
        lw,
        66.0,
        Style {
            fill: "white",
            dash: "6 4",
            ..Default::default()
        },
    );
    c.text(
        lx + 12.0,
        84.0,
        "client connections",
        Text {
            size: 17.0,
            italic: true,
            ..Default::default()
        },
    );
    c.text(
        lx + 12.0,
        106.0,
        "pipelined requests per connection; replies retired in request order",
        Text {
            size: 13.0,
            italic: true,
            fill: MUTED,
            ..Default::default()
        },
    );

    // worker lane
    c.box_(lx, 166.0, lw, 116.0, Style::default());
    c.text(
        lx + 12.0,
        190.0,
        "worker thread ×W",
        Text {
            size: 17.0,
            ..Default::default()
        },
    );
    let mut cx = lx + 12.0;
    cx += c.chip(cx, 200.0, "parse / reply", PINK) + 8.0;
    cx += c.chip(cx, 200.0, "direct reads", PINK) + 8.0;
    let _ = c.chip(cx, 200.0, "store buffer (pending writes)", GREEN);

    // owner box (right half only: owners participate in the write path alone)
    c.box_(560.0, 322.0, 580.0, 92.0, Style::default());
    c.text(
        572.0,
        346.0,
        "owner thread ×S",
        Text {
            size: 17.0,
            ..Default::default()
        },
    );
    c.text(
        572.0,
        364.0,
        "single writer per key range",
        Text {
            size: 13.0,
            fill: MUTED,
            ..Default::default()
        },
    );
    let mut ox = 572.0;
    ox += c.chip(ox, 376.0, "apply writes", GREEN) + 8.0;
    let _ = c.chip(ox, 376.0, "TTL stripe = private tail", BLUE);

    // shared engine band
    c.box_(
        lx,
        454.0,
        lw,
        96.0,
        Style {
            fill: GRAY,
            ..Default::default()
        },
    );
    c.text(
        lx + 12.0,
        478.0,
        "shared segcache engine",
        Text {
            size: 17.0,
            ..Default::default()
        },
    );
    c.text(
        lx + 12.0,
        496.0,
        "Arc<Segcache>",
        Text {
            size: 14.0,
            family: MONO,
            fill: MUTED,
            ..Default::default()
        },
    );
    c.rect(
        430.0,
        470.0,
        300.0,
        34.0,
        Style {
            fill: "white",
            ..Default::default()
        },
    );
    c.text(
        444.0,
        492.0,
        "hashtable — publish point",
        Text {
            size: 14.0,
            family: MONO,
            ..Default::default()
        },
    );
    c.rect(
        760.0,
        470.0,
        330.0,
        34.0,
        Style {
            fill: "white",
            ..Default::default()
        },
    );
    c.text(
        774.0,
        492.0,
        "segments + tails + free pool + merge",
        Text {
            size: 14.0,
            family: MONO,
            ..Default::default()
        },
    );
    c.text(430.0, 538.0,
        "the pointer swing is the linearization point; readers pin + revalidate (unchanged from 0.4.x)",
        Text { size: 12.0, italic: true, fill: MUTED, ..Default::default() });

    // ---- READ path (left, x 150..430): R1..R4 ----
    c.text(
        60.0,
        345.0,
        "READ PATH",
        Text {
            size: 14.0,
            weight: "bold",
            fill: MUTED,
            ..Default::default()
        },
    );
    c.text(
        60.0,
        364.0,
        "never queues",
        Text {
            size: 13.0,
            italic: true,
            fill: MUTED,
            ..Default::default()
        },
    );
    c.vline_arrow(200.0, 126.0, 166.0, 2.4);
    c.badge(182.0, 146.0, "R1");
    c.text(212.0, 150.0, "request bytes", Text::default());
    c.vline_arrow(260.0, 282.0, 454.0, 1.4);
    c.badge(242.0, 300.0, "R2");
    c.text(
        272.0,
        304.0,
        "get(key)",
        Text {
            family: MONO,
            ..Default::default()
        },
    );
    c.vline_arrow(320.0, 454.0, 282.0, 1.4);
    c.badge(302.0, 438.0, "R3");
    c.text(332.0, 434.0, "value (zero-copy)", Text::default());
    c.vline_arrow(380.0, 166.0, 126.0, 2.4);
    c.badge(362.0, 146.0, "R4");
    c.text(392.0, 150.0, "reply", Text::default());

    // ---- WRITE path (right, x 600..1110): W1..W7 ----
    c.text(
        620.0,
        310.0,
        "WRITE PATH — delegated",
        Text {
            size: 14.0,
            weight: "bold",
            fill: MUTED,
            ..Default::default()
        },
    );
    c.vline_arrow(640.0, 126.0, 166.0, 2.4);
    c.badge(622.0, 146.0, "W1");
    c.text(652.0, 150.0, "request bytes", Text::default());
    c.rect(
        612.0,
        236.0,
        210.0,
        28.0,
        Style {
            fill: "white",
            rx: 3.0,
            ..Default::default()
        },
    );
    c.badge(598.0, 250.0, "W2");
    c.text(624.0, 255.0, "append to store buffer", Text::default());
    c.text(
        612.0,
        278.0,
        "pipelined same-key GET is forwarded from here",
        Text {
            size: 12.0,
            italic: true,
            fill: MUTED,
            ..Default::default()
        },
    );
    c.vline_arrow(840.0, 282.0, 322.0, 1.4);
    c.queue_glyph(840.0, 302.0);
    c.badge(878.0, 296.0, "W3");
    c.text(894.0, 300.0, "batch ≤16", Text::default());
    c.vline_arrow(900.0, 414.0, 454.0, 1.4);
    c.badge(882.0, 430.0, "W4");
    c.text(
        912.0,
        436.0,
        "insert(key, ttl_stripe)",
        Text {
            family: MONO,
            ..Default::default()
        },
    );
    c.badge(744.0, 487.0, "W5");
    c.text(
        744.0,
        514.0,
        "publish",
        Text {
            size: 12.0,
            anchor: "middle",
            fill: MUTED,
            ..Default::default()
        },
    );
    c.vline_arrow(1010.0, 322.0, 282.0, 1.4);
    c.badge(992.0, 306.0, "W6");
    c.text(1022.0, 306.0, "ack (applied) †", Text::default());
    c.vline_arrow(1070.0, 166.0, 126.0, 2.4);
    c.badge(1052.0, 146.0, "W7");
    c.text(1082.0, 150.0, "reply †", Text::default());

    // ---- legend + footnote ----
    let ly = 586.0;
    c.elements.push(format!(
        "<line x1=\"{lx}\" y1=\"{ly}\" x2=\"{}\" y2=\"{ly}\" stroke=\"{INK}\" stroke-width=\"2.4\"/>",
        lx + 46.0
    ));
    c.text(
        lx + 54.0,
        ly + 4.0,
        "process boundary",
        Text {
            fill: MUTED,
            ..Default::default()
        },
    );
    c.elements.push(format!(
        "<line x1=\"{}\" y1=\"{ly}\" x2=\"{}\" y2=\"{ly}\" stroke=\"{INK}\" stroke-width=\"1.4\"/>",
        lx + 200.0,
        lx + 246.0
    ));
    c.text(
        lx + 254.0,
        ly + 4.0,
        "internal",
        Text {
            fill: MUTED,
            ..Default::default()
        },
    );
    c.rect(
        lx + 340.0,
        ly - 10.0,
        46.0,
        20.0,
        Style {
            fill: "white",
            dash: "6 4",
            ..Default::default()
        },
    );
    c.text(
        lx + 394.0,
        ly + 4.0,
        "external",
        Text {
            italic: true,
            fill: MUTED,
            ..Default::default()
        },
    );
    c.queue_glyph(lx + 510.0, ly);
    c.text(
        lx + 546.0,
        ly + 4.0,
        "bounded channel",
        Text {
            fill: MUTED,
            ..Default::default()
        },
    );
    let mut cxl = lx + 680.0;
    cxl += c.chip(cxl, ly - 11.0, "protocol", PINK) + 6.0;
    cxl += c.chip(cxl, ly - 11.0, "storage", BLUE) + 6.0;
    let _ = c.chip(cxl, ly - 11.0, "runtime", GREEN);
    c.text(
        lx,
        626.0,
        "† design contract (reply released only after W5 publish — reply-after-apply); \
         segbench models the throughput path, not reply release.",
        Text {
            size: 12.0,
            italic: true,
            fill: MUTED,
            ..Default::default()
        },
    );
    c.text(
        lx,
        646.0,
        "RMW/CAS ops travel the write path and return applied state in the ack; \
         `noreply` forms may skip W6/W7 — they are the protocol's early-ack path.",
        Text {
            size: 12.0,
            italic: true,
            fill: MUTED,
            ..Default::default()
        },
    );

    // ---- bounds check: everything inside the canvas ----
    for &(x1, y1, x2, y2) in &c.bounds {
        assert!(
            0.0 <= x1 && x2 <= W && 0.0 <= y1 && y2 <= H,
            "element out of bounds: {:?}",
            (x1, y1, x2, y2)
        );
    }

    let mut svg = String::new();
    let _ = write!(
        svg,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {W} {H}\" font-family=\"{SANS}\">\n\
         <rect width=\"{W}\" height=\"{H}\" fill=\"white\"/>\n"
    );
    svg.push_str(&c.elements.join("\n"));
    svg.push_str("\n</svg>\n");

    let out = out_dir.join("handoff-dataflow.svg");
    std::fs::write(&out, &svg).unwrap_or_else(|e| panic!("{}: {e}", out.display()));
    println!(
        "wrote handoff-dataflow.svg ({} bytes), claims OK, commit {commit}",
        svg.len()
    );
}
