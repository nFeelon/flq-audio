//! FLQ v1: framed audio format. Layout:
//!
//! ```text
//! header (28 B): "FLQ1" | version u8=1 | flags u8 | ntracks u8 | reserved u8
//!                | frame_len u32 | meta_len u32 | index_off u64 | header_crc u32
//! meta (meta_len B): per-track metas, last 4 bytes = crc32 of the rest
//! frames: back-to-back, ordered by frame_no ascending then track ascending
//!   sync "FQ" | track u8 | fmode u8 | e i16 | frame_no u32 | n u32
//!   | payload_len u32 | payload | crc32 u32   (crc over sync..payload)
//! index at index_off: per track nframes × u64 absolute frame offsets + crc32
//! ```
//!
//! A frame payload is the shared codec (`plan_track_with` + `write_track`)
//! applied to the frame's samples: codec history resets at frame starts and
//! carries between 4096-blocks inside a frame, so every frame is
//! independently decodable. At each frame's first block the codec's warm-up
//! path runs: order-`o` signals carry their first `o` samples as explicit
//! `wbits`-signed fields (residual placeholders 0), so a frame's cold start
//! doesn't depend on a zero history. All integers are little-endian.

pub mod dec;
pub mod enc;
#[cfg(test)]
mod tests;

use crate::bits::crc32;

pub const MAGIC: &[u8; 4] = b"FLQ1";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 28;
pub const FRAME_SYNC: [u8; 2] = [0x46, 0x51];
pub const FRAME_HDR_LEN: usize = 18; // sync through payload_len
pub const MAX_META_LEN: u64 = 1 << 26;
pub const MAX_TAGS: usize = 1 << 24;
pub const MAX_NAME: usize = 4096;
pub const MAX_CHANNELS: u8 = 16;
pub const MIN_FRAME_LEN: u32 = 4096;
pub const MAX_FRAME_LEN: u32 = 262144;

pub const ORIGIN_WAV: u8 = 0;
pub const ORIGIN_AIFF: u8 = 1;
pub const ORIGIN_FLAC: u8 = 2;
pub const ORIGIN_ALAC: u8 = 3;

pub const FMT_INT: u8 = 0;
pub const FMT_FLOAT: u8 = 1;

// ---------------------------------------------------------------- little io

pub(crate) fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
pub(crate) fn i16le(b: &[u8], at: usize) -> i16 {
    i16::from_le_bytes([b[at], b[at + 1]])
}
pub(crate) fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
pub(crate) fn u64le(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}
pub(crate) fn put_u16(v: &mut Vec<u8>, n: u16) {
    v.extend(n.to_le_bytes());
}
pub(crate) fn put_u32(v: &mut Vec<u8>, n: u32) {
    v.extend(n.to_le_bytes());
}
pub(crate) fn put_u64(v: &mut Vec<u8>, n: u64) {
    v.extend(n.to_le_bytes());
}

/// Track names become output filenames on decode: reject path pieces,
/// `:` (drive letters/ADS), trailing space/dot (Windows trims them), and
/// Windows device names — with or without an extension (`con`, `con.wav`).
pub(crate) fn valid_name(n: &str) -> bool {
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
        "NUL"
            | "CON"
            | "PRN"
            | "AUX"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

// ------------------------------------------------------------------- header

pub struct Header {
    pub ntracks: u8,
    pub frame_len: u32,
    pub meta_len: u32,
    pub index_off: u64,
}

/// Parse and validate the 28-byte header (magic, version, `flags`,
/// `reserved`, `frame_len` and the header crc); `meta_len`/`index_off`
/// bounds need the file length and are checked by the caller.
pub fn parse_header(b: &[u8]) -> Result<Header, String> {
    if b.len() < HEADER_LEN || &b[0..4] != MAGIC {
        return Err("not an FLQ file".into());
    }
    if b[4] != VERSION {
        return Err(format!("unsupported flq version {} (expected 1)", b[4]));
    }
    if u32le(b, 24) != crc32(&b[0..24]) {
        return Err("header crc mismatch".into());
    }
    if b[5] != 0 {
        return Err("unsupported flq flags".into());
    }
    if b[7] != 0 {
        return Err("flq reserved byte must be 0".into());
    }
    let frame_len = u32le(b, 8);
    if !(MIN_FRAME_LEN..=MAX_FRAME_LEN).contains(&frame_len) || frame_len % 4096 != 0 {
        return Err("frame_len must be a multiple of 4096 within 4096..=262144".into());
    }
    let ntracks = b[6];
    if ntracks == 0 {
        return Err("flq declares no tracks".into());
    }
    Ok(Header {
        ntracks,
        frame_len,
        meta_len: u32le(b, 12),
        index_off: u64le(b, 16),
    })
}

// --------------------------------------------------------------------- meta

/// One track's meta (origin/format/format fields + patched-in-place sizes).
#[derive(Clone, Debug)]
pub struct TrackInfo {
    pub name: String,
    pub origin: u8,
    pub rate: u32,
    pub channels: u8,
    pub channel_mask: u32,
    pub sample_fmt: u8, // 0 int, 1 float
    pub bits: u8,
    pub frames: u64,
    pub nframes: u32,
    pub pcm_crc: u32,
    pub tags: Vec<u8>,
}

fn valid_bits(sample_fmt: u8, bits: u8) -> bool {
    match sample_fmt {
        FMT_INT => matches!(bits, 8 | 16 | 24 | 32),
        FMT_FLOAT => matches!(bits, 32 | 64),
        _ => false,
    }
}

/// Append one track meta; returns the buffer offsets of the `frames`,
/// `nframes` and `pcm_crc` fields (patched by the encoder at finish).
pub(crate) fn put_track_meta(
    buf: &mut Vec<u8>,
    t: &TrackInfo,
) -> Result<(usize, usize, usize), String> {
    if t.name.len() > MAX_NAME {
        return Err(format!("track name exceeds {} bytes", MAX_NAME));
    }
    if t.tags.len() > MAX_TAGS {
        return Err(format!("tags exceed {} bytes", MAX_TAGS));
    }
    put_u16(buf, t.name.len() as u16);
    buf.extend(t.name.as_bytes());
    buf.push(t.origin);
    put_u32(buf, t.rate);
    buf.push(t.channels);
    put_u32(buf, t.channel_mask);
    buf.push(t.sample_fmt);
    buf.push(t.bits);
    let frames_at = buf.len();
    put_u64(buf, t.frames);
    let nframes_at = buf.len();
    put_u32(buf, t.nframes);
    let crc_at = buf.len();
    put_u32(buf, t.pcm_crc);
    put_u32(buf, t.tags.len() as u32);
    buf.extend(&t.tags);
    Ok((frames_at, nframes_at, crc_at))
}

/// Parse exactly `ntracks` track metas out of the meta blob (already
/// crc-verified by the caller; blob excludes the trailing crc).
pub fn parse_meta(meta: &[u8], ntracks: usize) -> Result<Vec<TrackInfo>, String> {
    let mut out = Vec::with_capacity(ntracks);
    let mut pos = 0usize;
    let need = |pos: usize, n: usize| -> Result<(), String> {
        if pos + n > meta.len() {
            Err("meta: truncated".into())
        } else {
            Ok(())
        }
    };
    for t in 0..ntracks {
        let err = |m: &str| format!("meta: track {t}: {m}");
        need(pos, 2)?;
        let nl = u16le(meta, pos) as usize;
        pos += 2;
        if nl > MAX_NAME {
            return Err(err("name too long"));
        }
        need(pos, nl)?;
        let name = String::from_utf8(meta[pos..pos + nl].to_vec())
            .map_err(|_| err("name is not UTF-8"))?;
        pos += nl;
        need(pos, 32)?;
        let origin = meta[pos];
        if origin > ORIGIN_ALAC {
            return Err(err("unknown origin"));
        }
        let rate = u32le(meta, pos + 1);
        if rate == 0 {
            return Err(err("rate is 0"));
        }
        let channels = meta[pos + 5];
        if channels == 0 || channels > MAX_CHANNELS {
            return Err(err("channel count out of range"));
        }
        let channel_mask = u32le(meta, pos + 6);
        let sample_fmt = meta[pos + 10];
        let bits = meta[pos + 11];
        if !valid_bits(sample_fmt, bits) {
            return Err(err("invalid sample format/bits"));
        }
        let frames = u64le(meta, pos + 12);
        let nframes = u32le(meta, pos + 20);
        let pcm_crc = u32le(meta, pos + 24);
        let tags_len = u32le(meta, pos + 28) as usize;
        pos += 32;
        if tags_len > MAX_TAGS {
            return Err(err("tags too long"));
        }
        need(pos, tags_len)?;
        let tags = meta[pos..pos + tags_len].to_vec();
        pos += tags_len;
        out.push(TrackInfo {
            name,
            origin,
            rate,
            channels,
            channel_mask,
            sample_fmt,
            bits,
            frames,
            nframes,
            pcm_crc,
            tags,
        });
    }
    if pos != meta.len() {
        return Err("meta: trailing bytes".into());
    }
    Ok(out)
}

// -------------------------------------------------------------------- frame

#[derive(Clone, Copy)]
pub struct FrameHead {
    pub track: u8,
    pub fmode: u8,
    pub e: i16,
    pub frame_no: u32,
    pub n: u32,
    pub payload_len: u32,
}

/// Parse an 18-byte frame header (sync checked); `n`/`fmode`/`track` are
/// validated against the file's metas by the caller.
pub fn parse_frame_head(b: &[u8]) -> Result<FrameHead, String> {
    if b.len() < FRAME_HDR_LEN {
        return Err("frame header truncated".into());
    }
    if b[0..2] != FRAME_SYNC {
        return Err("frame sync missing".into());
    }
    Ok(FrameHead {
        track: b[2],
        fmode: b[3],
        e: i16le(b, 4),
        frame_no: u32le(b, 6),
        n: u32le(b, 10),
        payload_len: u32le(b, 14),
    })
}

/// fmode must agree with the track's declared sample format:
/// int -> 0, f32 -> 1, f64 -> 2 (f64-as-f32) or 3 (true f64).
pub fn fmode_ok(t: &TrackInfo, fmode: u8) -> bool {
    match (t.sample_fmt, t.bits) {
        (FMT_INT, _) => fmode == 0,
        (FMT_FLOAT, 32) => fmode == 1,
        (FMT_FLOAT, 64) => fmode == 2 || fmode == 3,
        _ => false,
    }
}
