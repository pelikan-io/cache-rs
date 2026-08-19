#!/usr/bin/env python3
"""Byte-anatomy SVG for the ziplist hash golden block.

Derives every span from the actual frozen bytes (tests/golden.rs,
commit a68fd2d)\nRegenerate: python3 docs/diagrams/ziplist_anatomy.py > docs/diagrams/ziplist-block-anatomy.svg via a mini-decoder that re-implements the spec; any
mismatch between decoder and bytes aborts. Single-use chart per the
diagram skills' no-charter case.
"""
import sys

# The 27-byte hash golden fixture: hset("5","9"), hset("z","ab")
BYTES = bytes([
    0x01, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x16, 0x00, 0x00, 0x00,
    0x05, 0x01,
    0xFF, 0x01, 0x39, 0x03,
    0xFF, 0x01, 0x7A, 0x03,
    0xFF, 0x02, 0x61, 0x62, 0x04,
])

def fail(msg):
    print(f"DERIVATION FAILURE: {msg}", file=sys.stderr); sys.exit(1)

# --- mini-decoder (fail-loud) ---
if len(BYTES) < 12: fail("truncated header")
type_, fmt = BYTES[0], BYTES[1]
flags = int.from_bytes(BYTES[2:4], "little")
nentry = int.from_bytes(BYTES[4:8], "little")
tail_off = int.from_bytes(BYTES[8:12], "little")
if type_ != 1 or fmt != 0 or flags != 0: fail("unexpected header fields")

entries = []  # (start, tag_end, data_end, backlen_end, desc)
off = 12
while off < len(BYTES):
    tag = BYTES[off]
    if tag <= 250:
        data_end, desc = off + 1, f"Uint({tag})"
    elif tag == 255:
        # forward varint length (all fixture lengths are 1-byte varints)
        ln = BYTES[off + 1]
        if ln & 0x80: fail("multi-byte varint in fixture; decoder shortcut invalid")
        data_end = off + 2 + ln
        s = BYTES[off + 2:data_end].decode()
        desc = f'Str("{s}")'
    else:
        fail(f"tier tag {tag} not in fixture")
    # backlen: 1 byte in fixture; verify it equals tag+data length
    bl = BYTES[data_end]
    if bl & 0x80: fail("multi-byte backlen in fixture")
    if bl != data_end - off: fail(f"backlen {bl} != tag+data {data_end - off} at {off}")
    entries.append((off, off + 1, data_end, data_end + 1, desc))
    off = data_end + 1
if off != len(BYTES): fail("trailing bytes")
if len(entries) != nentry: fail(f"nentry {nentry} != walked {len(entries)}")
if entries[-1][0] != tail_off: fail(f"tail_off {tail_off} != last entry start {entries[-1][0]}")

# --- SVG emission ---
CW, CH = 42, 34          # byte cell size
X0, Y0 = 26, 78          # grid origin (leaves room for header-field bracket row)
N = len(BYTES)
W = X0 * 2 + CW * N
H = 236

HDR_FILL = "#B3CDE3"      # header fields (storage blue)
TAG_FILL = "#FBB4AE"      # entry tag byte
DATA_FILL = "#CCEBC5"     # entry data bytes
BL_FILL = "#F6E3B4"       # backlen byte
INK = "#22303C"           # fixed dark ink on pastel fills

def esc(s): return s.replace("&", "&amp;").replace("<", "&lt;")

svg = []
svg.append(f'<svg viewBox="0 0 {W} {H}" role="img" aria-label="Byte-by-byte anatomy of a 27-byte ziplist hash block: 12-byte header (type, format, flags, nentry, tail_off) followed by four entries, each a tag byte, optional data bytes, and a backlen byte." xmlns="http://www.w3.org/2000/svg" style="max-width:100%;height:auto">')
svg.append(f'<style>text{{font-family:ui-monospace,SFMono-Regular,Menlo,monospace}}</style>')

def cell(i, fill):
    x = X0 + i * CW
    svg.append(f'<rect x="{x}" y="{Y0}" width="{CW}" height="{CH}" fill="{fill}" stroke="{INK}" stroke-opacity="0.35"/>')
    svg.append(f'<text x="{x + CW/2}" y="{Y0 + CH/2 + 4}" text-anchor="middle" font-size="13" fill="{INK}">{BYTES[i]:02X}</text>')

# fills per byte
fills = {}
for i in range(12): fills[i] = HDR_FILL
for (s, te, de, be, _d) in entries:
    fills[s] = TAG_FILL
    for i in range(te, de): fills[i] = DATA_FILL
    fills[de] = BL_FILL
for i in range(N): cell(i, fills[i])

# offset ruler under cells
for i in range(N):
    x = X0 + i * CW + CW / 2
    svg.append(f'<text x="{x}" y="{Y0 + CH + 16}" text-anchor="middle" font-size="10" fill="currentColor" opacity="0.55">{i}</text>')

def bracket(x1, x2, y, label, above=True, size=12, mono=True):
    tick = 5 if above else -5
    svg.append(f'<path d="M {x1} {y + tick} V {y} H {x2} V {y + tick}" fill="none" stroke="currentColor" stroke-width="1.1"/>')
    ty = y - 6 if above else y + 16
    fam = '' if mono else ' font-family="system-ui,sans-serif"'
    svg.append(f'<text x="{(x1 + x2) / 2}" y="{ty}" text-anchor="middle" font-size="{size}"{fam} fill="currentColor">{esc(label)}</text>')

# header field brackets (above, two staggered rows to avoid label collisions)
hdr_fields = [(0, 1, "type=Hash"), (1, 2, "format"), (2, 4, "flags"), (4, 8, "nentry=4"), (8, 12, "tail_off=22")]
for idx, (a, b, lab) in enumerate(hdr_fields):
    y = Y0 - 10 if idx % 2 == 0 else Y0 - 38
    bracket(X0 + a * CW + 2, X0 + b * CW - 2, y, lab)

# entry brackets (below ruler)
YB = Y0 + CH + 30
labels = ["field", "value", "field", "value"]
for (s, te, de, be, d), role in zip(entries, labels):
    bracket(X0 + s * CW + 2, X0 + be * CW - 2, YB, f"{role} {d}", above=False)

# tail_off arrow: from header byte 8 down-around to last entry start
lx = X0 + entries[-1][0] * CW + CW / 2
hx = X0 + 8 * CW + CW / 2
ya = Y0 + CH + 58
svg.append(f'<path d="M {hx} {Y0 + CH + 24} V {ya + 26} H {lx} V {YB + 30}" fill="none" stroke="currentColor" stroke-width="1.2" stroke-dasharray="4 3" marker-end="url(#arr)"/>')
svg.append(f'<text x="{(hx + lx) / 2}" y="{ya + 40}" text-anchor="middle" font-size="11" font-family="system-ui,sans-serif" fill="currentColor">tail_off points at the last entry’s first byte → O(1) tail access</text>')
svg.append('<defs><marker id="arr" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="7" markerHeight="7" orient="auto"><path d="M0 0 L8 4 L0 8 z" fill="currentColor"/></marker></defs>')
svg.append('</svg>')

out = "\n".join(svg)
# standalone output for GitHub <img> rendering: fixed ink + opaque ground
out = out.replace("currentColor", INK)
head_end = out.index(">") + 1
out = out[:head_end] + f'<rect x="0" y="0" width="{W}" height="{H}" fill="#FCFCFA"/>' + out[head_end:]
# bounds check: no cell beyond viewBox
if X0 + N * CW > W - X0 + 1: fail("cells overflow viewBox")
print(out)
print(f"\n<!-- derived OK: {N} bytes, nentry={nentry}, tail_off={tail_off}, entries={[d for *_, d in entries]} -->", file=sys.stderr)
