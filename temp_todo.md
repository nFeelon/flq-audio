flq-audio — whole-repo audit (v4)

Status: all Must/Should fixed, most Nice-to-have done. cargo test --release 56/56 green, clippy clean, encode→decode smoke test passes (round-trip verified, .part renamed). Bitstream unchanged — no encoder-output changes; v1_vectors_conform still green.

Legend: [x] corrected · [-] deferred · [~] partial

What this repo does: flq is a lossless audio codec + CLI: it packs WAV/AIFF/FLAC/ALAC into a framed .flq archive (per-frame CRC + index + seek), decodes back bit-exact, and ships a WASM decoder for the web player. Assumed load: single-user CLI on local files, plus the browser player decoding whatever .flq someone drops on it — so crafted-input decode paths count as attack surface.

Must fix

[x] 1. Crafted rANS partition can hang the decoder for hours (bits.rs:475-488)
Fixed: cumulative 0-bit budget `cells.len() << 14` per partition (bits.rs read_residual Rans branch). Legit streams need ≤ ~11372·ncells zeros; budget has ~40% headroom. Spec §10.6 updated to document the bound.

[x] 2. ALAC .m4a with moov past the 64 KiB head read is rejected as lossy (enc.rs, formats.rs)
Fixed: Mp4 entry == [0;4] (undetermined) now falls through to the symphonia probe — only ALAC can decode since that's the only isomp4 codec compiled in; failure → same lossy error. WAV/AIFF: head_scan grows the header buffer (doubling to 64 MiB cap) until the payload chunk is found or the bound is hit.

Should fix

[x] 3. Failed encode leaves partial .flq + blocks retry (main.rs cmd_encode)
Fixed: encode writes {out}.part via create_new, renames on success, removes on error — same pattern as stream_wav_out.

[x] 4. AIFF COMM numSampleFrames ignored on streaming path (enc.rs)
Fixed: `frames = (avail / align).min(h.frames)` — one line, matches aiff::parse.

[x] 5. Warm-up count desync when order > n (codec.rs eval_order)
Fixed: verbatim path now requires warm == o (o <= n); otherwise falls back to wbits=0 ("decode normally"), which is already wire-legal.

[x] 6. Fuzzer can't reach the payload decoder (fuzz/)
Fixed: new target codec_payload.rs calls codec::read_track directly on fuzz bytes (meta from first bytes, frames kept feasible vs payload). bits + codec exposed as #[doc(hidden)] pub in lib.rs; [[bin]] added to fuzz/Cargo.toml. Not yet run (needs nightly + cargo-fuzz).

[x] 7. Unknown flags swallowed as positionals/track names (main.rs)
Fixed: positional() rejects `-`-prefixed args with USAGE; --only loop breaks on any `-`-prefixed arg so unknowns hit the usage arm.

[x] 8. --range accepts NaN (main.rs)
Fixed: requires is_finite() on both bounds.

[x] 9. Stale .part blocks decode forever (main.rs)
Fixed: part_open_err reports "already exists — stale temp file? delete it or choose another output" on AlreadyExists; used by encode + decode paths.

[x] 10. Decode doesn't re-validate track names; stem collisions (main.rs)
Fixed: outs loop now checks v1::valid_name (promoted pub(crate) -> pub) and rejects duplicate output paths before writing anything.

Nice to have

[x] 11. resync byte-wise seeks (dec.rs)
Fixed: windowed scan (1 MiB buffer, header-width overlap); per-offset plausible filtering happens in memory, full-frame CRC read only on candidates.

[-] 12. BitWriter/BitReader bit-at-a-time hot loops (bits.rs)
Deferred: perf hypothesis, needs a benchmark first — not a correctness issue.

[x] 13. Small deletions/dups
Done: find_atom + for_each merged into a single `atoms` iterator (formats.rs); dead `32` arm removed from mem_track (symphonia rejects bits >= 32); duplicated LTP doc paragraph removed (codec.rs); wasm Cargo.toml bumped to edition 2024.

[x] 14. SAFETY markers
Done: // SAFETY: comments added at autocorr_avx2's unsafe block, the GlobalAlloc impl, and the GetProcessTimes call. wasm unsafe fns already carried # Safety docs.

[x] 15. Spec nit: prec > 32 impossible (u4 field, prec <= 16)
Fixed: SPEC-v1.md prec field now states range 1..=16; dead rule removed from the rejection list (the code check stays as a harmless guard — field can't exceed 16 anyway). Wait — the code check `prec > 32` was NOT removed; it can't fire. Spec text corrected.

Verdict: Sound architecture and genuinely strong test coverage. Items 1-2 were the material risks; both now fixed without touching the frozen bitstream.

Lean: ~80 lines deletable realized (atom-walker merge, dead arm, dup doc block), 0 dependencies changed. bits/codec are now doc(hidden)-public — small API-surface cost for payload fuzzing.

Not checked: cargo-fuzz run of the new target, wasm build (build.ps1 not run — edition bump unverified but same rustc requirement), bit-IO throughput (12 deferred), sample_pack corpus sweeps.
