//! FLQ v3 container: header, (optionally deflated) meta section, then
//! per-track payloads in track order. Kind byte: 0 = WAV, 1 = MP3, 2 = AIFF,
//! 3 = PCM from FLAC/ALAC, 4 = raw stored file. Version 2: metas inline,
//! shared PCM bitstream, then u64-length-prefixed MP3/raw payloads. Version 1
//! has no kind byte (all tracks kind 0). v1/v2 files decode as before.
//!
//! v3 layout:
//! ```text
//! "FLQ1" | 3 | ntracks u8 | block u32
//! meta_mode u8 (0 stored, 1 deflate) | meta_raw_len u32 | meta_len u32 | meta
//! per-track payloads concatenated in track order (lengths live in metas)
//! ```
//! v3 per-track meta = the v2 fields, except the joint flag byte becomes
//! `self_ref u8 | nref u8 | refs[nref] u8` (earlier PCM-coded track indices,
//! strictly increasing) and every meta ends with `payload_len u64`. Each
//! PCM-coded track has its own byte-aligned bitstream payload.

use crate::aiff;
use crate::bits::{BitReader, BitWriter, crc32};
use crate::codec::{self, DBit, FloatData, TrackCtx};
use crate::formats::{self, Sniff};
use crate::joint;
use crate::mp3::{self, Mp3Stats};
use crate::wav;

pub const BLOCK: usize = codec::BLOCK;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrackKind {
    Wav,
    Mp3,
    Aiff,
    Pcm,
    Raw,
}

/// Output format for PCM-kind (FLAC/ALAC) tracks on decode.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PcmOut {
    /// FLAC: the lossless output for both FLAC and ALAC sources.
    #[default]
    Flac,
    Wav,
}

pub struct Input {
    pub name: String,
    pub bytes: Vec<u8>,
}

pub struct Output {
    pub name: String,
    pub bytes: Vec<u8>,
    /// `false` for PCM-kind tracks (audio-exact, file re-encoded).
    pub exact: bool,
}

pub struct TrackStats {
    pub name: String,
    pub kind: TrackKind,
    /// Input format label ("wav", "aiff", "flac", "alac", "mp3", "aac", ...).
    pub format: String,
    pub input_bytes: u64,
    pub float_mode: u8, // 0 int, 1 f32, 2 f64 stored as f32, 3 f64
    pub bits: u16,
    pub channels: u16,
    pub rate: u32,
    pub frames: u64,
    pub meta_bytes: u64,
    pub independent_bits: u64,
    pub stereo_bits: u64,
    pub final_bits: u64,
    pub float_bits: u64,
    pub joint: bool,
    pub joint_reason: String,
    pub max_ncc: Option<f64>,
    /// Blocks where >= 1 channel referenced the track's own earlier material.
    pub self_ref_blocks: u64,
    /// Blocks where >= 1 channel referenced an earlier track.
    pub cross_ref_blocks: u64,
    /// Earlier track indices kept in this track's reference pool.
    pub refs: Vec<usize>,
    pub blocks: u64,
    pub mp3: Option<Mp3Stats>,
}

pub struct Stats {
    pub tracks: Vec<TrackStats>,
    pub total_bytes: u64,
    /// Rough peak-heap estimate of the encode, in bytes; drives the CLI
    /// memory warning (see `est_encode_heap`).
    pub est_encode_heap: u64,
}

/// Rough peak-heap estimate for an encode: at each planning step every
/// still-referenced stream is resident (per `last_use`), plus ~1.5x the
/// current track's own streams on top for plan/residual copies (~2.5x
/// total), plus the bounded joint-analysis working set
/// (~3 * MAX_ANALYSIS * 16 B).
fn est_encode_heap(xbytes: &[u64], last_use: &[usize]) -> u64 {
    const ANALYSIS_WS: u64 = 3 * (1 << 21) * 16;
    let mut peak = 0u64;
    for (k, &own) in xbytes.iter().enumerate() {
        let alive: u64 = xbytes
            .iter()
            .enumerate()
            .filter(|&(j, _)| last_use[j] >= k)
            .map(|(_, &b)| b)
            .sum();
        peak = peak.max(alive + own * 3 / 2);
    }
    peak + ANALYSIS_WS
}

/// Track names become output filenames on decode: reject path pieces,
/// `:` (drive letters/ADS), trailing space/dot (Windows trims them), and
/// Windows device names — with or without an extension (`con`, `con.wav`).
fn valid_name(n: &str) -> bool {
    if n.is_empty()
        || n == "."
        || n == ".."
        || n.ends_with([' ', '.'])
        || n.contains(['/', '\\', ':'])
    {
        return false;
    }
    let stem = n.split('.').next().unwrap_or("");
    !matches!(
        stem.to_ascii_uppercase().as_str(),
        "NUL" | "CON" | "PRN" | "AUX"
            | "COM1" | "COM2" | "COM3" | "COM4" | "COM5"
            | "COM6" | "COM7" | "COM8" | "COM9"
            | "LPT1" | "LPT2" | "LPT3" | "LPT4" | "LPT5"
            | "LPT6" | "LPT7" | "LPT8" | "LPT9"
    )
}

fn mono(x: &[Vec<i64>]) -> Vec<f64> {
    let frames = x.first().map_or(0, |c| c.len());
    let ch = x.len() as f64;
    (0..frames)
        .map(|i| x.iter().map(|c| c[i] as f64).sum::<f64>() / ch)
        .collect()
}

/// A PCM-coded track (kinds 0 WAV, 2 AIFF, 3 FLAC/ALAC PCM).
struct Track {
    name: String,
    kind: TrackKind, // Wav | Aiff | Pcm
    format: &'static str,
    // kinds Wav/Aiff: exact rebuild
    prefix: Vec<u8>,
    suffix: Vec<u8>,
    crc: u32,
    // kind Pcm: generated output
    src: u8, // 0 = flac, 1 = alac
    tags: Vec<u8>,
    pcm_crc: u32,
    input_bytes: u64,
    channels: u16,
    rate: u32,
    bits: u16,
    frames: u64,
    float_mode: u8,
    e: i16,
    x: Vec<Vec<i64>>,
    d: Option<Vec<Vec<DBit>>>,
    stream_word_bits: u32,
}

/// Codec streams split from a WAV-convention `Wav` (float split or ints).
struct CodecStreams {
    float_mode: u8,
    e: i16,
    x: Vec<Vec<i64>>,
    d: Option<Vec<Vec<DBit>>>,
    word_bits: u32,
}

fn streams(w: &wav::Wav) -> CodecStreams {
    let (float_mode, e, x, d, word_bits) = if w.tag == 3 {
        let words = wav::channel_words(w);
        if w.bits == 64 {
            let exact = words.iter().flatten().all(|&u| {
                let f = f64::from_bits(u);
                f.is_finite() && ((f as f32) as f64).to_bits() == u
            });
            if exact {
                let w32: Vec<Vec<u64>> = words
                    .iter()
                    .map(|ch| {
                        ch.iter()
                            .map(|&u| u64::from((f64::from_bits(u) as f32).to_bits()))
                            .collect()
                    })
                    .collect();
                let FloatData { e, streams, d } = codec::split_float(&w32, 23);
                (2u8, e, streams, Some(d), 32)
            } else {
                let FloatData { e, streams, d } = codec::split_float(&words, 52);
                (3u8, e, streams, Some(d), 64)
            }
        } else {
            let FloatData { e, streams, d } = codec::split_float(&words, 23);
            (1u8, e, streams, Some(d), 32)
        }
    } else {
        (0u8, 0i16, wav::channel_ints(w), None, 0)
    };
    CodecStreams { float_mode, e, x, d, word_bits }
}

fn to_track(inp: &Input, w: wav::Wav, kind: TrackKind, format: &'static str) -> Track {
    let crc = crc32(&inp.bytes);
    let s = streams(&w);
    Track {
        name: inp.name.clone(),
        kind,
        format,
        prefix: w.prefix,
        suffix: w.suffix,
        crc,
        src: 0,
        tags: Vec::new(),
        pcm_crc: 0,
        input_bytes: inp.bytes.len() as u64,
        channels: w.channels,
        rate: w.rate,
        bits: w.bits,
        frames: w.frames,
        float_mode: s.float_mode,
        e: s.e,
        x: s.x,
        d: s.d,
        stream_word_bits: s.word_bits,
    }
}
struct Mp3Track {
    name: String,
    bytes: Vec<u8>,   // original file (length bookkeeping)
    payload: Vec<u8>, // bytes stored in the container
    mode: u8,         // 0 raw, 1 modeled
    crc: u32,
    mp3: Mp3Stats,
    reason: String,
}

struct RawTrack {
    name: String,
    format: String, // label for stats/output bookkeeping
    bytes: Vec<u8>,
    crc: u32,
    reason: String,
}

enum TrackData {
    Pcm(Track),
    Mp3(Box<Mp3Track>),
    Raw(RawTrack),
}

const MP3_REASON: &str = "mp3 track (no joint coding)";
const RAW_REASON: &str = "stored as-is (lossy format)";

/// kind-3 metadata byte count (kind + name + fixed fields + tags).
/// Fixed part: kind1 namelen2 src1 ch2 rate4 bits2 frames8 crc4 tagslen4
/// selfref1 nref1 plen8 = 38 (the refs themselves are added by the caller).
fn pcm_meta_len(name: &str, tags_len: usize) -> u64 {
    38 + name.len() as u64 + tags_len as u64
}

fn raw_track(inp: &Input, format: &str, reason: &str) -> TrackData {
    TrackData::Raw(RawTrack {
        name: inp.name.clone(),
        format: format.to_string(),
        bytes: inp.bytes.clone(),
        crc: crc32(&inp.bytes),
        reason: reason.to_string(),
    })
}

/// FLAC/ALAC source -> PCM track candidate (kind 3; kept only if it beats the
/// original file size, decided later).
fn pcm_track(
    inp: &Input,
    s: formats::PcmSource,
    src: u8,
    format: &'static str,
) -> Track {
    let frames = s.x.first().map_or(0, |c| c.len());
    let pcm = wav::ints_to_pcm(&s.x, s.bits, frames);
    Track {
        name: inp.name.clone(),
        kind: TrackKind::Pcm,
        format,
        prefix: Vec::new(),
        suffix: Vec::new(),
        crc: 0,
        src,
        tags: s.tags,
        pcm_crc: crc32(&pcm),
        input_bytes: inp.bytes.len() as u64,
        channels: s.channels,
        rate: s.rate,
        bits: s.bits,
        frames: frames as u64,
        float_mode: 0,
        e: 0,
        x: s.x,
        d: None,
        stream_word_bits: 0,
    }
}

/// `carry` is the model state a decoder holds after the last modeled MP3
/// track: a mode-1 track replaces it with its fresh-model post-state, a
/// mode-2 track advances it in place, raw tracks leave it alone — so a
/// chain spans non-MP3 and raw tracks transparently.
fn mp3_track(inp: &Input, carry: &mut Option<mp3::model::M>) -> TrackData {
    let parsed = mp3::parse(&inp.bytes);
    let rebuilds = mp3::rebuild(&parsed) == inp.bytes;
    let reason = if rebuilds { MP3_REASON } else { "parser mismatch" };
    // stage 2: try modeled recompression fresh (mode 1) and, when a model
    // is already carried, chained into a clone of it (mode 2). Keep the
    // smallest payload that verifies byte-exact; raw (mode 0) is the
    // fallback when the parse was imperfect or nothing beats storing.
    let mut cand: Option<(u8, Vec<u8>, [f64; mp3::model::NCAT], mp3::model::M)> = None;
    if rebuilds {
        let mut mf = mp3::model::carried();
        let (c, costs) = mp3::model::compress_into(&parsed, &mut mf);
        if c.len() < inp.bytes.len()
            && mp3::model::decompress(&c, inp.bytes.len() as u64)
                .is_ok_and(|d| mp3::rebuild(&d) == inp.bytes)
        {
            cand = Some((1, c, costs, mf));
        }
        if let Some(cm) = carry.as_ref() {
            let mut mc = cm.clone();
            let (c, costs) = mp3::model::compress_into(&parsed, &mut mc);
            if c.len() < cand.as_ref().map_or(inp.bytes.len(), |c| c.1.len()) {
                // verify against a cloned carry: the decoder will decode
                // this payload into the same pre-state
                let mut chk = cm.clone();
                if mp3::model::decompress_with(&c, inp.bytes.len() as u64, &mut chk)
                    .is_ok_and(|d| mp3::rebuild(&d) == inp.bytes)
                {
                    cand = Some((2, c, costs, mc));
                }
            }
        }
    }
    let (mode, payload, coded) = match cand {
        Some((mode, payload, coded, post)) => {
            *carry = Some(post);
            (mode, payload, coded)
        }
        None => (0, inp.bytes.clone(), [0.0; mp3::model::NCAT]),
    };
    let mut st = mp3::stats(&parsed);
    st.mode = mode;
    st.payload_bytes = payload.len() as u64;
    st.coded = coded;
    TrackData::Mp3(Box::new(Mp3Track {
        name: inp.name.clone(),
        bytes: inp.bytes.clone(),
        payload,
        mode,
        crc: crc32(&inp.bytes),
        mp3: st,
        reason: reason.to_string(),
    }))
}

pub fn encode(inputs: &[Input]) -> Result<(Vec<u8>, Stats), String> {
    encode_opts(inputs, true)
}

/// `joint = false` skips reference analysis entirely: PCM tracks are coded
/// with LPC/Rice only (no self or cross references).
pub fn encode_opts(inputs: &[Input], joint: bool) -> Result<(Vec<u8>, Stats), String> {
    if inputs.is_empty() || inputs.len() > 255 {
        return Err("expected 1..=255 inputs".into());
    }
    let n = inputs.len();
    let mut tracks: Vec<TrackData> = Vec::with_capacity(n);
    // shared model carried across MP3 tracks (container mode 2)
    let mut mp3_carry: Option<mp3::model::M> = None;
    for inp in inputs {
        if !valid_name(&inp.name) {
            return Err(format!("invalid track name {:?}", inp.name));
        }
        tracks.push(match formats::sniff(&inp.bytes) {
            Sniff::Wav => match wav::parse(&inp.bytes) {
                Ok(w) => TrackData::Pcm(to_track(inp, w, TrackKind::Wav, "wav")),
                Err(_) => raw_track(inp, "wav", "stored as-is (unsupported or malformed WAV)"),
            },
            Sniff::Aiff => match aiff::parse(&inp.bytes) {
                Ok(w) => TrackData::Pcm(to_track(inp, w, TrackKind::Aiff, "aiff")),
                Err(_) => raw_track(inp, "aiff", "stored as-is (unsupported or malformed AIFF)"),
            },
            Sniff::Flac => match formats::read_flac(&inp.bytes) {
                Ok(s) if matches!(s.bits, 8 | 16 | 24) => {
                    TrackData::Pcm(pcm_track(inp, s, 0, "flac"))
                }
                // shortcut: other depths are stored raw
                Ok(_) => raw_track(inp, "flac", "stored as-is (unsupported bit depth)"),
                Err(e) => {
                    raw_track(inp, "flac", &format!("stored as-is (decode failed: {e})"))
                }
            },
            Sniff::Mp4(entry) if &entry == b"alac" => match formats::read_m4a(&inp.bytes) {
                Ok(s) if matches!(s.bits, 8 | 16 | 24) => {
                    TrackData::Pcm(pcm_track(inp, s, 1, "alac"))
                }
                // shortcut: other depths are stored raw
                Ok(_) => raw_track(inp, "alac", "stored as-is (unsupported bit depth)"),
                Err(e) => {
                    raw_track(inp, "alac", &format!("stored as-is (decode failed: {e})"))
                }
            },
            Sniff::Mp4(entry) => {
                let label = if &entry == b"mp4a" { "m4a aac" } else { "m4a" };
                raw_track(inp, label, RAW_REASON)
            }
            Sniff::Ogg(label) => raw_track(inp, label, RAW_REASON),
            Sniff::Aac => raw_track(inp, "aac", RAW_REASON),
            Sniff::Mp3 => mp3_track(inp, &mut mp3_carry),
            Sniff::Unknown => return Err(formats::UNSUPPORTED_MSG.into()),
        });
    }

    // size rule: FLAC/ALAC candidates stored PCM only if smaller than source
    let mut standalone: Vec<Option<codec::TrackPlan>> = Vec::with_capacity(n);
    let mut fallback: Vec<usize> = Vec::new();
    for (k, t) in tracks.iter().enumerate() {
        let TrackData::Pcm(t) = t else {
            standalone.push(None);
            continue;
        };
        if t.kind != TrackKind::Pcm {
            standalone.push(None);
            continue;
        }
        let plan = codec::plan_track(&TrackCtx {
            x: &t.x,
            d: None,
            float_word_bits: 0,
            sources: &[],
        });
        let total = plan.stereo.div_ceil(8) + pcm_meta_len(&t.name, t.tags.len());
        if total >= t.input_bytes {
            fallback.push(k);
        }
        standalone.push(Some(plan));
    }
    for k in fallback {
        let TrackData::Pcm(t) = &tracks[k] else { unreachable!() };
        let format = t.format;
        tracks[k] = raw_track(
            &inputs[k],
            format,
            "stored as-is (FLQ not smaller than original)",
        );
    }

    fn pcm_of(tracks: &[TrackData], k: usize) -> Option<&Track> {
        match &tracks[k] {
            TrackData::Pcm(t) => Some(t),
            _ => None,
        }
    }

    // ------------------------------------------------ pass A: analysis
    // Per PCM track: self-similarity (lags <= -BLOCK) plus full analyses
    // against a screened pool of earlier same-rate PCM tracks.
    enum Src {
        SelfRef(joint::JointAnalysis),
        Cross(usize, joint::JointAnalysis),
    }
    let mono_of = |k: usize| mono(&pcm_of(&tracks, k).unwrap().x);
    let t0_label = match &tracks[0] {
        TrackData::Pcm(t) => t.format.to_string(),
        TrackData::Mp3(_) => "mp3".to_string(),
        TrackData::Raw(r) => r.format.clone(),
    };
    let mut pools: Vec<Vec<Src>> = (0..n).map(|_| Vec::new()).collect();
    let mut reasons: Vec<String> = vec![String::new(); n];
    let mut max_nccs: Vec<Option<f64>> = vec![None; n];
    let mut had_eligible: Vec<bool> = vec![false; n];
    for k in 0..n {
        match &tracks[k] {
            TrackData::Mp3(m) => {
                reasons[k] = m.reason.clone();
            }
            TrackData::Raw(r) => {
                reasons[k] = r.reason.clone();
            }
            TrackData::Pcm(_) => {}
        }
    }
    for k in 0..n {
        let TrackData::Pcm(_) = &tracks[k] else { continue };
        if !joint {
            reasons[k] = "joint disabled (--no-joint)".into();
            continue;
        }
        let t = pcm_of(&tracks, k).unwrap();
        let mut ncc = 0f64;
        let mut any_ncc = false;
        // self-similarity: needs more than one block of material
        if t.frames > BLOCK as u64 {
            let mk = mono_of(k);
            let a = joint::analyze(&mk, &mk, -(BLOCK as i64));
            ncc = ncc.max(a.max_ncc);
            any_ncc = true;
            if a.seg_peaks.iter().any(|p| !p.is_empty()) {
                pools[k].push(Src::SelfRef(a));
            }
        }
        // cross-track candidates: earlier PCM tracks, same rate, >= BLOCK
        let mut eligible: Vec<usize> = Vec::new();
        if t.frames >= BLOCK as u64 {
            for j in 0..k {
                let Some(tj) = pcm_of(&tracks, j) else { continue };
                if tj.rate == t.rate && tj.frames >= BLOCK as u64 {
                    eligible.push(j);
                }
            }
        }
        if !eligible.is_empty() {
            had_eligible[k] = true;
            let chosen: Vec<usize> = if eligible.len() <= joint::SCREEN_K {
                eligible
            } else {
                // coarse screen: decimate both monos by SCREEN_DEC, keep top K
                let dec = |v: &[f64]| -> Vec<f64> {
                    v.chunks(joint::SCREEN_DEC)
                        .map(|c| c.iter().sum::<f64>() / c.len() as f64)
                        .collect()
                };
                let mk = dec(&mono_of(k));
                let mut scored: Vec<(usize, f64)> = eligible
                    .into_iter()
                    .map(|j| {
                        let m0 = dec(&mono_of(j));
                        (j, joint::analyze(&m0, &mk, i64::MAX).max_ncc)
                    })
                    .collect();
                scored.sort_by(|a, b| {
                    b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0))
                });
                let mut top: Vec<usize> =
                    scored.into_iter().take(joint::SCREEN_K).map(|(j, _)| j).collect();
                // refs must serialize strictly increasing
                top.sort_unstable();
                top
            };
            for j in chosen {
                let m0 = mono_of(j);
                let mk = mono_of(k);
                let a = joint::analyze(&m0, &mk, i64::MAX);
                ncc = ncc.max(a.max_ncc);
                if a.seg_peaks.iter().any(|p| !p.is_empty()) {
                    pools[k].push(Src::Cross(j, a));
                }
            }
        }
        if any_ncc || ncc > 0.0 {
            max_nccs[k] = Some(ncc);
        }
    }
    for k in 0..n {
        let Some(t) = pcm_of(&tracks, k) else { continue };
        if pools[k].is_empty() && reasons[k].is_empty() {
            reasons[k] = if t.frames < BLOCK as u64 {
                "track too short for joint analysis".into()
            } else if let Some(j) = (0..k).find(|&j| pcm_of(&tracks, j).is_some()) {
                let tj = pcm_of(&tracks, j).unwrap();
                if had_eligible[k] {
                    format!(
                        "no shared content (max NCC {:.3} < 0.20)",
                        max_nccs[k].unwrap_or(0.0)
                    )
                } else if tj.rate != t.rate {
                    format!("different sample rates ({} vs {} Hz)", tj.rate, t.rate)
                } else {
                    "track too short for joint analysis".into()
                }
            } else if k == 0 {
                "reference track".into()
            } else {
                format!("{t0_label} track (no joint coding)")
            };
        }
    }

    // last track planning step that needs each track's streams (for freeing)
    let mut last_use: Vec<usize> = (0..n).collect();
    for (k, pool) in pools.iter().enumerate() {
        for s in pool {
            if let Src::Cross(j, _) = s {
                last_use[*j] = last_use[*j].max(k);
            }
        }
    }
    // resident stream bytes per track, for the peak-heap estimate (streams
    // are freed during pass B, so capture sizes now)
    let xbytes: Vec<u64> = tracks
        .iter()
        .map(|t| match t {
            TrackData::Pcm(t) => {
                let xs: usize = t.x.iter().map(|c| std::mem::size_of_val(c.as_slice())).sum();
                let ds: usize = t.d.as_ref().map_or(0, |d| {
                    d.iter().map(|c| std::mem::size_of_val(c.as_slice())).sum()
                });
                (xs + ds) as u64
            }
            _ => 0,
        })
        .collect();

    // -------------------------------------- pass B: plan + write payloads
    struct Planned {
        independent: u64,
        stereo: u64,
        joint_bits: u64,
        float_bits: u64,
        joint_on: bool,
        self_blocks: u64,
        cross_blocks: Vec<(u8, u64)>, // (track index, blocks)
        nblocks: u64,
    }
    let mut plans: Vec<Option<Planned>> = (0..n).map(|_| None).collect();
    let mut refspecs: Vec<(bool, Vec<u8>)> = (0..n).map(|_| (false, Vec::new())).collect();
    let mut payloads: Vec<Vec<u8>> = (0..n).map(|_| Vec::new()).collect();
    for k in 0..n {
        if pcm_of(&tracks, k).is_none() {
            continue;
        }
        {
            let sources: Vec<codec::RefSource> = pools[k]
                .iter()
                .map(|s| match s {
                    Src::SelfRef(a) => codec::RefSource {
                        self_ref: true,
                        track: k as u8,
                        x: &pcm_of(&tracks, k).unwrap().x,
                        peaks: &a.seg_peaks,
                        refine: a.refine,
                    },
                    Src::Cross(j, a) => codec::RefSource {
                        self_ref: false,
                        track: *j as u8,
                        x: &pcm_of(&tracks, *j).unwrap().x,
                        peaks: &a.seg_peaks,
                        refine: a.refine,
                    },
                })
                .collect();
            let t = pcm_of(&tracks, k).unwrap();
            let ctx = TrackCtx {
                x: &t.x,
                d: t.d.as_deref(),
                float_word_bits: t.stream_word_bits,
                sources: &sources,
            };
            let mut plan = if sources.is_empty() && standalone[k].is_some() {
                standalone[k].take().unwrap()
            } else {
                codec::plan_track(&ctx)
            };
            // keep the pool only if strictly cheaper, then drop unused sources
            let mut sbits = 0u32;
            if plan.joint_on {
                let mut map = vec![u8::MAX; sources.len()];
                let mut self_ref = false;
                let mut refs: Vec<u8> = Vec::new();
                for (si, s) in sources.iter().enumerate() {
                    if plan.source_blocks.get(si).copied().unwrap_or(0) == 0 {
                        continue;
                    }
                    map[si] = u8::from(self_ref) + refs.len() as u8;
                    if s.self_ref {
                        self_ref = true;
                    } else {
                        refs.push(s.track);
                    }
                }
                for bp in &mut plan.blocks {
                    if let Some(a) = &mut bp.alt {
                        for g in a.refs.iter_mut().flatten() {
                            g.src = map[g.src as usize];
                        }
                    }
                }
                refspecs[k] = (self_ref, refs);
                sbits = codec::src_bits(usize::from(self_ref) + refspecs[k].1.len());
            }
            let mut bw = BitWriter::new();
            codec::write_track(&mut bw, &ctx, &plan, sbits);
            payloads[k] = bw.into_bytes();
            let nb = plan.blocks.len() as u64;
            let mut self_blocks = 0u64;
            let mut cross: Vec<(u8, u64)> = Vec::new();
            if plan.joint_on {
                for (si, s) in sources.iter().enumerate() {
                    let b = plan.source_blocks.get(si).copied().unwrap_or(0);
                    if b == 0 {
                        continue;
                    }
                    if s.self_ref {
                        self_blocks = b;
                    } else {
                        cross.push((s.track, b));
                    }
                }
            }
            plans[k] = Some(Planned {
                independent: plan.independent,
                stereo: plan.stereo,
                joint_bits: plan.joint_bits,
                float_bits: plan.float_bits,
                joint_on: plan.joint_on,
                self_blocks,
                cross_blocks: cross,
                nblocks: nb,
            });
        }
        // free streams no track will reference again
        for (j, lu) in last_use.iter().enumerate().take(k + 1) {
            if *lu <= k
                && let TrackData::Pcm(t) = &mut tracks[j]
            {
                t.x = Vec::new();
                t.d = None;
            }
        }
    }
    for (k, p) in plans.iter().enumerate() {
        let Some(p) = p else { continue };
        if reasons[k].is_empty() || p.joint_on {
            reasons[k] = if p.joint_on {
                let mut parts = Vec::new();
                if p.self_blocks > 0 {
                    parts.push(format!("self {}/{} blocks", p.self_blocks, p.nblocks));
                }
                for (j, b) in &p.cross_blocks {
                    parts.push(format!("track {} in {}/{} blocks", *j + 1, b, p.nblocks));
                }
                parts.join(", ")
            } else {
                "no block benefited".to_string()
            };
        }
    }

    // --------------------------------------------- serialize (v3 layout)
    let mut meta = Vec::new();
    let mut metas = Vec::with_capacity(n);
    for (k, t) in tracks.iter().enumerate() {
        match t {
            TrackData::Pcm(t) => {
                let name = t.name.as_bytes();
                match t.kind {
                    TrackKind::Wav | TrackKind::Aiff => {
                        meta.push(if t.kind == TrackKind::Wav { 0 } else { 2 });
                        meta.extend((name.len() as u16).to_le_bytes());
                        meta.extend(name);
                        meta.extend((t.prefix.len() as u32).to_le_bytes());
                        meta.extend(&t.prefix);
                        meta.extend((t.suffix.len() as u32).to_le_bytes());
                        meta.extend(&t.suffix);
                        meta.extend(t.frames.to_le_bytes());
                        meta.extend(t.crc.to_le_bytes());
                        meta.push(t.float_mode);
                        meta.extend(t.e.to_le_bytes());
                        meta.push(u8::from(refspecs[k].0));
                        meta.push(refspecs[k].1.len() as u8);
                        meta.extend(&refspecs[k].1);
                        meta.extend((payloads[k].len() as u64).to_le_bytes());
                        metas.push(
                            36 + name.len() as u64
                                + t.prefix.len() as u64
                                + t.suffix.len() as u64
                                + refspecs[k].1.len() as u64,
                        );
                    }
                    TrackKind::Pcm => {
                        meta.push(3);
                        meta.extend((name.len() as u16).to_le_bytes());
                        meta.extend(name);
                        meta.push(t.src);
                        meta.extend(t.channels.to_le_bytes());
                        meta.extend(t.rate.to_le_bytes());
                        meta.extend(t.bits.to_le_bytes());
                        meta.extend(t.frames.to_le_bytes());
                        meta.extend(t.pcm_crc.to_le_bytes());
                        meta.extend((t.tags.len() as u32).to_le_bytes());
                        meta.extend(&t.tags);
                        meta.push(u8::from(refspecs[k].0));
                        meta.push(refspecs[k].1.len() as u8);
                        meta.extend(&refspecs[k].1);
                        meta.extend((payloads[k].len() as u64).to_le_bytes());
                        metas.push(
                            pcm_meta_len(&t.name, t.tags.len()) + refspecs[k].1.len() as u64,
                        );
                    }
                    _ => unreachable!(),
                }
            }
            TrackData::Mp3(m) => {
                let name = m.name.as_bytes();
                meta.push(1); // kind: MP3
                meta.extend((name.len() as u16).to_le_bytes());
                meta.extend(name);
                meta.extend((m.bytes.len() as u64).to_le_bytes());
                meta.extend(m.crc.to_le_bytes());
                meta.push(m.mode);
                meta.extend((m.payload.len() as u64).to_le_bytes());
                metas.push(24 + name.len() as u64);
            }
            TrackData::Raw(r) => {
                let name = r.name.as_bytes();
                let label = r.format.as_bytes();
                meta.push(4); // kind: raw stored file
                meta.extend((name.len() as u16).to_le_bytes());
                meta.extend(name);
                meta.extend((r.bytes.len() as u64).to_le_bytes());
                meta.extend(r.crc.to_le_bytes());
                meta.push(label.len() as u8);
                meta.extend(label);
                meta.extend((r.bytes.len() as u64).to_le_bytes());
                metas.push(24 + name.len() as u64 + label.len() as u64);
            }
        }
    }
    let packed = miniz_oxide::deflate::compress_to_vec(&meta, 10);
    let (meta_mode, meta_stored) = if packed.len() < meta.len() {
        (1u8, packed)
    } else {
        (0u8, meta.clone())
    };

    let mut out: Vec<u8> = Vec::new();
    out.extend(b"FLQ1");
    out.push(3);
    out.push(n as u8);
    out.extend((BLOCK as u32).to_le_bytes());
    out.push(meta_mode);
    out.extend((meta.len() as u32).to_le_bytes());
    out.extend((meta_stored.len() as u32).to_le_bytes());
    out.extend(&meta_stored);
    // per-track payloads in track order (lengths live in the metas)
    for (k, t) in tracks.iter().enumerate() {
        match t {
            TrackData::Pcm(_) => out.extend(&payloads[k]),
            TrackData::Mp3(m) => out.extend(&m.payload),
            TrackData::Raw(r) => out.extend(&r.bytes),
        }
    }

    let mut stats = Stats {
        tracks: Vec::with_capacity(n),
        total_bytes: out.len() as u64,
        est_encode_heap: est_encode_heap(&xbytes, &last_use),
    };
    for (k, t) in tracks.iter().enumerate() {
        match t {
            TrackData::Pcm(t) => {
                let p = plans[k].as_ref().unwrap();
                stats.tracks.push(TrackStats {
                    name: t.name.clone(),
                    kind: t.kind,
                    format: t.format.to_string(),
                    input_bytes: t.input_bytes,
                    float_mode: t.float_mode,
                    bits: t.bits,
                    channels: t.channels,
                    rate: t.rate,
                    frames: t.frames,
                    meta_bytes: metas[k],
                    independent_bits: p.independent,
                    stereo_bits: p.stereo,
                    final_bits: if p.joint_on { p.joint_bits } else { p.stereo },
                    float_bits: p.float_bits,
                    joint: p.joint_on,
                    joint_reason: reasons[k].clone(),
                    max_ncc: max_nccs[k],
                    self_ref_blocks: p.self_blocks,
                    cross_ref_blocks: p.cross_blocks.iter().map(|(_, b)| *b).sum(),
                    refs: refspecs[k].1.iter().map(|&j| usize::from(j)).collect(),
                    blocks: p.nblocks,
                    mp3: None,
                });
            }
            TrackData::Mp3(m) => {
                stats.tracks.push(TrackStats {
                    name: m.name.clone(),
                    kind: TrackKind::Mp3,
                    format: "mp3".to_string(),
                    input_bytes: m.bytes.len() as u64,
                    float_mode: 0,
                    bits: 0,
                    channels: u16::from(m.mp3.channels),
                    rate: m.mp3.rate,
                    frames: 1152 * m.mp3.frames,
                    meta_bytes: metas[k],
                    independent_bits: 0,
                    stereo_bits: 0,
                    final_bits: m.payload.len() as u64 * 8,
                    float_bits: 0,
                    joint: false,
                    joint_reason: reasons[k].clone(),
                    max_ncc: max_nccs[k],
                    self_ref_blocks: 0,
                    cross_ref_blocks: 0,
                    refs: Vec::new(),
                    blocks: 0,
                    mp3: Some(m.mp3.clone()),
                });
            }
            TrackData::Raw(r) => {
                stats.tracks.push(TrackStats {
                    name: r.name.clone(),
                    kind: TrackKind::Raw,
                    format: r.format.clone(),
                    input_bytes: r.bytes.len() as u64,
                    float_mode: 0,
                    bits: 0,
                    channels: 0,
                    rate: 0,
                    frames: 0,
                    meta_bytes: metas[k],
                    independent_bits: 0,
                    stereo_bits: 0,
                    final_bits: r.bytes.len() as u64 * 8,
                    float_bits: 0,
                    joint: false,
                    joint_reason: reasons[k].clone(),
                    max_ncc: max_nccs[k],
                    self_ref_blocks: 0,
                    cross_ref_blocks: 0,
                    refs: Vec::new(),
                    blocks: 0,
                    mp3: None,
                });
            }
        }
    }
    Ok((out, stats))
}

// ---------------------------------------------------------------- decode

/// File stem for generated PCM-kind outputs (drops the last extension).
fn pcm_stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(d) if d > 0 => &name[..d],
        _ => name,
    }
}

struct WavMeta {
    name: String,
    prefix: Vec<u8>,
    suffix: Vec<u8>,
    frames: u64,
    crc: u32,
    float_mode: u8,
    e: i16,
    self_ref: bool,
    refs: Vec<u8>,
    plen: u64,
    fmt: wav::Fmt,
}
struct PcmMeta {
    name: String,
    channels: u16,
    rate: u32,
    bits: u16,
    frames: u64,
    pcm_crc: u32,
    tags: Vec<u8>,
    self_ref: bool,
    refs: Vec<u8>,
    plen: u64,
}
enum Meta {
    Wav(WavMeta),
    Aiff(WavMeta),
    Pcm(PcmMeta),
    Mp3 { name: String, len: u64, crc: u32, mode: u8, plen: u64 },
    Raw { name: String, len: u64, crc: u32, plen: u64 },
}
impl Meta {
    fn pcm_coded(&self) -> bool {
        matches!(self, Meta::Wav(_) | Meta::Aiff(_) | Meta::Pcm(_))
    }
    fn name(&self) -> &str {
        match self {
            Meta::Wav(m) | Meta::Aiff(m) => &m.name,
            Meta::Pcm(m) => &m.name,
            Meta::Mp3 { name, .. } | Meta::Raw { name, .. } => name,
        }
    }
    /// v3 per-track payload length (metas of older versions carry none).
    fn plen(&self) -> u64 {
        match self {
            Meta::Wav(m) | Meta::Aiff(m) => m.plen,
            Meta::Pcm(m) => m.plen,
            Meta::Mp3 { plen, .. } | Meta::Raw { plen, .. } => *plen,
        }
    }
    fn rate(&self) -> u32 {
        match self {
            Meta::Wav(m) | Meta::Aiff(m) => m.fmt.rate,
            Meta::Pcm(m) => m.rate,
            _ => 0,
        }
    }
    /// Cross-track reference list (empty for non-PCM kinds).
    fn refs(&self) -> &[u8] {
        match self {
            Meta::Wav(m) | Meta::Aiff(m) => &m.refs,
            Meta::Pcm(m) => &m.refs,
            _ => &[],
        }
    }
}

fn meta_take<'a>(src: &'a [u8], p: &mut usize, n: usize) -> Result<&'a [u8], String> {
    let end = (*p).saturating_add(n);
    let s = src.get(*p..end).ok_or("truncated file")?;
    *p += n;
    Ok(s)
}
fn meta_u8(src: &[u8], p: &mut usize) -> Result<u8, String> {
    Ok(meta_take(src, p, 1)?[0])
}
fn meta_u16(src: &[u8], p: &mut usize) -> Result<u16, String> {
    Ok(u16::from_le_bytes(meta_take(src, p, 2)?.try_into().unwrap()))
}
fn meta_u32(src: &[u8], p: &mut usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(meta_take(src, p, 4)?.try_into().unwrap()))
}
fn meta_u64(src: &[u8], p: &mut usize) -> Result<u64, String> {
    Ok(u64::from_le_bytes(meta_take(src, p, 8)?.try_into().unwrap()))
}
fn meta_name(src: &[u8], p: &mut usize) -> Result<String, String> {
    let nl = meta_u16(src, p)? as usize;
    let name = String::from_utf8(meta_take(src, p, nl)?.to_vec())
        .map_err(|_| "invalid UTF-8 in track name")?;
    if !valid_name(&name) {
        return Err(format!("invalid track name {name:?}"));
    }
    Ok(name)
}

/// Reference spec: v3 reads (self_ref, nref, refs[]); v1/v2 maps the joint
/// flag to "reference track 0". Returns (self_ref, cross ref track indices).
fn meta_refspec(src: &[u8], p: &mut usize, v3: bool, i: usize) -> Result<(bool, Vec<u8>), String> {
    if v3 {
        let self_ref = match meta_u8(src, p)? {
            0 => false,
            1 => true,
            b => return Err(format!("invalid self_ref flag {b}")),
        };
        let nr = meta_u8(src, p)? as usize;
        Ok((self_ref, meta_take(src, p, nr)?.to_vec()))
    } else {
        match meta_u8(src, p)? {
            0 => Ok((false, Vec::new())),
            1 => {
                if i == 0 {
                    return Err("joint flag on first track".into());
                }
                Ok((false, vec![0]))
            }
            b => Err(format!("invalid joint flag {b}")),
        }
    }
}

fn read_one_meta(src: &[u8], p: &mut usize, kind: u8, i: usize, v3: bool) -> Result<Meta, String> {
    match kind {
        1 => {
            let name = meta_name(src, p)?;
            let len = meta_u64(src, p)?;
            let crc = meta_u32(src, p)?;
            let mode = meta_u8(src, p)?;
            let plen = if v3 { meta_u64(src, p)? } else { 0 };
            match mode {
                m @ (0 | 1) => Ok(Meta::Mp3 { name, len, crc, mode: m, plen }),
                // mode 2 = modeled, chained into the shared carry (v3 only)
                2 if v3 => Ok(Meta::Mp3 { name, len, crc, mode: 2, plen }),
                m => Err(format!("unsupported mp3 mode {m}")),
            }
        }
        3 => {
            let name = meta_name(src, p)?;
            let src_b = meta_u8(src, p)?;
            if src_b > 1 {
                return Err(format!("invalid pcm source {src_b}"));
            }
            let channels = meta_u16(src, p)?;
            let rate = meta_u32(src, p)?;
            let bits = meta_u16(src, p)?;
            let frames = meta_u64(src, p)?;
            let pcm_crc = meta_u32(src, p)?;
            let tl = meta_u32(src, p)? as usize;
            let tags = meta_take(src, p, tl)?.to_vec();
            let (self_ref, refs) = meta_refspec(src, p, v3, i)?;
            let plen = if v3 { meta_u64(src, p)? } else { 0 };
            if !matches!(bits, 8 | 16 | 24) {
                return Err(format!("invalid pcm bit depth {bits}"));
            }
            if channels == 0 || channels > wav::MAX_CHANNELS {
                return Err(format!("invalid channel count {channels}"));
            }
            if u128::from(frames) * u128::from(channels) * 3 > u128::from(u32::MAX) {
                return Err("frame count too large".into());
            }
            Ok(Meta::Pcm(PcmMeta {
                name, channels, rate, bits, frames, pcm_crc, tags, self_ref, refs, plen,
            }))
        }
        4 => {
            let name = meta_name(src, p)?;
            let len = meta_u64(src, p)?;
            let crc = meta_u32(src, p)?;
            let ll = meta_u8(src, p)? as usize;
            let _label = meta_take(src, p, ll)?; // format label (stats only)
            let plen = if v3 { meta_u64(src, p)? } else { 0 };
            Ok(Meta::Raw { name, len, crc, plen })
        }
        0 | 2 => {
            let name = meta_name(src, p)?;
            let pl = meta_u32(src, p)? as usize;
            let prefix = meta_take(src, p, pl)?.to_vec();
            let sl = meta_u32(src, p)? as usize;
            let suffix = meta_take(src, p, sl)?.to_vec();
            let frames = meta_u64(src, p)?;
            let crc = meta_u32(src, p)?;
            let float_mode = meta_u8(src, p)?;
            let e = i16::from_le_bytes(meta_take(src, p, 2)?.try_into().unwrap());
            let (self_ref, refs) = meta_refspec(src, p, v3, i)?;
            let plen = if v3 { meta_u64(src, p)? } else { 0 };
            let fmt = if kind == 0 {
                wav::parse_prefix(&prefix)?
            } else {
                aiff::parse_prefix(&prefix)?
            };
            if u128::from(frames) * u128::from(fmt.block_align) > u128::from(u32::MAX) {
                return Err("frame count too large".into());
            }
            match (float_mode, fmt.tag, fmt.bits) {
                (0, 1, _) | (1, 3, 32) | (2, 3, 64) | (3, 3, 64) => {}
                _ => return Err("float_mode inconsistent with fmt".into()),
            }
            let m = WavMeta {
                name, prefix, suffix, frames, crc, float_mode, e, self_ref, refs, plen, fmt,
            };
            Ok(if kind == 0 { Meta::Wav(m) } else { Meta::Aiff(m) })
        }
        k => Err(format!("invalid track kind {k}")),
    }
}

/// Codec pool + the `cross` slice (indexed by Pool::Cross entries) for one
/// PCM-coded track.
struct RefCtx<'a> {
    pool: Vec<codec::Pool>,
    cross: Vec<&'a [Vec<i64>]>,
}
fn pool_and_cross<'a>(
    self_ref: bool,
    refs: &[u8],
    decoded: &'a [Option<Vec<Vec<i64>>>],
) -> Result<RefCtx<'a>, String> {
    let mut pool = Vec::with_capacity(refs.len() + 1);
    if self_ref {
        pool.push(codec::Pool::SelfRef);
    }
    for ci in 0..refs.len() {
        pool.push(codec::Pool::Cross(ci as u8));
    }
    let mut cross = Vec::with_capacity(refs.len());
    for &j in refs {
        cross.push(
            decoded[usize::from(j)]
                .as_deref()
                .ok_or("joint track without PCM reference")?,
        );
    }
    Ok(RefCtx { pool, cross })
}

pub fn decode(data: &[u8], pcm_out: PcmOut) -> Result<Vec<Output>, String> {
    decode_inner(data, pcm_out, None)
}

/// Decode only the named tracks (exact meta names). v3 files skip the
/// payloads of unneeded tracks; v1/v2 files are read fully (one sequential
/// bitstream) and only the requested outputs are produced. "Needed" = the
/// requested tracks, the transitive closure of their PCM references, and —
/// for a mode-2 (chained-model) MP3 track — every earlier modeled MP3
/// track, whose payloads advance the shared model the chain depends on.
pub fn decode_subset(
    data: &[u8],
    pcm_out: PcmOut,
    names: &[String],
) -> Result<Vec<Output>, String> {
    decode_inner(data, pcm_out, Some(names))
}

fn decode_inner(
    data: &[u8],
    pcm_out: PcmOut,
    only: Option<&[String]>,
) -> Result<Vec<Output>, String> {
    let mut p = 0usize;
    let take = |p: &mut usize, n: usize| meta_take(data, p, n);
    if take(&mut p, 4)? != b"FLQ1" {
        return Err("bad magic".into());
    }
    let version = take(&mut p, 1)?[0];
    if !(1..=3).contains(&version) {
        return Err(format!("unsupported version {version}"));
    }
    let nt = take(&mut p, 1)?[0] as usize;
    if nt == 0 {
        return Err("no tracks".into());
    }
    let block = u32::from_le_bytes(take(&mut p, 4)?.try_into().unwrap());
    if block != BLOCK as u32 {
        return Err(format!("unsupported block size {block}"));
    }

    let mut metas = Vec::with_capacity(nt);
    if version == 3 {
        let meta_mode = take(&mut p, 1)?[0];
        let raw_len = u32::from_le_bytes(take(&mut p, 4)?.try_into().unwrap()) as usize;
        let mlen = u32::from_le_bytes(take(&mut p, 4)?.try_into().unwrap()) as usize;
        if raw_len > 512 * 1024 * 1024 || raw_len > mlen.saturating_mul(1000) {
            return Err("absurd meta section length".into());
        }
        let packed = take(&mut p, mlen)?;
        let meta: Vec<u8> = match meta_mode {
            0 => packed.to_vec(),
            1 => miniz_oxide::inflate::decompress_to_vec_with_limit(packed, raw_len)
                .map_err(|_| "corrupt deflate meta section".to_string())?,
            m => return Err(format!("invalid meta mode {m}")),
        };
        if meta.len() != raw_len {
            return Err("meta length mismatch".into());
        }
        let mut mp = 0usize;
        for i in 0..nt {
            let kind = meta_u8(&meta, &mut mp)?;
            metas.push(read_one_meta(&meta, &mut mp, kind, i, true)?);
        }
        if mp != meta.len() {
            return Err("trailing bytes in meta section".into());
        }
        // validate refspecs: strictly increasing, < own index, PCM-coded, same rate
        for (i, m) in metas.iter().enumerate() {
            let refs = m.refs();
            let mut prev = 0usize;
            for (ri, &j) in refs.iter().enumerate() {
                let j = usize::from(j);
                if j >= i {
                    return Err(format!(
                        "track {} references later/self track {}",
                        i + 1,
                        j + 1
                    ));
                }
                if ri > 0 && j <= prev {
                    return Err(format!("track {} references out of order", i + 1));
                }
                if !metas[j].pcm_coded() {
                    return Err(format!("track {} references a non-audio track", i + 1));
                }
                if metas[j].rate() != m.rate() {
                    return Err(format!(
                        "track {} references a different sample rate",
                        i + 1
                    ));
                }
                prev = j;
            }
        }
    } else {
        for i in 0..nt {
            let kind = if version == 1 { 0 } else { take(&mut p, 1)?[0] };
            metas.push(read_one_meta(data, &mut p, kind, i, false)?);
        }
    }

    // --only subset selection: `wanted` maps requested names to tracks.
    let wanted: Option<Vec<bool>> = match only {
        None => None,
        Some(names) => {
            let mut w = vec![false; nt];
            for n in names {
                let mut any = false;
                for (i, m) in metas.iter().enumerate() {
                    if m.name() == n.as_str() {
                        w[i] = true;
                        any = true;
                    }
                }
                if !any {
                    let avail =
                        metas.iter().map(|m| m.name()).collect::<Vec<_>>().join(", ");
                    return Err(format!("unknown track {n:?} (tracks: {avail})"));
                }
            }
            Some(w)
        }
    };
    // Tracks whose payloads must be read: the wanted ones, the transitive
    // closure of their PCM references, and every earlier modeled MP3 track
    // of each wanted mode-2 track (its shared-model carry depends on them).
    // v1/v2 use one sequential bitstream, so everything is read anyway.
    let mut needed: Vec<bool> = match (&wanted, version) {
        (Some(w), 3) => w.clone(),
        _ => vec![true; nt],
    };
    {
        let mut stack: Vec<usize> = (0..nt).filter(|&i| needed[i]).collect();
        while let Some(i) = stack.pop() {
            let refs = metas[i].refs();
            for &j in refs {
                let j = usize::from(j);
                if !needed[j] {
                    needed[j] = true;
                    stack.push(j);
                }
            }
            if matches!(&metas[i], Meta::Mp3 { mode: 2, .. }) {
                for j in 0..i {
                    if !needed[j] && matches!(&metas[j], Meta::Mp3 { mode: 1 | 2, .. }) {
                        needed[j] = true;
                        stack.push(j);
                    }
                }
            }
        }
    }

    // v1/v2: one shared PCM bitstream; v3: per-track payloads sized by meta.
    let mut shared: Option<BitReader> = None;
    if version < 3 {
        let plen = u64::from_le_bytes(take(&mut p, 8)?.try_into().unwrap()) as usize;
        shared = Some(BitReader::new(take(&mut p, plen)?));
    }
    let mut decoded: Vec<Option<Vec<Vec<i64>>>> = (0..nt).map(|_| None).collect();
    // last read track index that may still reference each track (for freeing)
    let mut last_use: Vec<usize> = (0..nt).collect();
    for (i, m) in metas.iter().enumerate() {
        for &j in m.refs() {
            if needed[i] {
                last_use[usize::from(j)] = last_use[usize::from(j)].max(i);
            }
        }
    }
    let mut used_names: Vec<String> = Vec::with_capacity(nt);
    let mut outs = Vec::with_capacity(nt);
    // shared model for chained (mode-2) MP3 payloads, advanced by every
    // modeled track in order — needed ones included even when not emitted
    let mut mp3_carry: Option<mp3::model::M> = None;
    for (i, m) in metas.iter().enumerate() {
        if version == 3 && !needed[i] {
            // v3 payload lengths live in the metas: skip unneeded payloads
            take(&mut p, m.plen() as usize)?;
            continue;
        }
        let emit = wanted.as_ref().is_none_or(|w| w[i]);
        let out: Option<(String, Vec<u8>, bool)> = match m {
            Meta::Mp3 { name, len, mode, plen, .. } => {
                let l = if version == 3 {
                    *plen as usize
                } else {
                    u64::from_le_bytes(take(&mut p, 8)?.try_into().unwrap()) as usize
                };
                let payload = take(&mut p, l)?;
                match mode {
                    0 => {
                        if emit && l as u64 != *len {
                            return Err("mp3 payload length mismatch".into());
                        }
                        emit.then(|| (name.clone(), payload.to_vec(), true))
                    }
                    1 => {
                        let mut mm = mp3::model::fresh_for(payload)
                            .map_err(|e| format!("mp3 track {}: {e}", i + 1))?;
                        let f = mp3::model::decompress_with(payload, *len, &mut mm)
                            .map_err(|e| format!("mp3 track {}: {e}", i + 1))?;
                        mp3_carry = Some(mm);
                        if !emit {
                            None
                        } else {
                            let b = mp3::rebuild(&f);
                            if b.len() as u64 != *len {
                                return Err("mp3 rebuild length mismatch".into());
                            }
                            Some((name.clone(), b, true))
                        }
                    }
                    _ => {
                        // mode 2: decode into the shared-model carry
                        let mm = mp3_carry.as_mut().ok_or_else(|| {
                            format!(
                                "mp3 track {}: chained payload without earlier modeled track",
                                i + 1
                            )
                        })?;
                        let f = mp3::model::decompress_with(payload, *len, mm)
                            .map_err(|e| format!("mp3 track {}: {e}", i + 1))?;
                        if !emit {
                            None
                        } else {
                            let b = mp3::rebuild(&f);
                            if b.len() as u64 != *len {
                                return Err("mp3 rebuild length mismatch".into());
                            }
                            Some((name.clone(), b, true))
                        }
                    }
                }
            }
            Meta::Raw { name, len, plen, .. } => {
                let l = if version == 3 {
                    *plen as usize
                } else {
                    u64::from_le_bytes(take(&mut p, 8)?.try_into().unwrap()) as usize
                };
                if emit && l as u64 != *len {
                    return Err("raw payload length mismatch".into());
                }
                let payload = take(&mut p, l)?;
                emit.then(|| (name.clone(), payload.to_vec(), true))
            }
            Meta::Pcm(m) => {
                let mut local;
                let r: &mut BitReader = if version == 3 {
                    local = BitReader::new(take(&mut p, m.plen as usize)?);
                    &mut local
                } else {
                    shared.as_mut().unwrap()
                };
                let RefCtx { pool, cross } = pool_and_cross(m.self_ref, &m.refs, &decoded)?;
                let meta = codec::TrackMeta {
                    channels: m.channels as usize,
                    frames: m.frames as usize,
                    pool,
                    float: None,
                };
                let core = codec::read_track(r, &meta, &cross)?;
                if r.overrun {
                    return Err("truncated payload".into());
                }
                if !emit {
                    // needed only as a reference: keep the streams, skip
                    // output construction and the CRC write
                    if last_use[i] > i {
                        decoded[i] = Some(core.x);
                    }
                    None
                } else {
                    if last_use[i] > i {
                        decoded[i] = Some(core.x.clone());
                    }
                    let pcm = wav::ints_to_pcm(&core.x, m.bits, m.frames as usize);
                    if crc32(&pcm) != m.pcm_crc {
                        return Err(format!("PCM CRC32 mismatch on track {}", i + 1));
                    }
                    let (ext, file) = match pcm_out {
                        PcmOut::Flac => (
                            "flac",
                            formats::encode_flac(&core.x, m.bits, m.channels, m.rate, &m.tags)
                                .map_err(|e| format!("pcm track {}: {e}", i + 1))?,
                        ),
                        PcmOut::Wav => (
                            "wav",
                            formats::make_wav(&core.x, m.bits, m.channels, m.rate),
                        ),
                    };
                    Some((format!("{}.{}", pcm_stem(&m.name), ext), file, false))
                }
            }
            wm @ (Meta::Wav(_) | Meta::Aiff(_)) => {
                let (m, is_aiff) = match wm {
                    Meta::Wav(w) => (w, false),
                    Meta::Aiff(w) => (w, true),
                    _ => unreachable!(),
                };
                let mut local;
                let r: &mut BitReader = if version == 3 {
                    local = BitReader::new(take(&mut p, m.plen as usize)?);
                    &mut local
                } else {
                    shared.as_mut().unwrap()
                };
                let RefCtx { pool, cross } = pool_and_cross(m.self_ref, &m.refs, &decoded)?;
                let meta = codec::TrackMeta {
                    channels: m.fmt.channels as usize,
                    frames: m.frames as usize,
                    pool,
                    float: if m.float_mode == 0 {
                        None
                    } else {
                        Some(codec::FloatMeta {
                            mw: if m.float_mode == 3 { 52 } else { 23 },
                            e: m.e,
                            stream_bits: if m.float_mode == 3 { 64 } else { 32 },
                            pcm_bits: m.fmt.bits,
                        })
                    },
                };
                let core = codec::read_track(r, &meta, &cross)?;
                if r.overrun {
                    return Err("truncated payload".into());
                }
                if !emit {
                    if last_use[i] > i {
                        decoded[i] = Some(core.x);
                    }
                    None
                } else {
                    if last_use[i] > i {
                        decoded[i] = Some(core.x.clone());
                    }
                    let pcm = match &core.words {
                        Some(wds) => wav::words_to_pcm(wds, m.fmt.bits, m.frames as usize),
                        None => wav::ints_to_pcm(&core.x, m.fmt.bits, m.frames as usize),
                    };
                    let pcm = if is_aiff {
                        aiff::to_native(&m.prefix, &pcm)
                            .map_err(|e| format!("aiff track {}: {e}", i + 1))?
                    } else {
                        pcm
                    };
                    let mut file = m.prefix.clone();
                    file.extend(&pcm);
                    file.extend(&m.suffix);
                    Some((m.name.clone(), file, true))
                }
            }
        };
        if let Some((name, bytes, exact)) = out {
            let crc = match m {
                Meta::Mp3 { crc, .. } | Meta::Raw { crc, .. } => Some(*crc),
                Meta::Wav(m) | Meta::Aiff(m) => Some(m.crc),
                Meta::Pcm(_) => None, // already checked against pcm_crc
            };
            if let Some(crc) = crc
                && crc32(&bytes) != crc
            {
                return Err(format!("CRC32 mismatch on track {}", i + 1));
            }
            let mut name = name;
            if used_names.contains(&name) {
                name = format!("{}_{}", i + 1, name);
            }
            used_names.push(name.clone());
            outs.push(Output { name, bytes, exact });
        }
        // free streams no later read track will reference again
        for (j, lu) in last_use.iter().enumerate() {
            if *lu <= i {
                decoded[j] = None;
            }
        }
    }
    if shared.as_ref().is_some_and(|r| r.overrun) {
        return Err("truncated payload".into());
    }
    Ok(outs)
}

/// Decode a file's audio to integer PCM streams; see `formats::decode_pcm`.
pub fn decode_pcm(bytes: &[u8]) -> Result<DecodedPcm, String> {
    formats::decode_pcm(bytes)
}

/// Expected tag blob for an input file; see `formats::tag_blocks`.
pub fn tag_blocks(bytes: &[u8]) -> Result<Vec<u8>, String> {
    formats::tag_blocks(bytes)
}

pub use formats::DecodedPcm;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::f64::consts::PI;

    // ------------------------------------------------------ wav builders

    fn riff(chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut body = b"WAVE".to_vec();
        for (id, data) in chunks {
            body.extend(*id);
            body.extend((data.len() as u32).to_le_bytes());
            body.extend(data);
            if data.len() % 2 == 1 {
                body.push(0);
            }
        }
        let mut out = b"RIFF".to_vec();
        out.extend((body.len() as u32).to_le_bytes());
        out.extend(body);
        out
    }

    fn fmt16(tag: u16, ch: u16, rate: u32, bits: u16) -> Vec<u8> {
        let align = ch * (bits / 8);
        let mut f = Vec::new();
        f.extend(tag.to_le_bytes());
        f.extend(ch.to_le_bytes());
        f.extend(rate.to_le_bytes());
        f.extend((rate * u32::from(align)).to_le_bytes());
        f.extend(align.to_le_bytes());
        f.extend(bits.to_le_bytes());
        f
    }

    fn fmt_ext(ch: u16, rate: u32, bits: u16, sub: u16) -> Vec<u8> {
        let mut p = fmt16(0xFFFE, ch, rate, bits);
        p.extend(22u16.to_le_bytes());
        p.extend(bits.to_le_bytes());
        p.extend(0u32.to_le_bytes());
        p.extend(sub.to_le_bytes());
        p.extend([0u8; 14]);
        p
    }

    fn int_wav(tag: u16, ch: u16, rate: u32, bits: u16, samples: &[Vec<i64>]) -> Vec<u8> {
        let frames = samples.first().map_or(0, |s| s.len());
        let bps = (bits / 8) as usize;
        let mut pcm = vec![0u8; frames * ch as usize * bps];
        for (f, frame) in pcm.chunks_exact_mut(ch as usize * bps).enumerate() {
            for (c, s) in samples.iter().enumerate() {
                let at = c * bps;
                let v = s[f];
                match bits {
                    8 => frame[at] = (v + 128) as u8,
                    16 => frame[at..at + 2].copy_from_slice(&(v as i16).to_le_bytes()),
                    24 => frame[at..at + 3].copy_from_slice(&(v as i32).to_le_bytes()[..3]),
                    _ => frame[at..at + 4].copy_from_slice(&(v as i32).to_le_bytes()),
                }
            }
        }
        riff(&[(b"fmt ", fmt16(tag, ch, rate, bits)), (b"data", pcm)])
    }

    fn float_wav(bits: u16, ch: u16, rate: u32, samples: &[Vec<u64>]) -> Vec<u8> {
        let frames = samples.first().map_or(0, |s| s.len());
        let bps = (bits / 8) as usize;
        let mut pcm = vec![0u8; frames * ch as usize * bps];
        for (f, frame) in pcm.chunks_exact_mut(ch as usize * bps).enumerate() {
            for (c, s) in samples.iter().enumerate() {
                let at = c * bps;
                frame[at..at + bps].copy_from_slice(&s[f].to_le_bytes()[..bps]);
            }
        }
        riff(&[(b"fmt ", fmt16(3, ch, rate, bits)), (b"data", pcm)])
    }

    fn lcg(state: &mut u64) -> f64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (*state >> 33) as f64 / u32::MAX as f64
    }

    /// Deterministic audio-like signal: LCG noise through a resonator.
    fn resonator(seed: u64, rate: u32, frames: usize) -> Vec<i64> {
        let f0 = 2000.0 + (seed % 4000) as f64;
        let w = 2.0 * PI * f0 / rate as f64;
        let r = 0.95f64;
        let a1 = 2.0 * r * w.cos();
        let a2 = -r * r;
        let mut rng = seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(1);
        let (mut y1, mut y2) = (0.0, 0.0);
        let mut out = Vec::with_capacity(frames);
        let mut peak = 0f64;
        for n in 0..frames {
            let noise = (lcg(&mut rng) - 0.5) * 4000.0;
            let y = a1 * y1 + a2 * y2 + noise;
            y2 = y1;
            y1 = y;
            let env = 0.6 + 0.4 * (2.0 * PI * n as f64 / (0.7 * rate as f64)).sin();
            let v = y * env;
            peak = peak.max(v.abs());
            out.push(v);
        }
        let sc = 8000.0 / peak.max(1.0);
        out.iter().map(|&v| (v * sc).round() as i64).collect()
    }

    /// Stereo pair: ch1 = 0.7*ch0 + 0.3*independent.
    pub(crate) fn stereo(seed: u64, rate: u32, frames: usize) -> Vec<Vec<i64>> {
        let c0 = resonator(seed, rate, frames);
        let ind = resonator(seed ^ 0x5EED5EED, rate, frames);
        let c1 = c0
            .iter()
            .zip(&ind)
            .map(|(&a, &b)| (0.7 * a as f64 + 0.3 * b as f64).round() as i64)
            .collect();
        vec![c0, c1]
    }


    fn input(name: &str, bytes: Vec<u8>) -> Input {
        Input { name: name.to_string(), bytes }
    }

    fn roundtrip_check(names: &[&str], wavs: &[Vec<u8>]) -> (Vec<u8>, Stats) {
        let inputs: Vec<Input> = names
            .iter()
            .zip(wavs.iter())
            .map(|(n, b)| input(n, b.clone()))
            .collect();
        let (bytes, stats) = encode(&inputs).unwrap();
        assert_eq!(stats.total_bytes, bytes.len() as u64, "total_bytes == len");
        let outs = decode(&bytes, PcmOut::Flac).unwrap();
        for (o, w) in outs.iter().zip(wavs.iter()) {
            assert_eq!(&o.bytes, w, "decoded WAV must be byte-identical");
        }
        (bytes, stats)
    }

    // ------------------------------------------------------ tests

    /// v2 container written by the pre-v3 encoder (two WAVs, track 1 joint-
    /// coded against track 0). Decode must stay byte-exact forever.
    #[test]
    fn v2_fixture_decode() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
        let flq = std::fs::read(dir.join("v2_joint.flq")).unwrap();
        let outs = decode(&flq, PcmOut::Flac).unwrap();
        assert_eq!(outs.len(), 2);
        for (o, name) in outs.iter().zip(["v2_a.wav", "v2_b.wav"]) {
            assert_eq!(o.name, name);
            assert_eq!(o.bytes, std::fs::read(dir.join(name)).unwrap(), "{name}");
        }
    }

    #[test]
    fn formats_roundtrip() {
        let rate = 48000;
        let frames_cycle = [0usize, 1, 4095, 4097, 10000];
        let mut kinds: Vec<Vec<u8>> = Vec::new();
        for ch in [1u16, 2, 3] {
            for (kind_i, fcount) in frames_cycle.iter().enumerate() {
                let case = (ch as usize) * 10 + kind_i;
                let sig = stereo(100 + case as u64, rate, *fcount);
                let sig3: Vec<Vec<i64>> = (0..ch as usize)
                    .map(|c| sig[c % 2].iter().map(|&v| v + c as i64).collect())
                    .collect();
                let w = match kind_i % 4 {
                    0 => int_wav(1, ch, rate, 8, &sig3),
                    1 => int_wav(1, ch, rate, 16, &sig3),
                    2 => int_wav(1, ch, rate, 24, &sig3),
                    _ => int_wav(1, ch, rate, 32, &sig3),
                };
                kinds.push(w);
            }
        }
        // float kinds: f32, f64 (non-exact), f64-as-f32, extensible
        let f32s: Vec<Vec<u64>> = stereo(7, rate, 4097)
            .iter()
            .map(|c| c.iter().map(|&v| u64::from(((v as f32) * 0.0001).to_bits())).collect())
            .collect();
        kinds.push(float_wav(32, 2, rate, &f32s));
        let f64s: Vec<Vec<u64>> = stereo(9, rate, 4097)
            .iter()
            .map(|c| c.iter().map(|&v| ((v as f64) * 0.0001 + 0.1).to_bits()).collect())
            .collect();
        kinds.push(float_wav(64, 2, rate, &f64s));
        let ex: Vec<Vec<u64>> = stereo(11, rate, 10000)
            .iter()
            .map(|c| c.iter().map(|&v| ((v as f64) * 0.5).to_bits()).collect())
            .collect();
        kinds.push(float_wav(64, 2, rate, &ex)); // f64-as-f32 exact
        let ext_sig = stereo(13, rate, 10000);
        kinds.push({
            let frames = ext_sig[0].len();
            let mut pcm = vec![0u8; frames * 4];
            for (f, frame) in pcm.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                for (c, s) in ext_sig.iter().enumerate() {
                    let at = c * 2;
                    frame[at..at + 2].copy_from_slice(&(s[f] as i16).to_le_bytes());
                }
            }
            riff(&[(b"fmt ", fmt_ext(2, rate, 16, 1)), (b"data", pcm)])
        });

        for (i, w) in kinds.iter().enumerate() {
            let inputs = vec![input(&format!("t{i}.wav"), w.clone())];
            let (bytes, stats) = encode(&inputs).unwrap();
            assert_eq!(stats.total_bytes, bytes.len() as u64, "total_bytes == len");
            let outs = decode(&bytes, PcmOut::Flac).unwrap();
            assert_eq!(&outs[0].bytes, w, "kind {i} must roundtrip byte-exact");
        }
    }

    #[test]
    fn identical_pair_joint() {
        let rate = 44100;
        let sig = stereo(1, rate, 100_000);
        let a = int_wav(1, 2, rate, 16, &sig);
        let b = int_wav(1, 2, rate, 16, &sig);
        let (_bytes, stats) = roundtrip_check(&["a.wav", "b.wav"], &[a, b]);
        let t1 = &stats.tracks[1];
        assert!(t1.joint, "identical track must use joint coding ({})", t1.joint_reason);
        assert!(
            t1.final_bits as f64 <= 0.02 * t1.stereo_bits as f64,
            "final {} bits vs stereo {} bits",
            t1.final_bits,
            t1.stereo_bits
        );
    }

    #[test]
    fn delayed_copy_joint() {
        let rate = 44100;
        let sig = stereo(2, rate, 60_000);
        let a = int_wav(1, 2, rate, 16, &sig);
        let delay = 10007usize;
        let bsig: Vec<Vec<i64>> = sig
            .iter()
            .map(|c| {
                (0..c.len())
                    .map(|i| {
                        if i < delay {
                            0
                        } else {
                            (0.7 * c[i - delay] as f64).round() as i64
                        }
                    })
                    .collect()
            })
            .collect();
        let b = int_wav(1, 2, rate, 16, &bsig);
        let (_bytes, stats) = roundtrip_check(&["a.wav", "b.wav"], &[a, b]);
        let t1 = &stats.tracks[1];
        assert!(t1.joint, "delayed copy must use joint coding ({})", t1.joint_reason);
        assert!(
            t1.final_bits as f64 <= 0.30 * t1.stereo_bits as f64,
            "final {} vs stereo {}",
            t1.final_bits,
            t1.stereo_bits
        );
    }

    #[test]
    fn mix_pair_joint() {
        let rate = 44100;
        let a_sig = stereo(3, rate, 40_000);
        let c_sig = stereo(77, rate, 40_000);
        let b_sig: Vec<Vec<i64>> = a_sig
            .iter()
            .zip(&c_sig)
            .map(|(a, c)| a.iter().zip(c).map(|(&x, &y)| x + y).collect())
            .collect();
        let a = int_wav(1, 2, rate, 16, &a_sig);
        let b = int_wav(1, 2, rate, 16, &b_sig);
        let c = int_wav(1, 2, rate, 16, &c_sig);
        let (_bytes, stats) = roundtrip_check(&["a.wav", "b.wav"], &[a, b]);
        let t1 = &stats.tracks[1];
        let (_cbytes, cstats) = roundtrip_check(&["c.wav"], &[c]);
        let c_alone = cstats.tracks[0].final_bits;
        assert!(t1.joint, "mix must use joint coding ({})", t1.joint_reason);
        assert!(t1.final_bits < t1.stereo_bits, "final < stereo");
        assert!(
            t1.final_bits as f64 <= 1.10 * c_alone as f64,
            "final(B) {} <= 1.10 * final(C) {}",
            t1.final_bits,
            c_alone
        );
    }

    #[test]
    fn no_joint_skips_self_refs() {
        // a.wav whose second half replays the first half: self-ref blocks
        // appear with joint analysis on, vanish with --no-joint.
        let rate = 44100;
        let sig = stereo(3, rate, 40_000);
        let mut half = sig.clone();
        for c in half.iter_mut() {
            let (a, b) = c.split_at_mut(20_000);
            b.copy_from_slice(a);
        }
        let wav = int_wav(1, 2, rate, 16, &half);
        let inputs = [input("a.wav", wav)];
        let (bytes, stats) = encode_opts(&inputs, true).unwrap();
        assert!(stats.tracks[0].self_ref_blocks > 0, "expected self-refs");
        let (bytes_nj, stats_nj) = encode_opts(&inputs, false).unwrap();
        let t = &stats_nj.tracks[0];
        assert_eq!(t.self_ref_blocks, 0);
        assert_eq!(t.cross_ref_blocks, 0);
        assert!(!t.joint);
        assert_eq!(t.final_bits, t.stereo_bits);
        let outs = decode(&bytes_nj, PcmOut::Flac).unwrap();
        assert_eq!(outs[0].bytes, inputs[0].bytes, "no-joint decode must be byte-exact");
        assert!(decode(&bytes, PcmOut::Flac).is_ok());
    }

    #[test]
    fn unrelated_pair_no_joint() {
        let rate = 44100;
        let a = int_wav(1, 2, rate, 16, &stereo(4, rate, 20_000));
        let b = int_wav(1, 2, rate, 16, &stereo(999_999, rate, 20_000));
        let (_bytes, stats) = roundtrip_check(&["a.wav", "b.wav"], &[a, b]);
        let t1 = &stats.tracks[1];
        assert!(!t1.joint, "unrelated tracks must not join ({})", t1.joint_reason);
        assert_eq!(t1.final_bits, t1.stereo_bits);
        assert!(
            t1.joint_reason.starts_with("no shared content"),
            "reason = {}",
            t1.joint_reason
        );
        assert!(
            t1.max_ncc.is_some_and(|v| v < 0.2),
            "max_ncc = {:?}",
            t1.max_ncc
        );
    }

    /// Frames just over one segment: tail merges into segment 0, so the last
    /// blocks still get their lag candidates.
    #[test]
    fn tail_segment_joint() {
        let rate = 44100;
        let a_sig = stereo(21, rate, crate::joint::SEG + 1000);
        let a = int_wav(1, 2, rate, 16, &a_sig);
        let b = int_wav(1, 2, rate, 16, &a_sig);
        let (_bytes, stats) = roundtrip_check(&["a.wav", "b.wav"], &[a, b]);
        let t1 = &stats.tracks[1];
        assert!(t1.joint, "tail segment must join ({})", t1.joint_reason);
        assert_eq!(t1.self_ref_blocks + t1.cross_ref_blocks, t1.blocks);
    }

    #[test]
    fn rate_mismatch_reason() {
        let a = int_wav(1, 2, 44100, 16, &stereo(5, 44100, 5000));
        let b = int_wav(1, 2, 48000, 16, &stereo(5, 48000, 5000));
        let (_bytes, stats) = roundtrip_check(&["a.wav", "b.wav"], &[a, b]);
        assert!(
            stats.tracks[1].joint_reason.starts_with("different sample rates"),
            "reason = {}",
            stats.tracks[1].joint_reason
        );
    }

    #[test]
    fn corrupt_inputs() {
        let sig = stereo(6, 44100, 9000);
        let a = int_wav(1, 2, 44100, 16, &sig);
        let b = int_wav(1, 2, 44100, 16, &sig);
        let inputs = vec![input("a.wav", a), input("b.wav", b)];
        let (bytes, _stats) = encode(&inputs).unwrap();

        // flipped payload byte
        let mut bad = bytes.clone();
        let mid = bad.len() - 20;
        bad[mid] ^= 0xFF;
        assert!(decode(&bad, PcmOut::Flac).is_err());
        // truncated
        assert!(decode(&bytes[..bytes.len() - 10], PcmOut::Flac).is_err());
        // bad name
        let inputs = vec![input("../x.wav", int_wav(1, 1, 8000, 16, &[vec![0i64; 4]]))];
        assert!(encode(&inputs).is_err());
    }

    // ------------------------------------------------------ mp3 (v2)

    fn small_mp3(seed: u64) -> Vec<u8> {
        let mut rng = crate::mp3::tests::Lcg::new(seed);
        crate::mp3::tests::synth_mp3(&mut rng, 24).0
    }

    #[test]
    fn mp3_mixes_roundtrip() {
        let m1 = small_mp3(11);
        let m2 = small_mp3(22);
        let w = int_wav(1, 2, 44100, 16, &stereo(5, 44100, 9000));

        for (names, files, kinds) in [
            (
                vec!["a.mp3", "b.mp3"],
                vec![m1.clone(), m2.clone()],
                vec![TrackKind::Mp3, TrackKind::Mp3],
            ),
            (
                vec!["a.wav", "b.mp3"],
                vec![w.clone(), m1.clone()],
                vec![TrackKind::Wav, TrackKind::Mp3],
            ),
            (
                vec!["a.mp3", "b.wav"],
                vec![m1.clone(), w.clone()],
                vec![TrackKind::Mp3, TrackKind::Wav],
            ),
        ] {
            let inputs: Vec<Input> = names
                .iter()
                .zip(&files)
                .map(|(n, b)| input(n, b.clone()))
                .collect();
            let (bytes, stats) = encode(&inputs).unwrap();
            assert_eq!(stats.total_bytes, bytes.len() as u64);
            let outs = decode(&bytes, PcmOut::Flac).unwrap();
            for (o, f) in outs.iter().zip(&files) {
                assert_eq!(&o.bytes, f, "{} must be byte-identical", o.name);
            }
            for (t, &k) in stats.tracks.iter().zip(&kinds) {
                assert_eq!(t.kind, k);
            }
            for t in &stats.tracks[1..] {
                assert_eq!(t.joint_reason, "mp3 track (no joint coding)");
            }
        }
        // MP3 stats exposed on the mp3 track; modeled mode must win on synth
        let inputs = vec![input("a.mp3", m1.clone()), input("b.wav", w.clone())];
        let (_, stats) = encode(&inputs).unwrap();
        let t0 = &stats.tracks[0];
        assert_eq!(t0.kind, TrackKind::Mp3);
        let m = t0.mp3.as_ref().unwrap();
        assert_eq!(m.frames, 24);
        assert_eq!(t0.frames, 1152 * 24);
        assert_eq!(m.mode, 1, "modeled payload must beat raw on synth mp3");
        assert_eq!(t0.final_bits, m.payload_bytes * 8);
        assert!(m.payload_bytes < m1.len() as u64);
        assert_eq!(t0.joint_reason, "mp3 track (no joint coding)");
        let t1 = &stats.tracks[1];
        assert_eq!(t1.kind, TrackKind::Wav);
        assert_eq!(t1.joint_reason, "mp3 track (no joint coding)");
    }

    #[test]
    fn sniff_rejects_non_audio() {
        match encode(&[input("x.bin", b"hello world".to_vec())]) {
            Err(e) => assert_eq!(e, formats::UNSUPPORTED_MSG),
            Ok(_) => panic!("non-audio input must be rejected"),
        }
    }

    #[test]
    fn v1_format_still_decodes() {
        // the v3 encoder can't be downgraded (metas live in a deflated
        // section, payloads are per-track), so build the v1 file by hand:
        // kindless metas + one shared PCM bitstream.
        let w = int_wav(1, 2, 44100, 16, &stereo(3, 44100, 8000));
        let w2 = int_wav(1, 1, 48000, 16, &stereo(4, 48000, 8000)[..1]);
        let mut v1 = b"FLQ1".to_vec();
        v1.push(1);
        v1.push(2);
        v1.extend((BLOCK as u32).to_le_bytes());
        let mut bw = BitWriter::new();
        for (name, bytes) in [("a.wav", &w), ("b.wav", &w2)] {
            let t = to_track(
                &input(name, bytes.clone()),
                wav::parse(bytes).unwrap(),
                TrackKind::Wav,
                "wav",
            );
            v1.extend((name.len() as u16).to_le_bytes());
            v1.extend(name.as_bytes());
            v1.extend((t.prefix.len() as u32).to_le_bytes());
            v1.extend(&t.prefix);
            v1.extend((t.suffix.len() as u32).to_le_bytes());
            v1.extend(&t.suffix);
            v1.extend(t.frames.to_le_bytes());
            v1.extend(t.crc.to_le_bytes());
            v1.push(t.float_mode);
            v1.extend(t.e.to_le_bytes());
            v1.push(0); // joint flag
            let ctx = TrackCtx {
                x: &t.x,
                d: t.d.as_deref(),
                float_word_bits: t.stream_word_bits,
                sources: &[],
            };
            let plan = codec::plan_track(&ctx);
            codec::write_track(&mut bw, &ctx, &plan, 0);
        }
        let payload = bw.into_bytes();
        v1.extend((payload.len() as u64).to_le_bytes());
        v1.extend(&payload);
        let outs = decode(&v1, PcmOut::Flac).unwrap();
        assert_eq!(outs[0].bytes, w);
        assert_eq!(outs[1].bytes, w2);
    }

    #[test]
    #[ignore]
    fn v1_files_compat() {
        let dir = env!("CARGO_MANIFEST_DIR");
        // originals live in testdata/ (verified byte-identical)
        let a = std::fs::read(format!("{dir}/testdata/birds_singing.wav")).unwrap();
        let b = std::fs::read(format!("{dir}/testdata/frogs_croaking.wav")).unwrap();
        let data = std::fs::read(format!("{dir}/testdata/birds_frogs.flq")).unwrap();
        let outs = decode(&data, PcmOut::Flac).unwrap();
        assert_eq!(outs.len(), 2);
        assert_eq!(outs[0].bytes, a);
        assert_eq!(outs[1].bytes, b);
        println!("birds_frogs.flq (v1) decodes byte-exact");
    }

    /// Any `testdata/*.mp3` files (not shipped — drop one in to enable).
    fn testdata_mp3s() -> Vec<std::path::PathBuf> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
        let mut v: Vec<_> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|e| e == "mp3"))
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    #[test]
    #[ignore]
    fn mp3_files_roundtrip() {
        let mp3s = testdata_mp3s();
        if mp3s.is_empty() {
            return;
        }
        for p in mp3s {
            let file = p.display().to_string();
            let b = std::fs::read(&p).unwrap();
            let f = crate::mp3::parse(&b);
            let rb = crate::mp3::rebuild(&f);
            assert_eq!(rb.len(), b.len());
            assert_eq!(rb, b, "{file}: rebuild not byte-exact");
            let s = crate::mp3::stats(&f);
            println!("{file} ({} B)\n  {s:#?}", b.len());
            assert_eq!(s.unparsed_grch, 0, "{file}: unparsed gr/ch");
            assert_eq!(s.invalid_frames, 0, "{file}: invalid frames");
            let (flq, st) = encode(&[input("song.mp3", b.clone())]).unwrap();
            let m = st.tracks[0].mp3.as_ref().unwrap();
            println!(
                "  mode={} payload={} B ({} B raw), coded {:?}",
                m.mode, m.payload_bytes, b.len(), m.coded
            );
            assert_eq!(decode(&flq, PcmOut::Flac).unwrap()[0].bytes, b, "{file}: container");
        }
    }

    #[test]
    #[ignore]
    fn real_files_roundtrip() {
        let dir = env!("CARGO_MANIFEST_DIR");
        // originals live in testdata/ (verified byte-identical)
        let a = std::fs::read(format!("{dir}/testdata/birds_singing.wav")).unwrap();
        let b = std::fs::read(format!("{dir}/testdata/frogs_croaking.wav")).unwrap();
        let inputs = vec![
            input("birds_singing.wav", a.clone()),
            input("frogs_croaking.wav", b.clone()),
        ];
        let t0 = std::time::Instant::now();
        let (bytes, stats) = encode(&inputs).unwrap();
        let enc = t0.elapsed();
        let t1 = std::time::Instant::now();
        let outs = decode(&bytes, PcmOut::Flac).unwrap();
        let dec = t1.elapsed();
        for t in &stats.tracks {
            println!(
                "{}: independent={}B stereo={}B final={}B float={}B reason={:?} max_ncc={:?} joint={}",
                t.name,
                t.independent_bits / 8,
                t.stereo_bits / 8,
                t.final_bits / 8,
                t.float_bits / 8,
                t.joint_reason,
                t.max_ncc,
                t.joint
            );
        }
        println!(
            "total {}B  encode {:?}  decode {:?}",
            bytes.len(),
            enc,
            dec
        );
        assert_eq!(outs[0].bytes, a, "birds must be byte-exact");
        assert_eq!(outs[1].bytes, b, "frogs must be byte-exact");
    }

    // ---------------------------------------------------- kinds 2-4 (formats)

    fn aiff_fixture(ch: u16, rate: u32, bits: u16, x: &[Vec<i64>]) -> Vec<u8> {
        let mut b = aiff::tests::AiffBuilder::new(ch, rate, bits, x[0].len() as u32);
        b.sound = aiff::tests::be_int_pcm(x, bits);
        b.build()
    }

    /// Pcm-kind accuracy: output FLAC re-decodes to the input samples.
    fn pcm_exact(orig: &[u8], out: &Output) {
        let (c0, r0, b0, x0) = decode_pcm(orig).unwrap();
        let (c1, r1, b1, x1) = decode_pcm(&out.bytes).unwrap();
        assert_eq!((c0, r0, b0), (c1, r1, b1), "{} params", out.name);
        assert_eq!(x0, x1, "{} samples", out.name);
        let t0 = tag_blocks(orig).unwrap();
        let t1 = tag_blocks(&out.bytes).unwrap();
        assert_eq!(t0, t1, "{} tag blocks", out.name);
    }

    #[test]
    fn aiff_roundtrip() {
        let rate = 44100;
        for bits in [8u16, 16, 24, 32] {
            let (lo, hi) = (-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1);
            let sig: Vec<Vec<i64>> = stereo(7 + u64::from(bits), rate, 5000)
                .iter()
                .map(|c| c.iter().map(|&v| v.clamp(lo, hi)).collect())
                .collect();
            let aif = aiff_fixture(2, rate, bits, &sig);
            let inputs = vec![input("a.aiff", aif.clone())];
            let (bytes, stats) = encode(&inputs).unwrap();
            assert_eq!(stats.total_bytes, bytes.len() as u64);
            assert_eq!(stats.tracks[0].kind, TrackKind::Aiff);
            assert_eq!(stats.tracks[0].format, "aiff");
            let outs = decode(&bytes, PcmOut::Flac).unwrap();
            assert_eq!(outs[0].bytes, aif, "bits {bits} must be byte-exact");
            assert_eq!(outs[0].name, "a.aiff");
        }
    }

    #[test]
    fn aiff_wav_joint() {
        let rate = 44100;
        let sig = stereo(23, rate, 8000);
        let aif = aiff_fixture(2, rate, 16, &sig);
        let w = int_wav(1, 2, rate, 16, &sig);
        let inputs = vec![input("a.wav", w), input("b.aiff", aif)];
        let (bytes, stats) = encode(&inputs).unwrap();
        assert_eq!(stats.tracks[0].kind, TrackKind::Wav);
        assert_eq!(stats.tracks[1].kind, TrackKind::Aiff);
        assert!(stats.tracks[1].joint, "aiff copy of wav must joint-code");
        let outs = decode(&bytes, PcmOut::Flac).unwrap();
        assert_eq!(outs[0].bytes, inputs[0].bytes);
        assert_eq!(outs[1].bytes, inputs[1].bytes);
    }

    #[test]
    fn aifc_ulaw_raw() {
        let mut b = aiff::tests::AiffBuilder::new(1, 8000, 8, 16);
        b.aifc = true;
        b.compression = *b"ulaw";
        b.sound = vec![0x5Au8; 16];
        let file = b.build();
        let inputs = vec![input("u.aifc", file.clone())];
        let (bytes, stats) = encode(&inputs).unwrap();
        assert_eq!(stats.tracks[0].kind, TrackKind::Raw);
        assert_eq!(stats.tracks[0].format, "aiff");
        let outs = decode(&bytes, PcmOut::Flac).unwrap();
        assert_eq!(outs[0].bytes, file);
    }

    #[test]
    fn flac_pcm_roundtrip() {
        let rate = 44100;
        for (bits, ch) in [(16u16, 2usize), (24, 2), (8, 1)] {
            let (lo, hi) = (-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1);
            let sig: Vec<Vec<i64>> = stereo(41 + u64::from(bits), rate, 4000)
                .iter()
                .map(|c| c.iter().map(|&v| v.clamp(lo, hi)).collect())
                .collect();
            let flac = formats::tests::flac_fixture(&sig[..ch.min(sig.len())], bits, rate);
            let inputs = vec![input("s.flac", flac.clone())];
            let (bytes, stats) = encode(&inputs).unwrap();
            assert_eq!(stats.tracks[0].kind, TrackKind::Pcm);
            assert_eq!(stats.tracks[0].format, "flac");
            // FLAC output: re-decodes to identical samples + tags
            let outs = decode(&bytes, PcmOut::Flac).unwrap();
            assert_eq!(outs[0].name, "s.flac");
            assert!(!outs[0].exact);
            pcm_exact(&flac, &outs[0]);
            // WAV output: parses, identical samples
            let outs = decode(&bytes, PcmOut::Wav).unwrap();
            assert_eq!(outs[0].name, "s.wav");
            let w = wav::parse(&outs[0].bytes).unwrap();
            assert_eq!((w.channels as usize, w.rate, w.bits), (ch, rate, bits));
            assert_eq!(wav::channel_ints(&w), sig[..ch.min(sig.len())].to_vec());
        }
    }

    #[test]
    fn flac_wav_joint() {
        let rate = 44100;
        let sig = stereo(19, rate, 8000);
        let flac = formats::tests::flac_fixture(&sig, 16, rate);
        let w = int_wav(1, 2, rate, 16, &sig);
        let inputs = vec![input("a.wav", w), input("b.flac", flac)];
        let (_, stats) = encode(&inputs).unwrap();
        assert_eq!(stats.tracks[1].kind, TrackKind::Pcm);
        assert!(stats.tracks[1].joint, "flac copy of wav must joint-code");
    }

    #[test]
    fn flac_size_fallback() {
        let rate = 44100;
        // full-scale white noise: FLQ is leaner than flacenc's framing here, so
        // it legitimately stores PCM — assert it does so audio-exactly
        let mut rng = 0x9E3779B97F4A7C15u64;
        let sig: Vec<Vec<i64>> = (0..2)
            .map(|_| {
                (0..4096)
                    .map(|_| {
                        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                        (rng >> 48) as u16 as i16 as i64
                    })
                    .collect()
            })
            .collect();
        let flac = formats::tests::flac_fixture(&sig, 16, rate);
        let inputs = vec![input("n.flac", flac.clone())];
        let (bytes, stats) = encode(&inputs).unwrap();
        let outs = decode(&bytes, PcmOut::Flac).unwrap();
        match stats.tracks[0].kind {
            TrackKind::Raw => assert_eq!(outs[0].bytes, flac, "raw fallback byte-exact"),
            TrackKind::Pcm => pcm_exact(&flac, &outs[0]),
            k => panic!("unexpected kind {k:?}"),
        }
        // constant signal: FLAC codes constant subframes (~200 B file) that FLQ
        // cannot match — the size rule must fall back to raw, byte-exact
        let sig = vec![vec![-1234i64; 4096], vec![777i64; 4096]];
        let flac = formats::tests::flac_fixture(&sig, 16, rate);
        assert!(flac.len() < 4096, "fixture sanity: {} B", flac.len());
        let inputs = vec![input("c.flac", flac.clone())];
        let (bytes, stats) = encode(&inputs).unwrap();
        assert_eq!(stats.tracks[0].kind, TrackKind::Raw);
        assert_eq!(stats.tracks[0].format, "flac");
        assert_eq!(
            stats.tracks[0].joint_reason,
            "stored as-is (FLQ not smaller than original)"
        );
        let outs = decode(&bytes, PcmOut::Flac).unwrap();
        assert_eq!(outs[0].bytes, flac, "raw fallback must be byte-exact");
    }

    #[test]
    fn alac_pcm_roundtrip() {
        let rate = 44100;
        let sig = stereo(53, rate, 2000);
        let m4a = formats::tests::mp4_fixture(&sig, 16, rate, b"alac");
        let inputs = vec![input("a.m4a", m4a.clone())];
        let (bytes, stats) = encode(&inputs).unwrap();
        assert_eq!(stats.tracks[0].kind, TrackKind::Pcm);
        assert_eq!(stats.tracks[0].format, "alac");
        let outs = decode(&bytes, PcmOut::Flac).unwrap();
        assert_eq!(outs[0].name, "a.flac");
        pcm_exact(&m4a, &outs[0]);
        let outs = decode(&bytes, PcmOut::Wav).unwrap();
        let w = wav::parse(&outs[0].bytes).unwrap();
        assert_eq!(wav::channel_ints(&w), sig);
    }

    #[test]
    fn raw_formats_roundtrip() {
        let ogg = |payload: &[u8]| -> Vec<u8> {
            assert!(payload.len() < 255);
            let mut v = b"OggS\x00\x02".to_vec();
            v.extend([0u8; 8]);
            v.extend([0u8; 8]);
            v.extend([0u8; 4]);
            v.push(1);
            v.push(payload.len() as u8);
            v.extend(payload);
            v
        };
        let mut opus_head = b"OpusHead".to_vec();
        opus_head.extend([1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let mut vorbis_head = vec![1u8];
        vorbis_head.extend(b"vorbis");
        let adts = {
            let payload = [0x55u8; 100];
            let len = 7 + payload.len();
            let mut h = vec![0xFFu8, 0xF1, 0x40];
            h.push(0x40 | ((len >> 11) as u8));
            h.push((len >> 3) as u8);
            h.push(((len & 7) << 5) as u8 | 0x1F);
            h.push(0xFC);
            h.extend(payload);
            h
        };
        let mut adts_id3 = b"ID3\x04\x00\x00\x00\x00\x00\x05abcde".to_vec();
        adts_id3.extend(&adts);
        let mp4a = formats::tests::mp4_fixture(
            &stereo(3, 44100, 10),
            16,
            44100,
            b"mp4a",
        );
        for (name, file, fmt) in [
            ("o.ogg", ogg(&opus_head), "ogg opus"),
            ("v.ogg", ogg(&vorbis_head), "ogg vorbis"),
            ("a.aac", adts, "aac"),
            ("a2.aac", adts_id3, "aac"),
            ("m.m4a", mp4a, "m4a aac"),
        ] {
            let inputs = vec![input(name, file.clone())];
            let (bytes, stats) = encode(&inputs).unwrap();
            assert_eq!(stats.tracks[0].kind, TrackKind::Raw, "{name}");
            assert_eq!(stats.tracks[0].format, fmt, "{name}");
            assert_eq!(stats.tracks[0].joint_reason, "stored as-is (lossy format)");
            let outs = decode(&bytes, PcmOut::Flac).unwrap();
            assert_eq!(outs[0].bytes, file, "{name} must be byte-exact");
            assert_eq!(outs[0].name, name);
        }
    }

    #[test]
    fn flac_decode_failure_falls_back_to_raw() {
        let rate = 44100;
        let sig = stereo(19, rate, 8000);
        let mut flac = formats::tests::flac_fixture(&sig, 16, rate);
        // first frame byte: walk metadata blocks until the last-block flag
        let mut pos = 4usize;
        loop {
            let last = flac[pos] & 0x80 != 0;
            let len = (usize::from(flac[pos + 1]) << 16)
                | (usize::from(flac[pos + 2]) << 8)
                | usize::from(flac[pos + 3]);
            pos += 4 + len;
            if last {
                break;
            }
        }
        flac[pos + 64] ^= 0xFF; // corrupt inside frame data
        let inputs = vec![input("bad.flac", flac.clone())];
        let (bytes, stats) = encode(&inputs).unwrap();
        assert_eq!(stats.tracks[0].kind, TrackKind::Raw);
        assert!(
            stats.tracks[0].joint_reason.contains("decode failed"),
            "{}",
            stats.tracks[0].joint_reason
        );
        let outs = decode(&bytes, PcmOut::Flac).unwrap();
        assert_eq!(outs[0].bytes, flac, "raw fallback must be byte-exact");
    }

    #[test]
    fn alac_decode_failure_falls_back_to_raw() {
        let rate = 44100;
        let sig = stereo(53, rate, 2000);
        let mut m4a = formats::tests::mp4_fixture(&sig, 16, rate, b"alac");
        // corrupt the ALAC packet's num_samples field (byte 4 of the packet,
        // which starts right after the mdat atom header)
        let mdat = m4a.windows(4).position(|w| w == b"mdat").unwrap() + 4;
        m4a[mdat + 4] ^= 0xFF;
        let inputs = vec![input("bad.m4a", m4a.clone())];
        let (bytes, stats) = encode(&inputs).unwrap();
        assert_eq!(stats.tracks[0].kind, TrackKind::Raw);
        assert!(
            stats.tracks[0].joint_reason.contains("decode failed"),
            "{}",
            stats.tracks[0].joint_reason
        );
        let outs = decode(&bytes, PcmOut::Flac).unwrap();
        assert_eq!(outs[0].bytes, m4a, "raw fallback must be byte-exact");
    }

    #[test]
    fn raw_track_corrupt_payload_fails_decode() {
        let mut ogg = b"OggS\x00\x02".to_vec();
        ogg.extend([0u8; 20]);
        ogg.push(1); // one segment
        ogg.push(10); // lacing value
        ogg.extend(b"OpusHead\x01\x02");
        let inputs = vec![input("o.ogg", ogg)];
        let (mut bytes, _) = encode(&inputs).unwrap();
        // per-track payloads are written last -> the final byte is payload
        *bytes.last_mut().unwrap() ^= 0xFF;
        assert!(decode(&bytes, PcmOut::Flac).is_err());
    }

    #[test]
    fn mixed_all_kinds() {
        let rate = 44100;
        let sig = stereo(71, rate, 6000);
        let w = int_wav(1, 2, rate, 16, &sig);
        let aif = aiff_fixture(2, rate, 16, &sig);
        let flac = formats::tests::flac_fixture(&sig, 16, rate);
        let m3 = small_mp3(7);
        let ogg = {
            let mut v = b"OggS\x00\x02".to_vec();
            v.extend([0u8; 20]);
            v.push(1); // one segment
            v.push(10); // lacing value
            v.extend(b"OpusHead\x01\x02");
            v
        };
        let inputs = vec![
            input("a.wav", w.clone()),
            input("b.aiff", aif.clone()),
            input("c.flac", flac.clone()),
            input("d.mp3", m3.clone()),
            input("e.ogg", ogg.clone()),
        ];
        let (bytes, stats) = encode(&inputs).unwrap();
        assert_eq!(
            stats.tracks.iter().map(|t| t.kind).collect::<Vec<_>>(),
            vec![
                TrackKind::Wav,
                TrackKind::Aiff,
                TrackKind::Pcm,
                TrackKind::Mp3,
                TrackKind::Raw,
            ]
        );
        let outs = decode(&bytes, PcmOut::Flac).unwrap();
        assert_eq!(outs[0].bytes, w);
        assert_eq!(outs[1].bytes, aif);
        pcm_exact(&flac, &outs[2]);
        assert_eq!(outs[3].bytes, m3);
        assert_eq!(outs[4].bytes, ogg);
    }

    // ------------------------------------------------ v3: reference pools

    /// Repeat each channel's first `third` frames three times.
    fn tiled(sig: &[Vec<i64>], third: usize) -> Vec<Vec<i64>> {
        sig.iter().map(|ch| ch[..third].repeat(3)).collect()
    }

    /// Wrap a v3 meta blob + payload bytes into a container (meta stored).
    fn v3_wrap(nt: u8, meta: &[u8], payloads: &[u8]) -> Vec<u8> {
        let mut out = b"FLQ1".to_vec();
        out.push(3);
        out.push(nt);
        out.extend((BLOCK as u32).to_le_bytes());
        out.push(0); // meta stored
        out.extend((meta.len() as u32).to_le_bytes());
        out.extend((meta.len() as u32).to_le_bytes());
        out.extend(meta);
        out.extend(payloads);
        out
    }

    /// Kind-0 WAV meta in v3 layout, from a parsed Track.
    fn v3_wav_meta(t: &Track, self_ref: bool, refs: &[u8], plen: u64) -> Vec<u8> {
        let name = t.name.as_bytes();
        let mut m = vec![0u8];
        m.extend((name.len() as u16).to_le_bytes());
        m.extend(name);
        m.extend((t.prefix.len() as u32).to_le_bytes());
        m.extend(&t.prefix);
        m.extend((t.suffix.len() as u32).to_le_bytes());
        m.extend(&t.suffix);
        m.extend(t.frames.to_le_bytes());
        m.extend(t.crc.to_le_bytes());
        m.push(t.float_mode);
        m.extend(t.e.to_le_bytes());
        m.push(u8::from(self_ref));
        m.push(refs.len() as u8);
        m.extend(refs);
        m.extend(plen.to_le_bytes());
        m
    }

    fn wav_track(name: &str, bytes: &[u8]) -> Track {
        to_track(
            &input(name, bytes.to_vec()),
            wav::parse(bytes).unwrap(),
            TrackKind::Wav,
            "wav",
        )
    }

    /// Thirds 2+3 copy third 1: the track must reference itself.
    #[test]
    fn self_repeat_joint() {
        let rate = 44100;
        let third = 2 * BLOCK;
        for ch in [1u16, 2] {
            let base = stereo(31, rate, third);
            let sig = tiled(&base[..usize::from(ch)], third);
            let w = int_wav(1, ch, rate, 16, &sig);
            let (_b, stats) = roundtrip_check(&["a.wav"], &[w]);
            let t = &stats.tracks[0];
            assert!(
                t.self_ref_blocks >= 3,
                "copied thirds must self-reference ({})",
                t.joint_reason
            );
            assert!(t.refs.is_empty(), "no cross refs in a single track");
            assert!(
                t.final_bits * 2 < t.stereo_bits,
                "final {} vs stereo {}",
                t.final_bits,
                t.stereo_bits
            );
        }
    }

    /// Same for a float32 WAV (self-ref applies to the I streams).
    #[test]
    fn self_repeat_joint_float() {
        let rate = 44100;
        let third = 2 * BLOCK;
        let base = resonator(32, rate, third);
        let words: Vec<u64> = base
            .iter()
            .map(|&v| u64::from(((v as f32) / 8192.0).to_bits()))
            .collect();
        let w = float_wav(32, 1, rate, &[words[..third].repeat(3)]);
        let (_b, stats) = roundtrip_check(&["a.wav"], &[w]);
        let t = &stats.tracks[0];
        assert!(
            t.self_ref_blocks >= 3,
            "float track must self-reference ({})",
            t.joint_reason
        );
    }

    /// [unrelated A, B, delayed+scaled copy of B] -> track 2 references track 1.
    #[test]
    fn cross_ref_nonzero_track() {
        let rate = 44100;
        let n = 40_000;
        let a = int_wav(1, 2, rate, 16, &stereo(11, rate, n));
        let b_sig = stereo(22, rate, n);
        let b = int_wav(1, 2, rate, 16, &b_sig);
        let delay = 10_007usize;
        let c_sig: Vec<Vec<i64>> = b_sig
            .iter()
            .map(|ch| {
                (0..n)
                    .map(|i| {
                        if i < delay {
                            0
                        } else {
                            (0.6 * ch[i - delay] as f64).round() as i64
                        }
                    })
                    .collect()
            })
            .collect();
        let c = int_wav(1, 2, rate, 16, &c_sig);
        let (_b, stats) = roundtrip_check(&["a.wav", "b.wav", "c.wav"], &[a, b, c]);
        let t2 = &stats.tracks[2];
        assert_eq!(t2.refs, vec![1], "({})", t2.joint_reason);
        assert!(t2.cross_ref_blocks > 0);
    }

    /// White noise (uncorrelated at any lag, so the true match always tops
    /// the peak list — resonator signals have a comb of partial-correlation
    /// lags that can crowd out weaker real matches).
    fn noise(seed: u64, n: usize) -> Vec<i64> {
        let mut rng = seed;
        (0..n).map(|_| ((lcg(&mut rng) - 0.5) * 16000.0) as i64).collect()
    }

    /// C = first half copy of A, second half copy of B -> pool of two sources.
    #[test]
    fn cross_ref_two_sources() {
        let rate = 44100;
        let n = 40_000;
        let a_sig = noise(11, n);
        let b_sig = noise(22, n);
        let c_sig: Vec<i64> = (0..n)
            .map(|i| if i < n / 2 { a_sig[i] } else { b_sig[i] })
            .collect();
        let a = int_wav(1, 1, rate, 16, &[a_sig]);
        let b = int_wav(1, 1, rate, 16, &[b_sig]);
        let c = int_wav(1, 1, rate, 16, &[c_sig]);
        let (_b, stats) = roundtrip_check(&["a.wav", "b.wav", "c.wav"], &[a, b, c]);
        assert_eq!(
            stats.tracks[2].refs,
            vec![0, 1],
            "({})",
            stats.tracks[2].joint_reason
        );
    }

    /// > 4 eligible earlier tracks, only one shares content -> it is selected.
    #[test]
    fn cross_ref_screening() {
        let rate = 44100;
        let n = 20_000;
        let sigs: Vec<Vec<i64>> = (0..5).map(|i| resonator(100 + i, rate, n)).collect();
        let mut wavs: Vec<Vec<u8>> = sigs
            .iter()
            .map(|s| int_wav(1, 1, rate, 16, std::slice::from_ref(s)))
            .collect();
        // track 5 = half-gain copy of track 3 only
        wavs.push(int_wav(
            1,
            1,
            rate,
            16,
            &[sigs[3].iter().map(|&v| (v as f64 * 0.5).round() as i64).collect()],
        ));
        let names = ["t0.wav", "t1.wav", "t2.wav", "t3.wav", "t4.wav", "t5.wav"];
        let (_b, stats) = roundtrip_check(&names, &wavs);
        assert_eq!(
            stats.tracks[5].refs,
            vec![3],
            "({})",
            stats.tracks[5].joint_reason
        );
    }

    /// Identical large chunks across tracks -> meta deflate; random -> stored.
    #[test]
    fn meta_deflate() {
        let rate = 44100;
        let n = 2000usize;
        let mut inputs = Vec::new();
        let mut wavs = Vec::new();
        for i in 0..20u64 {
            let pcm: Vec<u8> = resonator(i, rate, n)
                .iter()
                .flat_map(|&v| (v as i16).to_le_bytes())
                .collect();
            let w = riff(&[
                (b"fmt ", fmt16(1, 1, rate, 16)),
                (b"LIST", vec![b'X'; 4096]),
                (b"data", pcm),
            ]);
            inputs.push(input(&format!("t{i}.wav"), w.clone()));
            wavs.push(w);
        }
        let (bytes, stats) = encode(&inputs).unwrap();
        assert_eq!(stats.total_bytes, bytes.len() as u64);
        assert_eq!(bytes[10], 1, "repetitive metas must deflate");
        let raw_len = u32::from_le_bytes(bytes[11..15].try_into().unwrap()) as usize;
        let mlen = u32::from_le_bytes(bytes[15..19].try_into().unwrap()) as usize;
        assert!(mlen * 4 < raw_len, "deflate must shrink metas ({mlen} vs {raw_len})");
        let outs = decode(&bytes, PcmOut::Flac).unwrap();
        for (o, w) in outs.iter().zip(&wavs) {
            assert_eq!(&o.bytes, w);
        }

        // stored-meta (mode 0) path: wrap a real meta + payload by hand
        let mp3b = small_mp3(3);
        let (bytes, _) = encode(&[input("a.mp3", mp3b.clone())]).unwrap();
        let raw_len = u32::from_le_bytes(bytes[11..15].try_into().unwrap()) as usize;
        let mlen = u32::from_le_bytes(bytes[15..19].try_into().unwrap()) as usize;
        let meta = if bytes[10] == 1 {
            miniz_oxide::inflate::decompress_to_vec_with_limit(&bytes[19..19 + mlen], raw_len)
                .unwrap()
        } else {
            bytes[19..19 + mlen].to_vec()
        };
        let f = v3_wrap(1, &meta, &bytes[19 + mlen..]);
        let outs = decode(&f, PcmOut::Flac).unwrap();
        assert_eq!(outs[0].bytes, mp3b);
    }

    // ------------------------------------------------ v3: mp3 chain (mode 2)

    /// v3 kind-1 MP3 meta, matching the encoder's layout.
    fn v3_mp3_meta(name: &str, len: u64, crc: u32, mode: u8, plen: u64) -> Vec<u8> {
        let mut m = vec![1u8];
        m.extend((name.len() as u16).to_le_bytes());
        m.extend(name.as_bytes());
        m.extend(len.to_le_bytes());
        m.extend(crc.to_le_bytes());
        m.push(mode);
        m.extend(plen.to_le_bytes());
        m
    }

    /// Two identical modeled MP3s: the second chains into the shared model
    /// (mode 2) and beats what a fresh model (mode 1) would give.
    #[test]
    fn mp3_mode2_chain() {
        let m1 = small_mp3(11);
        let (_, solo) = encode(&[input("a.mp3", m1.clone())]).unwrap();
        let solo_size = solo.tracks[0].mp3.as_ref().unwrap().payload_bytes;

        let inputs = vec![input("a.mp3", m1.clone()), input("b.mp3", m1.clone())];
        let (bytes, stats) = encode(&inputs).unwrap();
        let s0 = stats.tracks[0].mp3.as_ref().unwrap();
        let s1 = stats.tracks[1].mp3.as_ref().unwrap();
        assert_eq!(s0.mode, 1, "first modeled track starts fresh");
        assert_eq!(s1.mode, 2, "identical track must chain");
        assert!(
            s1.payload_bytes < solo_size,
            "chained {} should beat fresh {}",
            s1.payload_bytes,
            solo_size
        );
        let outs = decode(&bytes, PcmOut::Flac).unwrap();
        assert_eq!(outs[0].bytes, m1);
        assert_eq!(outs[1].bytes, m1, "mode-2 track must decode byte-exact");
    }

    /// [modeled mp3, mp3 stored raw, same modeled mp3]: the chain carry
    /// survives the mode-0 gap and the third track still chains off the
    /// first.
    #[test]
    fn mp3_chain_across_raw() {
        let m1 = small_mp3(11);
        // sniffs as MP3 (ID3v2) but contains no frames -> all-raw items ->
        // the modeled payload can never beat storing -> mode 0
        let mut rng = crate::mp3::tests::Lcg::new(5);
        let n = 4000usize;
        let mut junk = b"ID3\x04\x00\x00".to_vec();
        junk.extend([
            ((n >> 21) & 0x7f) as u8,
            ((n >> 14) & 0x7f) as u8,
            ((n >> 7) & 0x7f) as u8,
            (n & 0x7f) as u8,
        ]);
        junk.extend((0..n).map(|_| rng.range(0, 255) as u8)); // no 0xFF syncs

        let inputs = vec![
            input("a.mp3", m1.clone()),
            input("junk.mp3", junk.clone()),
            input("c.mp3", m1.clone()),
        ];
        let (bytes, stats) = encode(&inputs).unwrap();
        let modes: Vec<u8> = stats
            .tracks
            .iter()
            .map(|t| t.mp3.as_ref().unwrap().mode)
            .collect();
        assert_eq!(modes, vec![1, 0, 2], "carry must span a raw mp3 track");
        let outs = decode(&bytes, PcmOut::Flac).unwrap();
        assert_eq!(outs[0].bytes, m1);
        assert_eq!(outs[1].bytes, junk);
        assert_eq!(outs[2].bytes, m1);
    }

    /// Hand-crafted v3 files whose mode-2 chain is broken must fail cleanly.
    #[test]
    fn mp3_mode2_bad_flag() {
        let mp3b = small_mp3(7);
        let crc = crc32(&mp3b);
        let f = mp3::parse(&mp3b);
        let (p1, _) = mp3::model::compress(&f); // flag = LIMIT | MIX

        // mode 2 as the very first track: nothing carried yet
        let meta = v3_mp3_meta("a.mp3", mp3b.len() as u64, crc, 2, p1.len() as u64);
        let file = v3_wrap(1, &meta, &p1);
        let e = decode(&file, PcmOut::Flac).err().unwrap();
        assert!(e.contains("without earlier modeled"), "unexpected error: {e}");

        // mode 1 then mode 2 with an incompatible flag (mixer bit cleared):
        // the carried model's (limit, mixer) no longer match the payload's
        let mut p2 = p1.clone();
        p2[0] &= !0x80;
        let mut meta = v3_mp3_meta("a.mp3", mp3b.len() as u64, crc, 1, p1.len() as u64);
        meta.extend(v3_mp3_meta("b.mp3", mp3b.len() as u64, crc, 2, p2.len() as u64));
        let mut payloads = p1.clone();
        payloads.extend(&p2);
        let file = v3_wrap(2, &meta, &payloads);
        let e = decode(&file, PcmOut::Flac).err().unwrap();
        assert!(e.contains("flag"), "unexpected error: {e}");
    }

    /// v2 metas must keep rejecting mode 2 (the chain exists only in v3).
    #[test]
    fn v2_rejects_mode2() {
        let mp3b = small_mp3(3);
        let mut v2 = b"FLQ1".to_vec();
        v2.push(2);
        v2.push(1);
        v2.extend((BLOCK as u32).to_le_bytes());
        v2.push(1); // kind: MP3
        v2.extend(5u16.to_le_bytes());
        v2.extend(b"a.mp3");
        v2.extend((mp3b.len() as u64).to_le_bytes());
        v2.extend(crc32(&mp3b).to_le_bytes());
        v2.push(2); // mode 2 — illegal before v3
        v2.extend(0u64.to_le_bytes()); // empty shared bitstream
        v2.extend((mp3b.len() as u64).to_le_bytes());
        v2.extend(&mp3b);
        let e = decode(&v2, PcmOut::Flac).err().unwrap();
        assert!(e.contains("mode"), "unexpected error: {e}");
    }

    // ------------------------------------------------ --only subset decode

    /// v3 multi-track with a cross reference: `--only` on the dependent
    /// track returns exactly its bytes while the reference is decoded but
    /// not emitted.
    #[test]
    fn only_subset_v3() {
        let rate = 44100;
        let n = 40_000;
        let a = int_wav(1, 2, rate, 16, &stereo(11, rate, n));
        let b_sig = stereo(22, rate, n);
        let b = int_wav(1, 2, rate, 16, &b_sig);
        let delay = 10_007usize;
        let c_sig: Vec<Vec<i64>> = b_sig
            .iter()
            .map(|ch| {
                (0..n)
                    .map(|i| {
                        if i < delay {
                            0
                        } else {
                            (0.6 * ch[i - delay] as f64).round() as i64
                        }
                    })
                    .collect()
            })
            .collect();
        let c = int_wav(1, 2, rate, 16, &c_sig);
        let inputs = vec![
            input("a.wav", a.clone()),
            input("b.wav", b.clone()),
            input("c.wav", c.clone()),
        ];
        let (bytes, stats) = encode(&inputs).unwrap();
        assert_eq!(stats.tracks[2].refs, vec![1], "c must reference b");
        let full = decode(&bytes, PcmOut::Flac).unwrap();

        let names = |s: &[&str]| s.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        // dependent track only: identical bytes to the full decode
        let sub = decode_subset(&bytes, PcmOut::Flac, &names(&["c.wav"])).unwrap();
        assert_eq!(sub.len(), 1);
        assert_eq!(sub[0].name, "c.wav");
        assert_eq!(sub[0].bytes, full[2].bytes);
        assert_eq!(sub[0].bytes, c);
        // reference track alone
        let sub = decode_subset(&bytes, PcmOut::Flac, &names(&["b.wav"])).unwrap();
        assert_eq!(sub.len(), 1);
        assert_eq!(sub[0].bytes, b);
        // several names come out in track order; b is still not emitted
        let sub =
            decode_subset(&bytes, PcmOut::Flac, &names(&["a.wav", "c.wav"])).unwrap();
        assert_eq!(sub.len(), 2);
        assert_eq!(sub[0].bytes, a);
        assert_eq!(sub[1].bytes, c);
        // unknown name -> error listing the tracks
        let e = decode_subset(&bytes, PcmOut::Flac, &names(&["zz.wav"])).err().unwrap();
        assert!(e.contains("zz.wav") && e.contains("a.wav"), "{e}");
    }

    /// `--only` on a chained (mode-2) MP3 decodes the earlier modeled
    /// payloads to advance the shared model, then returns byte-exact bytes.
    #[test]
    fn only_mp3_mode2() {
        let m1 = small_mp3(11);
        let w = int_wav(1, 2, 44100, 16, &stereo(5, 44100, 9000));
        let inputs = vec![
            input("a.mp3", m1.clone()),
            input("b.mp3", m1.clone()),
            input("c.wav", w),
        ];
        let (bytes, stats) = encode(&inputs).unwrap();
        assert_eq!(stats.tracks[1].mp3.as_ref().unwrap().mode, 2);
        let names = |s: &[&str]| s.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        let sub = decode_subset(&bytes, PcmOut::Flac, &names(&["b.mp3"])).unwrap();
        assert_eq!(sub.len(), 1);
        assert_eq!(sub[0].name, "b.mp3");
        assert_eq!(sub[0].bytes, m1, "mode-2 subset must be byte-exact");
    }

    /// v1/v2 files have one sequential bitstream: `--only` reads
    /// everything but emits just the requested tracks.
    #[test]
    fn only_v2_fixture() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
        let flq = std::fs::read(dir.join("v2_joint.flq")).unwrap();
        let sub =
            decode_subset(&flq, PcmOut::Flac, &["v2_b.wav".to_string()]).unwrap();
        assert_eq!(sub.len(), 1);
        assert_eq!(sub[0].name, "v2_b.wav");
        // v2_b is joint-coded against v2_a: byte-exact through the closure
        assert_eq!(sub[0].bytes, std::fs::read(dir.join("v2_b.wav")).unwrap());
        let sub =
            decode_subset(&flq, PcmOut::Flac, &["v2_a.wav".to_string()]).unwrap();
        assert_eq!(sub.len(), 1);
        assert_eq!(sub[0].bytes, std::fs::read(dir.join("v2_a.wav")).unwrap());
    }

    /// Peak-heap estimate: resident-set max over the planning order plus
    /// the bounded analysis working set.
    #[test]
    fn heap_estimate_fn() {
        const WS: u64 = 3 * (1 << 21) * 16;
        assert_eq!(est_encode_heap(&[], &[]), WS);
        // last_use [0,2,2]: alive sets 600 / 500 / 500, own-x extras
        // 150 / 300 / 450 -> peak 950 + working set
        assert_eq!(est_encode_heap(&[100, 200, 300], &[0, 2, 2]), 950 + WS);
        // single big track: only itself resident + ~1.5x planning overhead
        assert_eq!(est_encode_heap(&[1000], &[0]), 2500 + WS);
    }

    /// Malformed v3 files must produce clean errors, never panics.
    #[test]
    fn v3_malformed() {
        let rate = 44100;
        let w = int_wav(1, 1, rate, 16, &[resonator(1, rate, 9000)]);
        let t = wav_track("a.wav", &w);
        let ctx = TrackCtx {
            x: &t.x,
            d: None,
            float_word_bits: 0,
            sources: &[],
        };
        let plan = codec::plan_track(&ctx);
        let mut bw = BitWriter::new();
        codec::write_track(&mut bw, &ctx, &plan, 0);
        let payload = bw.into_bytes();

        // ref index >= own index
        let meta = v3_wav_meta(&t, false, &[0], payload.len() as u64);
        let f = v3_wrap(1, &meta, &payload);
        assert!(decode(&f, PcmOut::Flac).is_err(), "ref >= own index must fail");

        // ref to a non-PCM track (track 0 is mp3)
        let mp3b = small_mp3(7);
        let mut meta = vec![1u8];
        meta.extend(5u16.to_le_bytes());
        meta.extend(b"a.mp3");
        meta.extend((mp3b.len() as u64).to_le_bytes());
        meta.extend(crc32(&mp3b).to_le_bytes());
        meta.push(0); // mode 0 = stored
        meta.extend((mp3b.len() as u64).to_le_bytes());
        meta.extend(v3_wav_meta(&t, false, &[0], payload.len() as u64));
        let mut payloads = mp3b.clone();
        payloads.extend(&payload);
        let f = v3_wrap(2, &meta, &payloads);
        let e = decode(&f, PcmOut::Flac).err().unwrap();
        assert!(e.contains("non-audio"), "unexpected error: {e}");

        // illegal self lag (lag -1 reaches into the current block)
        let mut plan = codec::plan_track(&ctx);
        plan.blocks[0].alt = Some(codec::AltBlock {
            refs: vec![Some(joint::RefChoice {
                src: 0,
                ref_ch: 0,
                lag: -1,
                gm: 64,
                gshift: 6,
            })],
            mode: plan.blocks[0].mode,
            signals: std::mem::take(&mut plan.blocks[0].signals),
        });
        plan.joint_on = true;
        let mut bw = BitWriter::new();
        codec::write_track(&mut bw, &ctx, &plan, 0);
        let payload2 = bw.into_bytes();
        let meta = v3_wav_meta(&t, true, &[], payload2.len() as u64);
        let f = v3_wrap(1, &meta, &payload2);
        let e = decode(&f, PcmOut::Flac).err().unwrap();
        assert!(e.contains("undecoded"), "unexpected error: {e}");

        // truncated payload
        let meta = v3_wav_meta(&t, false, &[], payload.len() as u64);
        let f = v3_wrap(1, &meta, &payload[..payload.len() - 4]);
        assert!(decode(&f, PcmOut::Flac).is_err(), "short payload must fail");

        // meta_len beyond the file
        let mut f = b"FLQ1".to_vec();
        f.extend([3, 1]);
        f.extend((BLOCK as u32).to_le_bytes());
        f.push(0);
        f.extend(((meta.len() + 100) as u32).to_le_bytes());
        f.extend(((meta.len() + 100) as u32).to_le_bytes());
        f.extend(&meta);
        assert!(decode(&f, PcmOut::Flac).is_err(), "truncated meta must fail");

        // absurd meta_raw_len (absolute and ratio bounds)
        for raw_len in [600 * 1024 * 1024u32, 100_000] {
            let mut f = b"FLQ1".to_vec();
            f.extend([3, 1]);
            f.extend((BLOCK as u32).to_le_bytes());
            f.push(1); // deflate
            f.extend(raw_len.to_le_bytes());
            f.extend(4u32.to_le_bytes());
            f.extend([0u8; 4]);
            assert!(decode(&f, PcmOut::Flac).is_err(), "raw_len {raw_len} must fail");
        }
    }

    /// Crafted v3 MP3 tracks must fail fast: the decompressor bounds item
    /// counts and raw-item lengths by the actual payload size, so absurd
    /// meta `len` claims can never drive huge loops or allocations.
    #[test]
    fn v3_mp3_meta_bounds() {
        // len = u64::MAX with a ~10-byte payload
        let payload = [
            0xFFu8, // flag: LIMIT | MIX, matching the fresh model
            0xA5, 0x5A, 0x00, 0xFF, 0x11, 0xEE, 0x22, 0xDD, 0x33,
        ];
        let meta = v3_mp3_meta("a.mp3", u64::MAX, 0, 1, payload.len() as u64);
        let f = v3_wrap(1, &meta, &payload);
        assert!(decode(&f, PcmOut::Flac).is_err(), "absurd len must fail fast");

        // a real modeled payload holding one 100-byte Raw item, but the meta
        // plen truncates it to 20 B: the decoded raw len (100) then exceeds
        // the actual coded payload and must error instead of filling 100 B
        // of decoded garbage (mode 1 -> fresh model)
        let f = crate::mp3::Mp3File {
            items: vec![crate::mp3::Item::Raw(vec![0xAB; 100])],
            invalid_frames: 0,
        };
        let (p, _) = mp3::model::compress(&f);
        let meta = v3_mp3_meta("a.mp3", u64::MAX, 0, 1, 20);
        let f = v3_wrap(1, &meta, &p[..20]);
        let e = decode(&f, PcmOut::Flac).err().unwrap();
        assert!(e.contains("raw item"), "unexpected error: {e}");
    }

    /// Crafted v3 WAV metas with absurd `frames` must error before any
    /// attacker-sized allocation; a valid stream always satisfies the
    /// payload-feasibility bound, so quiet real files still decode.
    #[test]
    fn v3_frames_bounds() {
        let rate = 44100;
        let w = int_wav(1, 1, rate, 8, &[vec![0i64; 100]]);

        // hard cap: frames * ch > 2^30 channel-samples (block_align 1 slips
        // past the meta-level u32 product check)
        let mut big = wav_track("a.wav", &w);
        big.frames = u64::from(u32::MAX);
        let meta = v3_wav_meta(&big, false, &[], 4);
        let f = v3_wrap(1, &meta, &[0u8; 4]);
        let e = decode(&f, PcmOut::Flac).err().unwrap();
        assert!(e.contains("decode cap"), "unexpected error: {e}");

        // feasibility: 1M frames needs >= 256 blocks * 16 bits of payload
        let mut mid = wav_track("a.wav", &w);
        mid.frames = 1 << 20;
        let meta = v3_wav_meta(&mid, false, &[], 4);
        let f = v3_wrap(1, &meta, &[0u8; 4]);
        let e = decode(&f, PcmOut::Flac).err().unwrap();
        assert!(e.contains("exceed payload"), "unexpected error: {e}");

        // a real (silence-heavy) file still round-trips byte-exact
        let quiet = int_wav(1, 1, rate, 16, &[vec![0i64; 20_000]]);
        roundtrip_check(&["q.wav"], &[quiet]);
    }

    /// `stats.meta_bytes` must equal the true serialized meta length of
    /// each track — verified by re-parsing the v3 meta section.
    #[test]
    fn meta_bytes_golden() {
        let rate = 44100;
        let sig = stereo(71, rate, 6000);
        let w = int_wav(1, 2, rate, 16, &sig);
        let aif = aiff_fixture(2, rate, 16, &sig);
        let flac = formats::tests::flac_fixture(&sig, 16, rate);
        let m3 = small_mp3(7);
        let ogg = {
            let mut v = b"OggS\x00\x02".to_vec();
            v.extend([0u8; 20]);
            v.push(1);
            v.push(10);
            v.extend(b"OpusHead\x01\x02");
            v
        };
        let inputs = vec![
            input("a.wav", w),
            input("b.aiff", aif),
            input("c.flac", flac),
            input("d.mp3", m3),
            input("e.ogg", ogg),
        ];
        let (bytes, stats) = encode(&inputs).unwrap();
        let raw_len = u32::from_le_bytes(bytes[11..15].try_into().unwrap()) as usize;
        let mlen = u32::from_le_bytes(bytes[15..19].try_into().unwrap()) as usize;
        let meta = if bytes[10] == 1 {
            miniz_oxide::inflate::decompress_to_vec_with_limit(
                &bytes[19..19 + mlen],
                raw_len,
            )
            .unwrap()
        } else {
            bytes[19..19 + mlen].to_vec()
        };
        let mut mp = 0usize;
        for (i, t) in stats.tracks.iter().enumerate() {
            let start = mp; // meta_bytes includes the kind byte
            let kind = meta_u8(&meta, &mut mp).unwrap();
            read_one_meta(&meta, &mut mp, kind, i, true).unwrap();
            assert_eq!(
                t.meta_bytes,
                (mp - start) as u64,
                "track {} ({}) meta_bytes",
                i + 1,
                t.name
            );
        }
        assert_eq!(mp, meta.len(), "meta section fully consumed");
    }

    /// A RIFF/WAVE blob whose fmt tag is unsupported (6 = A-law) must fall
    /// back to a kind-4 raw track instead of failing the whole encode.
    #[test]
    fn wav_unsupported_fmt_raw() {
        let file = riff(&[
            (b"fmt ", fmt16(6, 1, 8000, 8)),
            (b"data", vec![0x55u8; 16]),
        ]);
        let inputs = vec![input("a.wav", file.clone())];
        let (bytes, stats) = encode(&inputs).unwrap();
        assert_eq!(stats.tracks[0].kind, TrackKind::Raw);
        assert_eq!(stats.tracks[0].format, "wav");
        assert_eq!(
            stats.tracks[0].joint_reason,
            "stored as-is (unsupported or malformed WAV)"
        );
        let outs = decode(&bytes, PcmOut::Flac).unwrap();
        assert_eq!(outs[0].bytes, file, "raw fallback must be byte-exact");
    }

    /// Track names become filenames on decode: path pieces, colons, trailing
    /// space/dot and Windows device names are all rejected.
    #[test]
    fn name_validation() {
        for good in ["a.wav", "song one.mp3", "..x", "com10.flac", ".hidden"] {
            assert!(valid_name(good), "{good} must be valid");
        }
        for bad in [
            "", ".", "..", "a/b", "a\\b", "c:x.wav", "name ", "name.", "con.wav",
            "CON", "nul", "prn.txt", "aux", "com1", "lpt9.wav",
        ] {
            assert!(!valid_name(bad), "{bad:?} must be invalid");
        }
        // and encode rejects them up front
        let w = int_wav(1, 1, 8000, 16, &[vec![0i64; 4]]);
        for bad in ["con.wav", "a:b.wav", "x."] {
            assert!(encode(&[input(bad, w.clone())]).is_err(), "{bad} must fail");
        }
    }
}
