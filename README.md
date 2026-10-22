![FLQ — lossless audio format](docs/assets/banner.png)

# FLQ — Filon Lossless Quality

[![ci](https://github.com/nFeelon/flq-audio/actions/workflows/ci.yml/badge.svg)](https://github.com/nFeelon/flq-audio/actions/workflows/ci.yml)
![license](https://img.shields.io/badge/license-MIT-blue.svg)
![status](https://img.shields.io/badge/status-experimental-orange.svg)
![rust](https://img.shields.io/badge/rust-edition_2024-red.svg)

FLQ is a lossless audio format for multi-track material: up to 255 tracks
(e.g. stems) coded as independent, CRC-checked frames of 16384 samples.
Every frame decodes on its own, so a player can jump anywhere, extract a
single stem, or keep going past a damaged frame — and a decoder needs
O(frame) memory. Inputs are WAV, AIFF/AIFF-C, FLAC and ALAC; samples are
kept exactly, and FLAC/ALAC tag blocks ride along. The bitstream is
specified in [`docs/SPEC-v1.md`](docs/SPEC-v1.md) with conformance vectors
in [`testdata/v1/`](testdata/v1/).

**Benchmark headline.** On a common 47-file subset accepted by every
compared tool, `flq encode` produced 50.3% of the source size, compared
with 51.1% for FLAC -8 and 52.0% for WavPack -hx — smaller files than
FLAC on 46/47 inputs and than WavPack on 35/47, and ahead of Monkey's
Audio (50.85%) on aggregate.

The deeper analysis chain (high-order LPC, LTP, LMS, rANS) spends CPU
on encode — ~100× realtime on 24 threads (~10× single-threaded) for a
10-minute WAV. Decode is light: ~290× realtime at one thread, faster
still with parallel frames.

[Full comparison and methodology](#vs-reference-codecs).

## Quick start

```console
cargo run --release -- encode drums.wav pad.flac stems.flq            # encode
cargo run --release -- decode stems.flq -o out/                       # all tracks
cargo run --release -- decode stems.flq --only pad.flac --range 30:45 # one stem, 15 s
cargo run --release -- info stems.flq
cargo run --release -- bench drums.wav pad.flac                       # encode+decode in memory
```

Requires Rust 1.85+ (edition 2024). The release binary lands at
`target/release/flq` — it is not added to PATH. Prebuilt binaries are
attached to [GitHub releases](https://github.com/nFeelon/flq-audio/releases).
Try it in the browser: [`web/`](web/) is a WASM player (mute/solo per
stem, seek, WAV export) — see [`web/README.md`](web/README.md). The
bundled 8-track **stems** demo is live at
[nfeelon.github.io/flq-audio](https://nfeelon.github.io/flq-audio/):

[![FLQ web player: stems.flq, 8 tracks in the Layers view](docs/assets/webplayer.png)](https://nfeelon.github.io/flq-audio/)

Options. `encode`: `-0`/`-5`/`-8` (LPC search depth; default `-5`, within
0.01 pp of exhaustive `-8`), `-j N` (threads; default all cores),
`--frame N` (frame length, multiple of 4096), `--no-index`. `decode`:
`-j N` (threads; default all cores), `--only name…`, `--range a:b`
(seconds), `--format wav|flac`
(default: the source's family). `bench` measures encode+decode in memory.
Every command refuses to overwrite existing files.

## Measured results

Dated reports: [`testdata/results/`](testdata/results/). Corpus: a
Freesound-derived sample pack (not redistributed).

### 148 WAV/AIFF files, 1572 MiB

| | Result |
|---|---|
| Accepted / verified | 148/148 — decoded WAV data chunks byte-identical (AIFF: pcm_crc) |
| Output | 54.74% of source (median 57.3%) |
| Encode / decode | 19.6 MiB/s on 24 threads (process-level, incl. small files) / 12.3 s total, parallel frames |
| Peak heap | encode ≤ 53 MiB; decode 0.6 MiB at `-j1`, ≤ 27 MiB at `-j24` (48 frames in flight) |
| Seek | 0.1 s anywhere in a 600 s file: 6–20 ms wall, process start included |

Frame-reset cost on the 47-file set vs the largest frame (262144
samples): +0.44 pp at 4096-sample frames, +0.22 pp at 8192, **+0.10 pp
at 16384 (default)**, +0.05 pp at 32768, +0.02 pp at 65536. Details:
[`2026-10-22_recheck.md`](testdata/results/2026-10-22_recheck.md).

Single-file encodes by input format — FLAC sources stay near input
size (already-compressed payload), PCM lands at ~37–78% depending on
content; per-file numbers live in [`testdata/results/`](testdata/results/):

![FLQ size as % of source by format — single-file encode](docs/assets/compression-by-format.svg)

### vs reference codecs

Same corpus, same files, single-file encode, verified decode — the
47-file subset every tool accepted (identical inputs):

![FLQ vs reference codecs — output size on identical inputs](docs/assets/vs-reference-codecs.svg)

FLQ lands at 50.27% — ahead of FLAC, WavPack and Monkey's Audio on
aggregate. Aggregate hides the per-file picture, so here are the
pairwise counts — a "win" means FLQ's file is smaller on that input:

| FLQ vs | wins / losses | median delta |
|---|---|---|
| FLAC | 46 / 1 | +1.08 pp |
| WavPack | 35 / 12 | +1.26 pp |
| Monkey's Audio | 25 / 22 | +0.05 pp |
| TAK | 9 / 38 | −0.50 pp |
| OptimFROG | 1 / 46 | −1.69 pp |
| 7-Zip | 40 / 7 | +12.19 pp |
| Zstd | 43 / 4 | +26.54 pp |

Delta = reference codec size (% of source) − FLQ size (% of source).
Positive values mean FLQ produced smaller files.

FLQ's lead over FLAC and WavPack is consistent across files; Monkey's
Audio is a per-file coin flip with FLQ ahead on aggregate.

### Memory

Peak process memory vs input length — generated i16 stereo 48 kHz
inputs, internal measurement on this machine:

![Peak process memory vs audio length](docs/assets/memory-vs-length.svg)

Flat in input length — the codec streams: ~40 MiB encode, ~28 MiB
decode at 24 threads (parallel frame workers); at `-j1` decode peaks at
0.6 MiB counted heap. Per-file numbers:
[`2026-10-22_memspeed.md`](testdata/results/2026-10-22_memspeed.md)

## What each format becomes

Samples are kept exactly; the input file container is not (a WAV comes
back as a WAV with identical audio bytes, not necessarily identical
headers; a FLAC comes back re-encoded with its tag blocks preserved).

| Input | Stored as | `decode` gives (default) | Guarantee |
|---|---|---|---|
| WAV (int 8/16/24/32, float 32/64, extensible) | framed LPC+LTP/rANS | WAV | sample-exact |
| AIFF / AIFF-C | framed LPC+LTP/rANS | WAV | sample-exact |
| FLAC | framed LPC+LTP/rANS + tag blocks | FLAC (or WAV) | sample-exact + tags |
| ALAC (`.m4a`) | framed LPC+LTP/rANS + tags | FLAC (or WAV) | sample-exact + tags |
| MP3, AAC, Ogg | rejected — flq is lossless | | |

## How it works

```text
header 28 B   "FLQ1" | 1 | flags | ntracks | frame_len | meta_len | index_off | crc
meta          per track: name, origin, rate, channels, mask, format, frames, pcm_crc, tags
frames        t0f0 t1f0 t0f1 t1f1 …   each: "FQ" | track | fmode | e | frame_no | n | len | payload | crc32
index         per track: frame offsets (u64) + crc32          (optional; frames resync without it)
```

A frame payload is the codec applied to `frame_len` samples (default
16384): 4096-sample blocks, per block a stereo mode (L/R, L/S, R/S, M/S,
plus two adaptive-gain modes on a 1/128 grid), per signal LPC up to order
63 with 15-bit coefficients, then optional long-term pitch prediction
(lag ≤ 4095) and an adaptive two-tap sign-sign pass on the residual, and
finally partitioned coding where each partition picks Rice, escape, or an
adaptive rANS quotient coder; float tracks split into an integer
significand stream plus exact tail bits. History resets at every frame
start; the first `order` samples are stored verbatim, which keeps the
frame cost at +0.10 pp over continuous coding. Seeking reads the index, decodes one frame and trims.
Decoding streams frame by frame (0.6 MiB heap for a 77 MB WAV); encoding
streams WAV/AIFF input and codes frames in parallel. There are no
cross-track references anywhere in the codec or the wire format.

## Modules

| Path | Role |
|---|---|
| `src/v1/` | the format: header/meta/frame/index (`mod.rs`), streaming encoder (`enc.rs`), `Reader` with seek, range decode, resync (`dec.rs`) |
| `src/codec.rs` | float split, stereo modes incl. adaptive gain, LPC (presets, warm-up), LTP, LMS pass, block plan/write/read |
| `src/bits.rs` | MSB-first bit IO, partitioned Rice + escape + rANS partitions, CRC32 |
| `src/rc.rs` | adaptive-binary rANS core (u16-cell Giesen-style renormalization) |
| `src/formats.rs` | sniffing, FLAC/ALAC decode, tag blobs, FLAC/WAV output |
| `src/wav.rs` · `src/aiff.rs` | container parsing (whole-file and header-only) and write-back |
| `src/main.rs` | CLI + timing/heap metrics |
| `wasm/` · `web/` | dependency-free WASM decoder exports + browser player |
| `fuzz/` | cargo-fuzz target for the v1 reader |

Bitstream spec: [`docs/SPEC-v1.md`](docs/SPEC-v1.md). Architecture and
contributor notes: [`AGENTS.md`](AGENTS.md).

## Limitations

**Memory.** Decode keeps one frame (< 1 MiB); encode reads WAV/AIFF
frame by frame, but FLAC/ALAC inputs are still decoded whole into
memory first.

**Format coverage.** Integer 8/16/24/32-bit, float 32/64-bit, 1–16
channels; lossy inputs (MP3, AAC, Ogg) are refused — flq is lossless
only. WAV output drops tags; FLAC output can't hold float or 32-bit int;
FLAC/ALAC are re-encoded on output (samples and tags preserved, original
bytes not); ALAC comes out as FLAC; SEEKTABLE/PADDING not preserved.

**Structure.** 255 tracks max per `.flq`. Frames are independent — a
corrupt frame costs at most that frame, and only on its own track.

**Speed.** Encoding spends real CPU on its analysis chain (~100×
realtime at 24 threads, ~10× single-threaded); decoding parallelizes
across frames (`-j`, default all cores) and stays a streaming O(frame)
reader.

**Status.** Experimental tool. The v1.0 bitstream is frozen by the spec
and conformance vectors. Results from one corpus — not yet checked on a
second collection (e.g. music masters).

## Development

```console
cargo test --release                # unit + conformance + mutation tests
cargo test --release -- --ignored   # + vector/demo generators
cargo clippy --release --all-targets
```

CI: GitHub Actions — `ci.yml` (build, tests and clippy on Ubuntu and
Windows; 60 s cargo-fuzz run on nightly), `release.yml` (binaries for
Linux/Windows/macOS on `v*` tags), `pages.yml` (builds the WASM player
and deploys `web/` to GitHub Pages).

- [`AGENTS.md`](AGENTS.md) — invariants, wire format, contributor notes
- [`testdata/`](testdata/) — tracked fixtures, v1 conformance vectors,
  dated measurement reports
- [`docs/make_assets.py`](docs/make_assets.py) — regenerates the banner
  and charts above

## License

[MIT](LICENSE) © 2026 Nikita Filon
