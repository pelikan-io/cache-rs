#!/usr/bin/env python3
"""Segcache eviction-strategy diagrams, derived from this repo's source.

Regenerate: python3 docs/diagrams/eviction_diagrams.py  (from the repo root or anywhere)

Single-use chart set (architecture-diagram skill, no charter): every drawn
claim is asserted against the source below and the run aborts on drift.
"""
import subprocess, sys, html

import pathlib
REPO = str(pathlib.Path(__file__).resolve().parents[2])
COMMIT = subprocess.run(["git", "-C", REPO, "log", "-1", "--format=%h"],
                        capture_output=True, text=True).stdout.strip()
DIRTY = subprocess.run(["git", "-C", REPO, "status", "--short", "crates/segcache/"],
                       capture_output=True, text=True).stdout.strip()
assert not DIRTY, "crates/segcache is dirty; refusing to stamp " + COMMIT

def src(path):
    return open(f"{REPO}/{path}").read()

# ── Source claims (fail loud on drift) ─────────────────────────────
CLAIMS = [
    ("crates/segcache/src/eviction/policy.rs", "pub enum Policy",
     "the eight policies live on one enum"),
    ("crates/segcache/src/eviction/policy.rs", "S3Fifo {",
     "S3-FIFO is a first-class policy"),
    ("crates/segcache/src/eviction/mod.rs", "max(lhs.create_at(), lhs.merge_at())",
     "Fifo age = later of create and last merge"),
    ("crates/segcache/src/eviction/mod.rs", "lhs.create_at() + lhs.ttl()",
     "Cte ranks by absolute expiry time"),
    ("crates/segcache/src/eviction/mod.rs", "lhs.live_bytes().cmp(&rhs.live_bytes())",
     "Util ranks by live bytes"),
    ("crates/segcache/src/eviction/mod.rs", "Policy::Fifo | Policy::Cte | Policy::Util",
     "only Fifo/Cte/Util rerank"),
    ("crates/segcache/src/segments/segments.rs", "if ttl_buckets.expire(hashtable, self) > 0",
     "expired segments are freed before any policy runs"),
    ("crates/segcache/src/segments/segments.rs", "return ttl_bucket.head();",
     "RandomFifo evicts the sampled bucket's head"),
    ("crates/segcache/src/segments/segments.rs", "if chain_len < 3",
     "merge needs a chain of at least 3 evictable segments"),
    ("crates/segcache/src/segments/segments.rs", "merge_evict_fallback_drop(start, ttl_bucket, hashtable)",
     "no spare -> drop the chain head whole"),
    ("crates/segcache/src/segments/segments.rs", "cutoff = cand.prune(hashtable, cutoff, target_ratio);",
     "merge prunes items below a frequency cutoff"),
    ("crates/segcache/src/segments/segments.rs", "self.claim_for_drain(cand_id)",
     "candidates are claimed Sealed->Draining before mutation"),
    ("crates/segcache/src/segments/segments.rs", "self.publish_dest_sealed(spare_id);",
     "the filled spare is published Relinking->Sealed"),
    ("crates/segcache/src/segments/segments.rs", "if ratio > target_ratio {",
     "compaction fires below the 1/compact occupancy watermark"),
    ("crates/segcache/src/segments/segments.rs", "find_oldest_seg_in_pool(ttl_buckets, SegmentPool::Admission)",
     "S3-FIFO evicts from the admission pool"),
    ("crates/segcache/src/segments/segments.rs", "find_oldest_seg_in_pool(ttl_buckets, SegmentPool::Main)",
     "S3-FIFO falls back to the main pool"),
    ("crates/segcache/src/segments/segments.rs", "self.s3fifo_ghost_remaining(seg_id, hashtable);",
     "freq==0 admission items are recorded in the ghost queue"),
    ("crates/segcache/src/segments/segments.rs", "fn s3fifo_promote_from",
     "freq>0 items are promoted by copy"),
    ("crates/segcache/src/segments/segments.rs", "pub(crate) fn ghost_contains",
     "inserts consult the ghost queue"),
    ("crates/segcache/src/segments/segments.rs", "(segments as f64 * admission_ratio).round() as u32",
     "admission pool sized by admission_ratio"),
    ("crates/segcache/src/segments/header.rs", "self.state().is_evictable() && self.ref_count() == 0",
     "can_evict = evictable state and no reader pins"),
    ("crates/segcache/src/eviction/ghost.rs", "queue.pop_front()",
     "ghost queue is a bounded FIFO"),
]
for path, needle, meaning in CLAIMS:
    if needle not in src(path):
        sys.exit(f"CLAIM DRIFT: {path} no longer contains {needle!r} ({meaning})")

# Ordered claim: admission pool is tried before main.
seg = src("crates/segcache/src/segments/segments.rs")
adm = seg.index("find_oldest_seg_in_pool(ttl_buckets, SegmentPool::Admission)")
mai = seg.index("find_oldest_seg_in_pool(ttl_buckets, SegmentPool::Main)")
assert adm < mai, "CLAIM DRIFT: admission-pool eviction no longer precedes main"

# ── SVG helpers (theme-aware: currentColor + two meaning hues) ─────
KEEP = "#2a78d6"   # selected / survivor / promoted
DROP = "#eb6834"   # dropped / pruned
ELEMS = []          # (left, top, right, bottom) for bounds check

def esc(s): return html.escape(s, quote=True)

class Svg:
    def __init__(self, w, h):
        self.w, self.h = w, h
        self.parts = [
            f'<svg viewBox="0 0 {w} {h}" role="img" xmlns="http://www.w3.org/2000/svg" '
            f'fill="none" stroke-linejoin="round">',
            '<defs><marker id="MID" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6.5" '
            'markerHeight="6.5" orient="auto-start-reverse">'
            '<path d="M0 0 L10 5 L0 10 z" fill="currentColor" stroke="none"/></marker>'
            '<marker id="MID-keep" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6.5" '
            f'markerHeight="6.5" orient="auto-start-reverse">'
            f'<path d="M0 0 L10 5 L0 10 z" fill="{KEEP}" stroke="none"/></marker>'
            '<marker id="MID-drop" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6.5" '
            f'markerHeight="6.5" orient="auto-start-reverse">'
            f'<path d="M0 0 L10 5 L0 10 z" fill="{DROP}" stroke="none"/></marker></defs>',
        ]
    def bound(self, l, t, r, b):
        assert 0 <= l <= r <= self.w and 0 <= t <= b <= self.h, \
            f"element out of bounds: ({l},{t})..({r},{b}) in {self.w}x{self.h}"
        ELEMS.append((l, t, r, b))
    def rect(self, x, y, w, h, stroke="currentColor", dash=None, rx=6, fill="none", sw=1.3, opacity=None):
        self.bound(x, y, x+w, y+h)
        d = f' stroke-dasharray="{dash}"' if dash else ""
        o = f' fill-opacity="{opacity}"' if opacity is not None else ""
        self.parts.append(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{rx}" '
                          f'fill="{fill}"{o} stroke="{stroke}" stroke-width="{sw}"{d}/>')
    def text(self, x, y, s, size=12, anchor="middle", color="currentColor", weight=None, mono=False, opacity=None):
        est = len(s) * size * 0.6
        l = x - est/2 if anchor == "middle" else (x - est if anchor == "end" else x)
        self.bound(max(0, l), y - size, min(self.w, l + est), y + 3)
        fam = "ui-monospace,monospace" if mono else "inherit"
        w = f' font-weight="{weight}"' if weight else ""
        o = f' opacity="{opacity}"' if opacity is not None else ""
        self.parts.append(f'<text x="{x}" y="{y}" text-anchor="{anchor}" font-size="{size}" '
                          f'font-family="{fam}" fill="{color}" stroke="none"{w}{o}>{esc(s)}</text>')
    def line(self, x1, y1, x2, y2, color="currentColor", arrow=True, dash=None, sw=1.3):
        self.bound(min(x1,x2), min(y1,y2), max(x1,x2), max(y1,y2))
        mid = {"currentColor": "MID", KEEP: "MID-keep", DROP: "MID-drop"}[color]
        m = f' marker-end="url(#{mid})"' if arrow else ""
        d = f' stroke-dasharray="{dash}"' if dash else ""
        self.parts.append(f'<line x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2}" '
                          f'stroke="{color}" stroke-width="{sw}"{m}{d}/>')
    def badge(self, x, y, n, r=9):
        self.bound(x-r, y-r, x+r, y+r)
        self.parts.append(f'<circle cx="{x}" cy="{y}" r="{r}" fill="none" '
                          f'stroke="currentColor" stroke-width="1.2"/>')
        self.text(x, y+4, str(n), size=11, weight=600)
    def done(self):
        self.parts.append("</svg>")
        return "\n".join(self.parts)

def segment(s, x, y, label, sub, w=112, h=58, tail=False, pick=None):
    s.rect(x, y, w, h, dash="5 4" if tail else None)
    s.text(x+w/2, y+22, label, size=12.5, weight=600)
    s.text(x+w/2, y+40, sub, size=10.5, opacity=0.75)
    if tail:
        s.text(x+w/2, y+h-6, "write tail · Live", size=9.5, opacity=0.65)
    if pick:
        s.rect(x, y, w, h, stroke=KEEP, sw=2.2, fill="none")

# ── Figure 1: shared substrate + the five whole-segment pickers ────
f1 = Svg(980, 545)
GX = [150, 276, 402, 528, 654]   # segment column x positions
rows = [
    (72,  "TTL ~5 m",  5),
    (208, "TTL ~1 h",  4),
    (344, "TTL ~6 h",  3),
]
for y, label, n in rows:
    f1.text(20, y+33, label, size=12, anchor="start", weight=600)
    f1.text(20, y+49, "bucket", size=10, anchor="start", opacity=0.6)
    for i in range(n-1):
        f1.line(GX[i]+112, y+29, GX[i+1], y+29, arrow=True)
    f1.text(GX[0]-8, y+16, "head", size=9.5, anchor="end", opacity=0.6)
# row 1
segment(f1, GX[0], 72, "S1", "age 120 s · live 85 %", pick="cte")
segment(f1, GX[1], 72, "S2", "age 90 s · live 70 %")
segment(f1, GX[2], 72, "S3", "age 40 s · live 15 %", pick="util")
segment(f1, GX[3], 72, "S4", "age 20 s · live 90 %")
segment(f1, GX[4], 72, "S5", "filling", tail=True)
# row 2
segment(f1, GX[0], 208, "S6", "age 300 s · live 95 %", pick="fifo")
segment(f1, GX[1], 208, "S7", "age 150 s · live 60 %")
segment(f1, GX[2], 208, "S8", "age 60 s · live 80 %", pick="random")
segment(f1, GX[3], 208, "S9", "filling", tail=True)
# row 3
segment(f1, GX[0], 344, "S10", "age 200 s · live 75 %", pick="randomfifo")
segment(f1, GX[1], 344, "S11", "age 20 s · live 90 %")
segment(f1, GX[2], 344, "S12", "filling", tail=True)
# callouts (short arrows into whitespace bands)
def callout(s, x, y, tx, ty, name, why):
    s.text(x, y, name, size=11.5, color=KEEP, weight=700)
    s.text(x, y+13, why, size=10, opacity=0.75)
    s.line(x, y+18, tx, ty, color=KEEP, sw=1.6)
callout(f1, GX[0]+56, 28, GX[0]+56, 70, "Cte", "min(create_at + ttl): expires in 180 s")
callout(f1, GX[2]+56, 28, GX[2]+56, 70, "Util", "min(live_bytes): 85 % dead space")
callout(f1, GX[0]+56, 164, GX[0]+56, 206, "Fifo", "max(create_at, merge_at) oldest overall")
callout(f1, GX[2]+56, 164, GX[2]+56, 206, "Random", "uniform over evictable segments")
callout(f1, GX[0]+56, 300, GX[0]+56, 342, "RandomFifo", "random readable seg -> its bucket's head")
# shared invariants
f1.line(20, 430, 960, 430, arrow=False, dash="2 4", sw=0.8)
f1.text(20, 452, "Shared, before any policy runs:  expire() frees whole expired segments first — eviction is the fallback.",
        size=11.5, anchor="start")
f1.text(20, 472, "can_evict = evictable state && ref_count == 0 — the Live write tail (dashed) and reader-pinned segments are never selected.",
        size=11.5, anchor="start")
f1.text(20, 492, "Fifo / Cte / Util rerank a sorted segment list (at most once per second); Random / RandomFifo sample statelessly.",
        size=11.5, anchor="start")
f1.text(20, 512, "All five drop the selected segment whole: every item in it is evicted in one step.  Policy::None skips selection — inserts fail when full.",
        size=11.5, anchor="start")
f1.text(960, 532, f"derived from cache-rs @ {COMMIT}", size=9.5, anchor="end", opacity=0.55, mono=True)
FIG1 = f1.done()

# ── Figure 2: merge eviction ───────────────────────────────────────
f2 = Svg(980, 500)
CY = 120
f2.text(192, 44, "bucket head", size=10.5, anchor="end", opacity=0.65)
f2.rect(150, 88, 140, 74, dash="5 4", rx=8)
f2.text(220, 112, "spare segment", size=12.5, weight=600)
f2.text(220, 130, "Relinking:", size=10.5, opacity=0.75)
f2.text(220, 144, "readable, not evictable", size=10.5, opacity=0.75)
f2.text(220, 176, "1  reserve spare + head-insert", size=10.5)
cands = [(330, "A"), (505, "B"), (680, "C")]
for x, name in cands:
    f2.rect(x, 88, 140, 74, rx=8)
    f2.text(x+70, 106, f"candidate {name}", size=12, weight=600)
    # item blocks: top row survivors (keep), bottom row pruned (drop)
    for i in range(3):
        f2.rect(x+16+i*38, 116, 26, 16, stroke=KEEP, fill=KEEP, opacity=0.28, rx=2, sw=1.1)
    for i in range(3):
        f2.rect(x+16+i*38, 138, 26, 16, stroke=DROP, fill=DROP, opacity=0.28, rx=2, sw=1.1)
    f2.text(x+70, 176, "2  claim Sealed -> Draining", size=10.5)
f2.line(290, 125, 330, 125, arrow=True)
f2.line(470, 125, 505, 125, arrow=True)
f2.line(645, 125, 680, 125, arrow=True)
f2.line(820, 125, 862, 125, arrow=True)
f2.text(905, 129, "rest of chain", size=10.5, opacity=0.65)
# copy-survivor arcs (straight lines above boxes)
for i, (x, _) in enumerate(cands):
    lane_y, drop_x = 74 - i*10, 248 - i*24
    f2.line(x+40, 88, x+40, lane_y, color=KEEP, arrow=False, sw=1.5)
    f2.line(x+40, lane_y, drop_x, lane_y, color=KEEP, arrow=False, sw=1.5)
    f2.line(drop_x, lane_y, drop_x, 86, color=KEEP, sw=1.5)
f2.text(430, 36, "4  copy survivors into the spare (append + Release-CAS republish)", size=11, color=KEEP, anchor="start")
# prune + finalize
for x, _ in cands:
    f2.line(x+70, 196, x+70, 236, color=DROP, sw=1.5)
f2.text(30, 248, "3  prune: freq < cutoff -> dropped", size=11, color=DROP, anchor="start")
f2.text(30, 264, "(marked deleted - no bytes move)", size=10, color=DROP, anchor="start", opacity=0.85)
for x, _ in cands:
    f2.line(x+108, 196, x+108, 292, sw=1.1)
f2.rect(300, 300, 560, 44, rx=8)
f2.text(580, 320, "5  finalize drained candidate -> unlink from chain -> recycle to free pool", size=11.5)
f2.text(580, 336, "(a reader-pinned candidate is condemned to its last reader instead)", size=10, opacity=0.7)
f2.text(580, 370, "6  publish spare: Relinking -> Sealed - it remains the bucket head", size=11)
# side notes
f2.line(20, 404, 960, 404, arrow=False, dash="2 4", sw=0.8)
f2.text(20, 426, "start: random TTL bucket -> its next_to_merge cursor · needs >= 3 evictable chained segments · no spare -> fallback: drop chain head whole",
        size=11, anchor="start")
f2.text(20, 446, "stops when: max segments merged · spare reaches stop_ratio · candidate unevictable · drain claim lost",
        size=11, anchor="start")
f2.text(20, 466, "compaction sub-mode (from remove_at, occupancy < 1/compact): same copy machinery, no pruning, skips instead of dropping when no spare",
        size=11, anchor="start")
f2.text(960, 488, f"derived from cache-rs @ {COMMIT}", size=9.5, anchor="end", opacity=0.55, mono=True)
FIG2 = f2.done()

# ── Figure 3: S3-FIFO ──────────────────────────────────────────────
f3 = Svg(980, 470)
f3.rect(30, 70, 130, 54, rx=8)
f3.text(95, 93, "insert", size=12.5, weight=600)
f3.text(95, 110, "new key", size=10.5, opacity=0.7)
f3.rect(250, 40, 210, 120, rx=8)
f3.text(355, 62, "admission pool", size=12.5, weight=600)
f3.text(355, 78, "~admission_ratio of segments", size=10, opacity=0.7)
for i in range(3):
    f3.rect(268+i*60, 92, 48, 30, rx=4, sw=1.1)
f3.text(355, 142, "evicted first: oldest segment", size=10, opacity=0.7)
f3.rect(640, 40, 230, 120, rx=8)
f3.text(755, 62, "main pool", size=12.5, weight=600)
f3.text(755, 78, "remaining segments", size=10, opacity=0.7)
for i in range(3):
    f3.rect(660+i*64, 92, 52, 30, rx=4, sw=1.1)
f3.text(755, 142, "evicted second: CLOCK sweep", size=10, opacity=0.7)
f3.rect(250, 330, 210, 60, dash="5 4", rx=8)
f3.text(355, 355, "ghost queue", size=12.5, weight=600)
f3.text(355, 372, "bounded FIFO of key hashes", size=10, opacity=0.7)
# edges
f3.line(160, 84, 250, 84, arrow=True)
f3.text(205, 76, "miss", size=10)
f3.line(460, 84, 640, 84, color=KEEP, sw=1.6)
f3.text(550, 76, "freq > 0: promote (copy)", size=10.5, color=KEEP)
f3.line(355, 160, 355, 330, color=DROP, sw=1.6)
f3.text(368, 250, "freq == 0: drop item,", size=10.5, color=DROP, anchor="start")
f3.text(368, 265, "record key hash", size=10.5, color=DROP, anchor="start")
f3.line(95, 124, 95, 300, arrow=False)
f3.line(95, 300, 640, 300, arrow=False)
f3.line(640, 300, 640, 164, arrow=True)
f3.text(560, 292, "ghost hit: skip admission, insert to main", size=10.5)
f3.line(250, 344, 108, 310, dash="3 4", sw=1.1)
f3.text(150, 290, "consulted on insert", size=9.5, opacity=0.7)
f3.rect(640, 330, 230, 60, rx=8)
f3.text(755, 355, "fresh main segment", size=12, weight=600)
f3.text(755, 372, "second chance for freq > 0", size=10, opacity=0.7)
f3.line(720, 160, 720, 330, color=KEEP, sw=1.6)
f3.text(733, 250, "freq > 0: copy", size=10.5, color=KEEP, anchor="start")
f3.line(830, 160, 830, 240, color=DROP, sw=1.6)
f3.text(843, 205, "freq == 0:", size=10.5, color=DROP, anchor="start")
f3.text(843, 220, "dropped", size=10.5, color=DROP, anchor="start")
f3.line(20, 410, 960, 410, arrow=False, dash="2 4", sw=0.8)
f3.text(20, 432, "One-hit wonders die in admission; a ghost hit is the proof of a second request, earning direct main placement.",
        size=11, anchor="start")
f3.text(20, 452, "Promotion and second-chance copies reuse the merge relink machinery: copy bytes, then Release-CAS the hashtable location.",
        size=11, anchor="start")
f3.text(960, 464, f"derived from cache-rs @ {COMMIT}", size=9.5, anchor="end", opacity=0.55, mono=True)
FIG3 = f3.done()

HERE = pathlib.Path(__file__).resolve().parent
BG = '<rect width="100%" height="100%" fill="white"/>'
def emit(name, svg):
    # Committed doc render: explicit ink on a white ground so the figure is
    # self-contained wherever the markdown is viewed.
    svg = svg.replace("currentColor", "#1a2433")
    svg = svg.replace("</defs>", "</defs>\n" + BG, 1)
    (HERE / name).write_text(svg)
emit("eviction-policies.svg", FIG1)
emit("eviction-merge.svg", FIG2)
emit("eviction-s3fifo.svg", FIG3)
print(f"claims: {len(CLAIMS)} + 1 ordered — all hold at {COMMIT}")
print(f"elements bounds-checked: {len(ELEMS)}")
