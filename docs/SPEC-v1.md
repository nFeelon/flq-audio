# FLQ v1.0 — Framed Lossless Audio — Normative Specification

Version: **v1.0** (frozen). This document is sufficient to write an
independent decoder without access to the reference implementation.
"MUST"/"MUST NOT"/"SHOULD" are used as in RFC 2119.

## 1. Conventions

- All multi-byte integer fields at the **byte layer** are **little-endian**
  (LE). Signed fields are two's complement.
- Frame *payloads* are bitstreams read **MSB-first**: the first bit of the
  stream is bit 7 of payload byte 0. A field of `n` bits is written most
  significant bit first. The stream is padded with zero bits to the next
  byte boundary; trailing pad bits are ignored by the reader (overrun past
  the payload is an error, but unread pad bits are permitted).
- A "signed n-bit field" is an n-bit two's-complement value, MSB first.
  `write_signed(v,n)` emits `v mod 2^n` in n bits.
- **Unary**: `q` is coded as `q` zero bits followed by a single one bit.
  A decoder MUST reject a run of more than 2^32 zero bits ("unary run too
  long") and MUST reject a unary that overruns the payload.
- **Zigzag**: `zz(v) = (v << 1) ^ (v >> 63)` on 64-bit two's complement;
  `unzig(u) = (u >> 1) ^ -(u & 1)`.
- Bit lengths: `bit_len(v)` = position of the most significant 1-bit + 1;
  `bit_len(0) = 0`.

## 2. CRC-32

All CRCs use the reflected CRC-32 (same as zlib/PNG):

| Parameter | Value |
|---|---|
| Polynomial | `0xEDB88320` (reflected form of `0x04C11DB7`) |
| Input reflection | yes (byte processed LSB-first via the standard table algorithm) |
| Initial value | `0xFFFFFFFF` |
| Final XOR | `0xFFFFFFFF` |
| Check value | `crc32("123456789") = 0xCBF43926` |

Equivalent algorithm: `c = 0xFFFFFFFF; for each byte b: c = T[(c ^ b) & 0xFF] ^ (c >> 8); result = c ^ 0xFFFFFFFF`, where `T` is the standard
reflected table (`T[i]` = 8 rounds of `c = (c&1) ? 0xEDB88320 ^ (c>>1) : c>>1`).

### 2.1 Bytes covered by each CRC

| CRC | Covers | Location |
|---|---|---|
| `header_crc` | header bytes 0..24 (all fields before the CRC) | header bytes 24..28 |
| meta CRC | all meta bytes except its own last 4 | last 4 bytes of the meta region |
| frame CRC | the whole frame record from sync through payload (i.e. `FRAME_HDR_LEN + payload_len` bytes) | last 4 bytes of the frame record |
| index CRC | all index bytes except its own last 4 | last 4 bytes of the index |
| `pcm_crc` | the track's canonical PCM bytes (§9), whole track | per-track meta field |

## 3. File layout

```
+----------+------+----------------------+---------+
| header   | meta | frame records ...    | index   |
| 28 bytes |      | (the "frame region") | (opt.)  |
+----------+------+----------------------+---------+
0         28   meta_end             index_off     EOF
```

- The **frame region** is `[meta_end, frames_end)` where
  `frames_end = index_off` when an index is present, else file length.
- With an index, the index is the last structure in the file
  (`frames_end = index_off`); a decoder MUST NOT scan for frames at or
  beyond `index_off`.

## 4. File header — 28 bytes

| Offset | Size | Field | Value / rule |
|---|---|---|---|
| 0 | 4 | magic | `FLQ1` |
| 4 | 1 | version | `1` |
| 5 | 1 | flags | MUST be `0` in v1.0 (see §11.4) |
| 6 | 1 | ntracks | `1..=255` |
| 7 | 1 | reserved | MUST be `0` |
| 8 | 4 | frame_len | multiple of 4096, `4096..=262144` |
| 12 | 4 | meta_len | `≤ 2^26`; `28 + meta_len ≤ file_len` |
| 16 | 8 | index_off | `0` = no index; else `≥ meta_end`, `≤ file_len` |
| 24 | 4 | header_crc | crc32 of bytes 0..24 |

Reject (in this order in the reference implementation): wrong magic
("not an FLQ file"), version ≠ 1 ("unsupported flq version N (expected
1)"), `header_crc` mismatch, nonzero `flags`, nonzero
`reserved`, `frame_len` out of range, `ntracks == 0`,
`28 + meta_len` overflowing or `> file_len`, `index_off` inside
`[0, meta_end)` (nonzero but overlapping header/meta), `meta_len < 4`.

Additional open-time bound: for every track, `nframes` MUST NOT exceed
`frame_span / 22` where `frame_span = min(frames_end, file_len) - meta_end`
(each frame record is at least 18-byte header + 4-byte CRC).

## 5. Metadata

`meta_len` bytes start at offset 28. The last 4 bytes are the CRC32 of the
preceding `meta_len - 4` bytes. The body is `ntracks` consecutive track
metas; it MUST be consumed exactly — trailing bytes are an error.

### 5.1 Track meta (per track, in order)

| Field | Size | Rule |
|---|---|---|
| name_len | u16 | `≤ 4096` |
| name | name_len | valid UTF-8 |
| origin | u8 | `0` WAV, `1` AIFF, `2` FLAC, `3` ALAC; other values rejected |
| rate | u32 | `> 0` |
| channels | u8 | `1..=16` |
| channel_mask | u32 | WAVE `dwChannelMask`; `0` = unspecified. Informative: declares the speaker positions for `channels > 2`; channel order is WAVE order |
| sample_fmt | u8 | `0` = integer, `1` = float |
| bits | u8 | int: `8|16|24|32`; float: `32|64`; other combinations rejected |
| frames | u64 | total per-channel samples |
| nframes | u32 | `0` iff `frames == 0`; else MUST equal `ceil(frames / frame_len)` |
| pcm_crc | u32 | crc32 of canonical PCM (§9) |
| tags_len | u32 | `≤ 2^24` |
| tags | tags_len | FLAC metadata block chain (§5.2); empty for WAV/AIFF origins |

### 5.2 Tags blob

A chain of FLAC-format metadata blocks: `[header 4 B][body]` repeated.
Header byte 0 = `last_flag<<7 | block_type`, bytes 1..3 = big-endian body
length. The blob contains every block from the source file except
STREAMINFO(0), PADDING(1) and SEEKTABLE(3), with the last-block flag
cleared in the blob. A decoder SHOULD treat the blob as opaque; it is not
needed to reconstruct audio.

## 6. Frame records

Back-to-back in the frame region. The encoder writes them ordered by
`frame_no` ascending, then `track` ascending (round-robin across live
tracks); a decoder MUST NOT rely on this ordering — it identifies frames by
`(track, frame_no)` header fields and MAY scan/resynchronize (§11.3).

| Offset | Size | Field | Rule |
|---|---|---|---|
| 0 | 2 | sync | `0x46 0x51` ("FQ") |
| 2 | 1 | track | `< ntracks` |
| 3 | 1 | fmode | `0` int, `1` f32, `2` f64-as-f32, `3` f64; MUST agree with the track's `sample_fmt`/`bits`: int→0, float32→1, float64→2 or 3 |
| 4 | 2 | e | i16; `0` when `fmode == 0`; else the float exponent bias `e_max` (§8.6) |
| 6 | 4 | frame_no | `0..nframes-1` |
| 10 | 4 | n | samples in this frame. MUST equal `min(frame_len, frames − frame_no·frame_len)` and be `> 0` |
| 14 | 4 | payload_len | `FRAME_HDR_LEN + payload_len + 4 ≤ file_len − off` |
| 18 | payload_len | payload | codec bitstream (§8) |
| 18+payload_len | 4 | crc32 | over the `18 + payload_len` preceding bytes |

A frame record is 22 + payload_len bytes. `n` is fully determined by the
track meta — a reader MUST NOT trust the header's `n` field, it MUST match
the computed expectation.

## 7. Index

At `index_off` (when nonzero): for each track in meta order, `nframes`
× u64 absolute byte offsets of that track's frame records, then a u32
CRC32 of all preceding index bytes. Total = `8·Σnframes + 4` bytes; it MUST
fit inside the file (`index_off + size ≤ file_len`). Every offset MUST lie
in `[meta_end, index_off)` with at least `FRAME_HDR_LEN + 4` bytes
remaining before `index_off`.

The index is optional. Index-less operation: frames are found by
sequential scan of the frame region (§11.3); the file is still valid and
decodes identically.

## 8. Frame payload bitstream

The payload is the codec bitstream for the frame's `n` samples, coded
independently per frame: prediction history is zero before the frame's
first sample and carries between blocks *inside* the frame only.

### 8.1 Block partitioning

The frame's `n` samples per channel are split into blocks of 4096; the
last block holds `n mod 4096` (1..4096) samples when nonzero. For each
block, in order:

1. **Stereo mode** — present iff `channels == 2`: 3 bits, value 0..5
   (§8.2); modes 4/5 are followed by an 8-bit signed gain field.
   For `channels != 2` there is no mode field and each channel is an
   independent coded signal.
2. **Coded signals** — 2 signals when `channels == 2`, else `channels`
   signals (§8.3–8.5).
3. **Float tail data** — only for `fmode != 0` (§8.6), per channel per
   sample in block order.

The format has no cross-track references: every block is coded from the
frame's own samples only.

### 8.2 Stereo modes (channels == 2 only)

Let L, R be the block's channel samples; A, B the two coded signals.

| mode | name | forward (encode) | inverse (decode, per sample) |
|---|---|---|---|
| 0 | LR | A=L, B=R | L=A, R=B |
| 1 | LS | A=L, B=L−R | L=A, R=A−B |
| 2 | RS | A=R, B=L−R | L=A+B, R=A |
| 3 | MS | A=mid=(L+R)>>1 (arithmetic), B=L−R | `m = 2A | (B & 1)`; L=(m+B)>>1, R=(m−B)>>1 |
| 4 | LG | A=L, B=R−gain(L) | L=A, R=B+gain(A) |
| 5 | RG | A=R, B=L−gain(R) | R=A, L=B+gain(A) |

Modes 6–7 are reserved; a reader MUST reject them ("stereo mode N out of
range").

For modes 4/5 the mode field is followed immediately by a signed 8-bit
**gain** field `g`. The gain operator is a quantized multiply on a 1/128
grid:

```
gain(v) = (g·v + 64) >> 7        // wrapping i64 multiply, arithmetic shift
```

All arithmetic is wrapping 64-bit; `>>1` is an arithmetic (sign-extending)
shift. The mode-3 inverse restores the lost low bit via `B & 1`, which is
exact: `L+R` and `L−R` always have the same parity.

Note the mode also applies to the *history* transform: the coded signals'
prediction history is the stereo transform of the reconstructed
y-history (the previous `MAX_ORDER` = 63 samples per channel,
zero-padded before the frame start).

### 8.3 Coded signal header

Per coded signal, in signal order:

| Field | Bits | Rule |
|---|---|---|
| order | 6 | LPC order `o`, `0..=63`; `> 63` impossible (field width) |
| prec−1 | 4 | only when `o > 0`; coefficient precision `prec = field + 1`. Reference encoder emits 14 (prec 15). `prec > 32` rejected; coefficients are read as signed `prec`-bit values and kept mod 2^16 (i16) |
| qshift | 5 | only when `o > 0`; prediction right-shift |
| coefficients | `o` × `prec` | only when `o > 0`; signed `prec`-bit fields, order k = 1..o |
| wbits | 6 | only in **block 0** of a frame **and** `o > 0`; see §8.4 |
| warm-up values | `o` × `wbits` | only when `wbits > 0`; signed `wbits`-bit fields |
| ltp_flag | 1 | always present; `1` = LTP fields follow (§8.5a) |
| ltp τ | 12 | only when `ltp_flag`; MUST be `> 0` |
| ltp gain | 8 signed | only when `ltp_flag`; gain on the 1/128 grid |
| lms_flag | 1 | always present; `1` = adaptive pass applied (§8.5b) |
| lms step | 2 | only when `lms_flag`; index into `[2, 4, 8, 16]` |

`order == 0` means "no prediction": the signal equals its residuals.

### 8.4 Warm-up fields (block 0 of every frame)

Because history is zero at a frame start, the first `o` samples of block 0
cannot be predicted. When `o > 0` in block 0:

- `wbits` (6 bits) is the minimal signed field width holding all `o`
  warm-up values (a signed value v needs `bit_len(v)+1` bits for `v ≥ 0`,
  `bit_len(~v)+1` for `v < 0`; minimal width ≥ 1).
- `wbits == 0`: no warm-up fields follow — the positions are decoded by
  ordinary prediction against the zero history (encoder fallback for
  values needing 64-bit width, which does not fit the 6-bit field).
- `wbits > 0`: `o` signed `wbits`-bit values follow; these ARE the first
  `o` samples of the coded signal (verbatim).
- At warm-up positions the residual stream MUST contain `0` placeholders;
  a nonzero residual at position `i < o` in block 0 is an error
  ("warm-up residual must be 0").

A decoder of a conforming v1.0 file will only ever see this in block 0 of a
frame — frames always reset history, so the condition is `(block == 0 &&
order > 0)`.

### 8.5 Prediction & residuals

The signal header fields are followed by the coded residual stream `R`
of length `n` (the block length). Decoding pipeline — each stage feeds
the next:

```
R  =  partitioned read (§8.5c)
e2 =  lms_flag ? LMS⁻¹(R)      : R            // §8.5b
e1 =  ltp_flag ? LTP⁻¹(e2, τ, g) : e2         // §8.5a
x_i =  e1_i + pred_i                            // LPC add-back below
```

For sample position `i` in the block (beyond any warm-up positions), let
`aw` be the reconstructed coded signal with its 63-sample history prefix
(history = the coded-domain previous samples; zeros before the frame
start; at block boundaries inside the frame it is the already-decoded
tail of the previous block, transformed to coded domain):

```
pred = ( Σ_{k=1..o} c[k] · aw[i−k] ) >> qshift     // wrapping i64 sum, arithmetic shift
x_i  = e1_i + pred                                // wrapping i64 add
```

Residuals `i < o` in block 0 are the zero placeholders of §8.4 — that
check happens on `e1` (after LMS/LTP inversion). The signal's
reconstruction for the whole block feeds the next block's history inside
the same frame.

#### 8.5a LTP — long-term prediction (ltp_flag)

The wire residual is the *post-LTP* stream: at encode, `e2[i] =
e1[i] − gain(e1'[base+i−τ])` where `gain(v) = (g·v + 64) >> 7` and
`e1'` is the reconstructed residual stream of the whole frame up to that
point (per coded signal, positions before frame start read as 0). The
decoder rebuilds in place, forward order:

```
e1[i] = e2[i] + gain(tap),  tap = e1[base+i−τ]   // 0 when index < 0
```

`base` is the frame-absolute position of the block start. Because the
reconstruction is forward-sequential, taps inside the same block
(`i−τ ≥ base`) read already-rebuilt values. τ ∈ `1..=4095` (u12 field);
τ = 0 is rejected ("LTP lag out of range"). Note the LTP history is the
*residual* stream — it is not affected by LMS, which sits one stage later
and is inverted first.

#### 8.5b LMS — adaptive last-mile pass (lms_flag)

When `lms_flag` is set, the wire stream was produced by a two-tap
sign-sign adaptive decorrelator; the decoder replays it exactly:

```
w1 = w2 = 0  (i32-range accumulators, clamped to ±4096)
step = LMS_STEPS[lms_step] = [2, 4, 8, 16][idx]
for i in 0..n:
    s1 = R'[i−1] or 0,  s2 = R'[i−2] or 0        // previous *output* values
    p  = (w1·s1 + w2·s2 + 512) >> 10             // wrapping i64
    R'[i] = R[i] + p                              // output = wire + prediction
    if R[i] != 0:                                 // adapt on the wire value's sign
        ds = R[i] > 0 ? +step : −step
        if s1 != 0: w1 += (s1 > 0 ? +ds : −ds); clamp ±4096
        if s2 != 0: w2 += (s2 > 0 ? +ds : −ds); clamp ±4096
```

The encoder's forward pass is `R[i] = s[i] − p` with the same update —
`R'` after inversion equals `s`. Weights reset at every signal start
(every block).

#### 8.5c Residual stream coding

| Field | Bits | Rule |
|---|---|---|
| partition order `p` | 4 | `0..=8`; `2^p` MUST divide `n` |
| per partition j (`2^p` partitions of `n_p = n >> p` residuals, in order): | | |
| param | 6 | `0..=30` = Rice parameter `k`; `31..=61` = rANS partition with `k = param − 31`; `62` = escape; `63` reserved, rejected |
| if Rice `k`: per residual | `unary(u>>k)` + `k` low bits of `u`, where `u = zigzag(residual)` | |
| if rANS `k`: blob_len | 16 | number of u16 cells following |
| if rANS `k`: cells | `16 × blob_len` | rANS state cells, payload order |
| if rANS `k`: per residual | `k` low bits of `zigzag(residual)`, in order | |
| if escape: width | 6 | `width ≤ 63`; `width == 0` means all `n_p` residuals are `0` |
| if escape: per residual | `width` raw bits = low `width` bits of `zigzag(residual)` | |

**rANS partition coding** (param `31..=61`, `k = param − 31`): the
quotient sequence `q_i = zigzag(residual_i) >> k` is coded as adaptive
unary — for each residual, `q` zero bits then a one bit — where every bit
position uses its own adaptive binary context indexed by the run position
`j` capped at 63 (context for bit j of the unary expansion is
`ctx[min(j, 63)]`). Each context holds a 12-bit probability `p1` of a
1-bit, initialized to `2048` and updated after every coded bit `b`:

```
p1 += ((b ? 4096 : 0) − p1) >> 4;   clamp to [4, 4092]
```

The bit sequence is coded with a Giesen-style rANS over 16-bit state
cells (`TOT = 4096`, lower bound `L = 2^24`): the encoder first walks the
bits forward to record each bit's probability, then encodes them in
reverse order; the decoder reads forward. Encoding a bit with frequency
`f` and cumulant `cum` (for bit `b`: `f = b ? p1 : TOT−p1`,
`cum = b ? TOT−p1 : 0`):

```
emit:  while x >= f << 28:  append low u16 of x;  x >>= 16
fold:  x = (x / f) · TOT + cum + (x mod f)
```

The encoder finishes by appending 3 u16 cells of the final state,
least-significant cell first. The decoder initializes `x` from those 3
cells, consumes cells in reverse payload order (last-emitted-first), and
after each symbol pulls cells while `x < 2^24`:

```
c = x mod 4096;  bit = (c >= TOT − p1)
x = f · (x >> 12) + c − cum
renorm: while x < 2^24 and cells remain: x = (x << 16) | next_cell
```

Bounds: `blob_len` counts u16 cells (allocation ≤ 128 KiB); a blob of
fewer than 3 cells is an error ("rANS blob too short"); a unary run of
more than 2^26 zeros is an error ("unary run too long") — the reference
encoder never emits a quotient above 2^26 in an rANS partition.

An escape with `width > 63` is an error; param `63` is an error
("partition param out of range"); any read past the payload is an error
("truncated payload").

### 8.6 Float streams (fmode 1/2/3)

A float track is coded as an integer **significand stream** `I` (through
all of §8.1–8.5, stereo mode included) plus per-sample **tail data**
written after the signals of each block, channel-major then sample order.

`e` (frame header) = `e_max` = the maximum unbiased exponent over the
frame's finite nonzero samples: `e_max = max(ef − bias)`, where `ef` is the
raw exponent field; `0` if none. `mw` = mantissa width: 23 for fmode 1/2,
52 for fmode 3. `bias` = 127 / 1023; `expw` = 8 / 11; `emask = 2^expw − 1`;
`mmask = 2^mw − 1`. The stream word width `W` = 32 for fmode 1/2, 64 for
fmode 3.

Per sample `i`, given decoded stream value `I_i`:

| `I_i` | tail bits | meaning |
|---|---|---|
| `0` | 1 bit `b` | `b = 0` → word = `+0.0` (0); `b = 1` → escape: next `W` bits are the raw stream word (verbatim IEEE bits) |
| `≠ 0` | `s = mw − (bit_len(|I_i|) − 1)` bits | low `s` mantissa bits `db`; recombine per below. `s < 0` is an error ("mantissa magnitude out of range") |

Recombination for `I_i ≠ 0`:

```
s   = mw − (bit_len(|I_i|) − 1)          // must be ≥ 0
e_i = e_max − (s − (mw − 23))            // unbiased exponent
ef  = e_i + bias                          // must satisfy 0 < ef < emask, else error
m   = ((|I_i| << s) | db) & mmask         // mantissa (implicit top bit dropped)
word = (sign(I_i) << (mw + expw)) | (ef << mw) | m     // sign = 1 iff I_i < 0
```

Inversion check: `bit_len(|I_i|) ≤ mw + 1` is enforced ("mantissa
magnitude out of range").

- **fmode 1** (f32): `mw=23`, `W=32`, `pcm_bits=32`; word = f32 bits.
- **fmode 2** (f64-as-f32): `mw=23`, `W=32`; decode the f32 `word`, then
  widen to f64: `output = (word as f32) as f64`, i.e. the PCM word is the
  f64 reinterpretation of the exact f32 value. Used when every f64 sample
  in the frame is exactly representable as f32.
- **fmode 3** (true f64): `mw=52`, `W=64`; word = f64 bits.

The escape path (zero stream word, 1-bit flag + raw `W`-bit word) carries
exact values that fall outside the significand model: ±0.0 distinctions
(`+0.0` uses the `b=0` path; `-0.0` the escape), denormals, infinities,
NaNs — any bit pattern.

## 9. Canonical PCM and `pcm_crc`

`pcm_crc` is the CRC32 of the track's **canonical interleaved PCM** — the
WAVE `data`-chunk convention, little-endian, channel-interleaved
(sample-major, channel-minor), whole track:

| Format | Canonical bytes per sample |
|---|---|
| int 8 | one unsigned byte = `sample + 128` |
| int 16 | i16 LE |
| int 24 | low 3 bytes of the i32 LE (two's complement, sign preserved) |
| int 32 | i32 LE |
| float 32 | f32 bits, 4 B LE |
| float 64 | f64 bits, 8 B LE |

Decoded integer samples MUST lie in `−2^(bits−1) .. 2^(bits−1)−1`;
a reader MUST reject an out-of-range sample ("sample out of range for
N-bit"). This is the WAV-centered form: an 8-bit sample `u` on the wire
corresponds to `u − 128` in the decoded signal.

## 10. Reader validation summary

A conforming decoder MUST reject on:

| # | Condition |
|---|---|
| 1 | magic ≠ `FLQ1`, version ≠ 1, `header_crc` mismatch, `flags ≠ 0`, `reserved ≠ 0`, `ntracks == 0`, `frame_len` not a multiple of 4096 in `4096..=262144` |
| 2 | `meta_len > 2^26`, `28 + meta_len > file_len`, `meta_len < 4`, meta CRC mismatch, meta body not consumed exactly |
| 3 | per-meta: `name_len > 4096`, name not UTF-8, `origin > 3`, `rate == 0`, `channels` outside `1..=16`, invalid `sample_fmt`/`bits` combination, `tags_len > 2^24`, `nframes == 0` XOR `frames == 0`, `nframes ≠ ceil(frames/frame_len)`, `nframes > frame_span/22` |
| 4 | `index_off` nonzero but `< meta_end`; index size `> file_len − index_off`; index CRC mismatch; an index offset outside `[meta_end, index_off − 22]` |
| 5 | per frame: bad sync, `track ≥ ntracks`, `frame_no ≥ nframes` (implied by `n` check), `n` ≠ `min(frame_len, frames − frame_no·frame_len)`, `fmode` inconsistent with `sample_fmt`/`bits`, `e ≠ 0` on `fmode 0`, record extending past file end, frame CRC mismatch |
| 6 | payload: any read past the payload ("truncated payload"/"bitstream overrun"), stereo `mode > 5`, `order > 63` (impossible — u6 field), `prec > 32`, LTP `τ == 0`, warm-up residual nonzero, partition `p > 8` or `2^p ∤ n`, param `63`, escape `width > 63`, rANS blob < 3 cells, rANS unary run > 2^26, Rice unary run > 2^32, `bit_len(|I|) > mw + 1`, float exponent `ef` outside `(0, emask)` |
| 7 | decoded int sample outside the declared bit range |
| 8 | `decode_track`: `pcm_crc` mismatch |

All errors SHOULD name the track and frame where applicable
(`"track T frame N: …"`).

## 11. Informative notes

### 11.1 Encoder freedom

A conforming encoder may choose: `frame_len` (any valid value),
compression effort (the LPC order search preset is an encoder-time
decision only — the bitstream is self-describing), and thread count
(output MUST be identical for any thread count). A decoder MUST accept
any valid `frame_len` and MUST NOT assume the encoder's choices. The
reference encoder's defaults: `frame_len = 16384`, index on.

### 11.2 Streaming decode

The format is designed for O(frame) memory: read the index, then for each
wanted frame read `18 + payload_len + 4` bytes, verify CRC, decode the
payload into `n`-sample buffers, emit the wanted slice. `pcm_crc` can be
verified incrementally over emitted canonical PCM.

### 11.3 Resynchronization

After a corrupt or unparseable frame, a decoder MAY resynchronize by
scanning forward byte-by-byte within `[meta_end, frames_end)` for a
plausible frame: correct sync, plausible header fields per §10.5, and a
passing frame CRC. A sequential scan MUST stop at `frames_end` (an
indexed file's index bytes are not frame data). Lost frames of a track
report as "lost (corrupt or truncated)"; frames of other tracks are
unaffected — corruption is frame-local.

### 11.4 Reserved flag bit 0

Header `flags` bit 0 is reserved for a future lossy-storage capability.
v1.0 decoders MUST reject any nonzero `flags` byte ("unsupported flq
flags"). `reserved` (byte 7) likewise MUST be 0 and rejected otherwise.

### 11.5 Origins and channel order

`origin` records the source container for default re-export (WAV/AIFF →
WAV; FLAC/ALAC → FLAC); it does not change decoding. Channels are always
stored and reconstructed in WAVE channel order (L, R, C, LFE, …);
`channel_mask` (WAVE `dwChannelMask`, 0 = unspecified) is informative
metadata carried to WAVE_FORMAT_EXTENSIBLE output.
