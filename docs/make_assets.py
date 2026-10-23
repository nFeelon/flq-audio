"""Regenerate docs assets (banner + charts). Numbers are transcribed from
testdata/results/*.md and AGENTS.md — update them there first.

    python docs/make_assets.py
"""
import os

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

OUT = os.path.join(os.path.dirname(__file__), "assets")
os.makedirs(OUT, exist_ok=True)

BG, FG, MUT = "#0d1117", "#e6edf3", "#8b949e"
BLUE, ORANGE, GREEN = "#58a6ff", "#f78166", "#3fb950"

# ---------------------------------------------------------------- banner
fig = plt.figure(figsize=(12.8, 6.4), dpi=100)
fig.patch.set_facecolor(BG)
ax = fig.add_axes([0, 0, 1, 1]); ax.set_facecolor(BG)
ax.set_xlim(0, 1280); ax.set_ylim(0, 640); ax.axis("off")

ax.text(70, 400, "FLQ", fontsize=110, fontweight="bold", color=FG,
        family="DejaVu Sans", va="center")
ax.text(74, 305, "Filon Lossless Quality", fontsize=26, color=BLUE, va="center")
ax.text(74, 250, "framed lossless audio\nseekable multi-track container",
        fontsize=15, color=MUT, va="center", linespacing=1.6)

# three source tracks -> one container
rng = np.random.default_rng(7)
t = np.linspace(0, 4 * np.pi, 400)
def wave(seed_phase, f1, f2):
    env = np.abs(np.sin(t * 0.5 + seed_phase)) ** 0.8
    sig = (np.sin(t * f1 + seed_phase) * 0.6 + np.sin(t * f2) * 0.3
           + rng.normal(0, 0.08, t.size)) * env
    return sig

x0, x1 = 640, 1180                      # tracks strip x-range
ytop, dy, h = 500, 105, 42              # strips: top y, spacing, height
for i, (ph, f1, f2, c) in enumerate([(0.3, 5, 11, BLUE), (1.1, 4, 9, GREEN), (2.2, 6, 8, "#d2a8ff")]):
    y = ytop - i * dy
    s = wave(ph, f1, f2)
    xs = np.linspace(x0, x1, s.size)
    ax.plot(xs, y + s * (h * 0.45), color=c, lw=1.1)
    ax.text(x0 - 14, y, f"track {i+1}", fontsize=11, color=MUT, ha="right", va="center")

# container: a row of independent CRC'd frames (frame-major/track-minor)
cy, ch, bw, gap = 150, 60, 46, 6
track_cols = [BLUE, GREEN, "#d2a8ff"]
for i in range(11):
    x = x0 + i * (bw + gap)
    ax.add_patch(plt.Rectangle((x, cy), bw, ch,
                               facecolor=track_cols[i % 3], alpha=0.75,
                               edgecolor=BLUE, lw=0.8))
ax.text(x0, cy - 26, "bundle.flq — every frame independently decodable",
        fontsize=12, color=MUT, va="center")
fig.savefig(os.path.join(OUT, "banner.png"))
plt.close(fig)
print("wrote docs/assets/banner.png")

# ----------------------------------------- chart 1: size by input format
# .flq size as % of source, aggregate over the corpus's accepted files
# (lower = better). Data: testdata/results/2026-10-23_sweep256.md.
fmt = [
    ("FLAC input (24)", 97.8),
    ("AIFF (6)", 77.7),
    ("WAV int (79)", 54.0),
    ("WAV float (63)", 50.5),
]
fig, ax = plt.subplots(figsize=(7.2, 2.9))
bars = ax.barh([n for n, _ in fmt], [v for _, v in fmt], color=BLUE, height=0.62)
for b, v in zip(bars, [v for _, v in fmt]):
    ax.text(v + 1.5, b.get_y() + b.get_height() / 2, f"{v:.1f}%",
            va="center", fontsize=9, color=MUT)
ax.set_xlim(0, 118)
ax.set_xlabel(".flq size, % of source — aggregate per kind, lower is better", fontsize=9, color=MUT)
ax.tick_params(labelsize=9, colors=MUT, length=0)
for s in ("top", "right"):
    ax.spines[s].set_visible(False)
for s in ("left", "bottom"):
    ax.spines[s].set_color(MUT)
fig.patch.set_alpha(0); ax.set_facecolor("none")
fig.tight_layout()
fig.savefig(os.path.join(OUT, "compression-by-format.svg"), transparent=True)
plt.close(fig)
print("wrote docs/assets/compression-by-format.svg")

# ------------------------------- chart 2: FLQ vs reference codecs
# output size as % of source on the 47 corpus files every tool accepted
# (identical inputs — see testdata/results/2026-10-22_e254a5b.md; FLQ row:
# 2026-10-22_9f25157.md, frame_len 16384).
ref = [
    ("Zstd --ultra -22", 78.08, MUT),
    ("7-Zip LZMA2 -mx9", 64.60, MUT),
    ("WavPack -hx", 52.04, BLUE),
    ("FLAC -8", 51.15, BLUE),
    ("Monkey's Audio -c4000", 50.85, BLUE),
    ("FLQ encode", 50.27, ORANGE),
    ("TAK -p4m", 49.88, BLUE),
    ("OptimFROG extranew-light", 48.40, BLUE),
]
fig, ax = plt.subplots(figsize=(7.2, 3.4))
bars = ax.barh([n for n, _, _ in ref], [v for _, v, _ in ref],
               color=[c for _, _, c in ref], height=0.62)
for b, v in zip(bars, [v for _, v, _ in ref]):
    ax.text(v + 1.0, b.get_y() + b.get_height() / 2, f"{v:.2f}%",
            va="center", fontsize=9, color=MUT)
ax.set_xlim(0, 90)
ax.set_xlabel("output size, % of source — identical 47-file subset, lower is better",
              fontsize=9, color=MUT)
ax.tick_params(labelsize=9, colors=MUT, length=0)
for s in ("top", "right"):
    ax.spines[s].set_visible(False)
for s in ("left", "bottom"):
    ax.spines[s].set_color(MUT)
fig.patch.set_alpha(0); ax.set_facecolor("none")
fig.tight_layout()
fig.savefig(os.path.join(OUT, "vs-reference-codecs.svg"), transparent=True)
plt.close(fig)
print("wrote docs/assets/vs-reference-codecs.svg")

# ----------------- chart 3: wall time, same file for every tool ----------
# 73.6 MiB i16 stereo 48 kHz WAV (~6.7 min audio), single encode+decode,
# peak working set polled. flq -j24 (all cores) vs references at their
# bench settings — incl. flac -j8 (its own multithreading).
# Data: testdata/results/2026-10-23_encspeed.md (D:\TEMP\flq_ab\mem_bench.py).
spd = [
    # (name, enc s, dec s, color)
    ("OptimFROG xnewl",      12.08, 8.98, BLUE),
    ("TAK -p4m",              2.06, 0.63, BLUE),
    ("FLQ encode",            1.67, 0.62, ORANGE),
    ("WavPack -hx",           1.37, 0.37, BLUE),
    ("Monkey's Audio -c4000", 0.60, 0.72, BLUE),
    ("FLAC -8 -j8",           0.22, 0.40, BLUE),
]
fig, ax = plt.subplots(figsize=(7.2, 3.2))
y = np.arange(len(spd))
bh = 0.36
b1 = ax.barh(y + bh / 2, [e for _, e, _, _ in spd], height=bh, color=[c for _, _, _, c in spd])
b2 = ax.barh(y - bh / 2, [d for _, _, d, _ in spd], height=bh, color=[c for _, _, _, c in spd], alpha=0.45)
for b, v in zip(b1, [e for _, e, _, _ in spd]):
    ax.text(v * 1.04, b.get_y() + b.get_height() / 2, f"{v:.2f}s", va="center", fontsize=8, color=MUT)
for b, v in zip(b2, [d for _, _, d, _ in spd]):
    ax.text(v * 1.04, b.get_y() + b.get_height() / 2, f"{v:.2f}s", va="center", fontsize=8, color=MUT)
ax.set_yticks(y, [n for n, _, _, _ in spd])
ax.invert_yaxis()
ax.set_xscale("log")
ax.set_xlim(0.2, 25)
ax.set_xlabel("wall seconds — upper solid bar encode, lower faint bar decode (log scale)",
              fontsize=9, color=MUT)
ax.tick_params(labelsize=9, colors=MUT, length=0)
for s in ("top", "right"):
    ax.spines[s].set_visible(False)
for s in ("left", "bottom"):
    ax.spines[s].set_color(MUT)
fig.patch.set_alpha(0); ax.set_facecolor("none")
fig.tight_layout()
fig.savefig(os.path.join(OUT, "speed-vs-codecs.svg"), transparent=True)
plt.close(fig)
print("wrote docs/assets/speed-vs-codecs.svg")
