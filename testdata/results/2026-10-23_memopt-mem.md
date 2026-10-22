# 2026-10-23 — memory/speed re-run on memory-optimization build

`mem_bench.py` on the same 73.6 MiB i16 stereo 48 kHz source as
`2026-10-22_memspeed.md`; peak = Windows working-set polling. Build
`564754b` + uncommitted memory-optimization diff; flq at all cores,
references at their bench settings.

| tool | out MiB | %src | enc s | enc MiB | dec s | dec MiB |
|---|---|---|---|---|---|---|
| flq | 46.6 | 63.3% | 4.25 | **32.7** | 0.59 | **21.8** |
| flac -8 -j8 | 46.6 | 63.4% | 0.20 | 21.0 | 0.41 | 4.5 |
| wavpack -hx | 46.4 | 63.1% | 1.37 | 13.8 | 0.37 | 10.5 |
| ofr xnewl | 45.9 | 62.4% | 21.90 | 13.1 | 10.72 | 4.2 |
| tak -p4m | 46.3 | 62.9% | 2.01 | 8.6 | 0.57 | 8.4 |
| ape -c4000 | 46.3 | 62.9% | 0.57 | 22.5 | 0.61 | 13.0 |

vs `memspeed` (pre-v1 build, same file): flq enc 38.5→32.7 MiB (−15%),
dec 4.9→21.8 MiB — the old number was a sequential decoder; the current
parallel decode holds ~`threads`×frame in flight (~17 MiB at `-j24`,
few-MiB class at `-j1`) and is faster: 1.30→0.59 s. Encode wall
1.50→4.25 s is the heavier v1 pipeline (LTP+LMS), not this diff.

Remaining gap: flq enc still ~1.5–3.8× flac/tak — bounded by per-frame
codec scratch (LPC/LMS/rANS working vectors) × window, not buffering;
a scratch-pool reuse would be the next lever. flq dec ~4.8× flac at
`-j24`; `-j1` closes it.

Repro (outside repo): `mem_bench.py` in `D:\TEMP\flq_ab\`.
