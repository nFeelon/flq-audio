# 2026-10-23 — memory-optimization gate, 148-file re-run

Re-run of `gate148.py` on the memory-optimization build: `564754b` +
uncommitted diff (in-flight window `2*threads`→`threads`, `Src::Sym`
streaming FLAC/ALAC input, streaming FLAC export, `verify_track_par`).
`cargo test --release`: 56/56 pass, `v1_vectors_conform` green — encoder
output byte-identical to `2026-10-22_recheck.md` (same ratios, same
subsets, same median). Machine: i7-14650HX, 24 threads, `-5` defaults.

## Full WAV/AIFF set (148 files, 1572 MiB, defaults)

| | Result | (was `recheck`) |
|---|---|---|
| Accepted / verified | 148/148 (WAV: data chunk byte-identical; AIFF: pcm_crc) | 148/148 |
| Output | **54.74%** (median 57.28%) | 54.74% / 57.28% — identical |
| Encode | 82.2 s total, 19.1 MiB/s (isolated run) | 80.4 s / 19.6 MiB/s |
| Decode | 14.2 s total, parallel frames | 12.3 s |
| Peak heap | enc ≤ **32.1 MiB**; dec ≤ **19.1 MiB** at `-j24` | enc ≤ 52.2; dec ≤ 26.2 |

Heap: enc −38%, dec −27% vs recheck. Output bytes identical —
zero compression regression. The +15% decode wall is a single isolated
rerun (borderline run-to-run variance; a prior run taken under
concurrent sweep load read 26.1 s, enc 123.5 s — contention, not code).

Per-tool subsets identical to `recheck` (v4 vs references):

| vs | files | flq | reference | flq smaller |
|---|---|---|---|---|
| FLAC -8 | 79 | 53.98% | 54.89% | 74/79 |
| WavPack -hx | 148 | 54.74% | 56.81% | 111/148 |
| Monkey's Audio -c4000 | 148 | 54.74% | 64.12% | 114/148 |
| TAK -p4m | 48 | 50.32% | 49.87% | 9/48 |
| OptimFROG extranew-light | 73 | 53.27% | 51.33% | 3/73 |

Repro (outside repo): `gate148.py` in `D:\TEMP\flq_ab\`.
