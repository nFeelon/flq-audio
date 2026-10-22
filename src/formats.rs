//! External format handling: input sniffing, symphonia decode of FLAC/ALAC,
//! FLAC metadata-block preservation, and FLAC output via flacenc.

use std::io::Cursor;

use symphonia::core::audio::{Audio, GenericAudioBufferRef};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoderOptions, well_known};
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTag, Tag, Visual};
use symphonia::default::{get_codecs, get_probe};

use crate::aiff;
use crate::wav;

// ------------------------------------------------------------------ sniffing

/// Recognized input format (checked in this order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sniff {
    Wav,
    Aiff,
    Flac,
    /// MP4 family; carries the first audio sample-entry fourcc (`alac` ->
    /// PCM path, anything else -> lossy). `[0;4]` when the walk fails.
    Mp4([u8; 4]),
    /// Recognized lossy bitstream (mp3/aac/ogg family); the label is for
    /// the rejection message — the payload is never parsed.
    Lossy(&'static str),
    Unknown,
}

pub const UNSUPPORTED_MSG: &str =
    "unsupported input (expected WAV, AIFF, FLAC or M4A (ALAC); flq is lossless only)";

fn id3v2_skip(b: &[u8]) -> usize {
    if b.len() < 10 || !b.starts_with(b"ID3") {
        return 0;
    }
    let size = (u64::from(b[6] & 0x7f) << 21)
        | (u64::from(b[7] & 0x7f) << 14)
        | (u64::from(b[8] & 0x7f) << 7)
        | u64::from(b[9] & 0x7f);
    let footer = if b[5] & 0x10 != 0 { 10 } else { 0 };
    let Some(total) = 10usize
        .checked_add(size as usize)
        .and_then(|t| t.checked_add(footer))
    else {
        return 0;
    };
    if total <= b.len() { total } else { 0 }
}

/// ADTS sync at `b`: 12-bit syncword, layer bits 00, sane sampling index.
fn adts_at(b: &[u8]) -> bool {
    b.len() >= 7 && b[0] == 0xFF && (b[1] & 0xF6) == 0xF0 && (b[2] >> 2) & 0xF != 0xF
}

/// Find a child atom inside `b`; returns its payload range.
fn find_atom<'a>(b: &'a [u8], want: &[u8; 4]) -> Option<&'a [u8]> {
    let mut pos = 0usize;
    while pos + 8 <= b.len() {
        let size = u32::from_be_bytes(b[pos..pos + 4].try_into().unwrap()) as u64;
        let id = &b[pos + 4..pos + 8];
        let (hdr, body_len) = match size {
            0 => (8u64, (b.len() - pos) as u64 - 8),
            1 => {
                if pos + 16 > b.len() {
                    return None;
                }
                let l = u64::from_be_bytes(b[pos + 8..pos + 16].try_into().unwrap());
                (16, l.saturating_sub(16))
            }
            s => (8, s.saturating_sub(8)),
        };
        let start = pos + hdr as usize;
        if id == want {
            return b.get(start..start.saturating_add(body_len as usize));
        }
        let next = pos.checked_add(hdr as usize + body_len as usize)?;
        if next <= pos {
            return None;
        }
        pos = next;
    }
    None
}

/// First audio sample-entry fourcc in moov/trak/mdia/minf/stbl/stsd
/// (prefers the trak whose hdlr is 'soun').
fn mp4_audio_entry(b: &[u8]) -> Option<[u8; 4]> {
    let moov = find_atom(b, b"moov")?;
    let mut first_any: Option<[u8; 4]> = None;
    let mut sound_entry: Option<[u8; 4]> = None;
    for_each(moov, b"trak", |trak| {
        let Some(mdia) = find_atom(trak, b"mdia") else {
            return;
        };
        let entry = find_atom(mdia, b"minf")
            .and_then(|m| find_atom(m, b"stbl"))
            .and_then(|s| find_atom(s, b"stsd"))
            .and_then(|stsd| {
                // stsd: version+flags, entry count, then [size][fourcc] entries
                if stsd.len() < 16 {
                    return None;
                }
                let mut e = [0u8; 4];
                e.copy_from_slice(&stsd[12..16]);
                Some(e)
            });
        let Some(e) = entry else { return };
        if first_any.is_none() {
            first_any = Some(e);
        }
        let soun = find_atom(mdia, b"hdlr").is_some_and(|h| h.len() >= 12 && &h[8..12] == b"soun");
        if soun && sound_entry.is_none() {
            sound_entry = Some(e);
        }
    });
    sound_entry.or(first_any)
}

// helper closure over traks; needs a mutable slot for the "found" result
fn for_each(b: &[u8], want: &[u8; 4], mut f: impl FnMut(&[u8])) {
    let mut pos = 0usize;
    while pos + 8 <= b.len() {
        let size = u32::from_be_bytes(b[pos..pos + 4].try_into().unwrap()) as u64;
        let id = &b[pos + 4..pos + 8];
        let (hdr, body_len) = match size {
            0 => (8u64, (b.len() - pos) as u64 - 8),
            1 => {
                if pos + 16 > b.len() {
                    return;
                }
                let l = u64::from_be_bytes(b[pos + 8..pos + 16].try_into().unwrap());
                (16, l.saturating_sub(16))
            }
            s => (8, s.saturating_sub(8)),
        };
        let start = pos + hdr as usize;
        let end = start.saturating_add(body_len as usize);
        if id == want
            && let Some(body) = b.get(start..end.min(b.len()))
        {
            f(body);
        }
        if end <= pos {
            return;
        }
        pos = end;
    }
}

fn ogg_label(b: &[u8]) -> &'static str {
    // first page payload
    if b.len() >= 27 && &b[..4] == b"OggS" {
        let nsegs = b[26] as usize;
        if b.len() >= 27 + nsegs {
            let plen: usize = b[27..27 + nsegs].iter().map(|&v| v as usize).sum();
            let start = 27 + nsegs;
            let end = (start + plen).min(b.len());
            let payload = &b[start..end];
            if payload.windows(8).any(|w| w == b"OpusHead") {
                return "ogg opus";
            }
            if payload.windows(7).any(|w| w == b"\x01vorbis") {
                return "ogg vorbis";
            }
        }
    }
    "ogg"
}

pub fn sniff(b: &[u8]) -> Sniff {
    if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WAVE" {
        return Sniff::Wav;
    }
    if b.len() >= 12 && &b[..4] == b"FORM" && (&b[8..12] == b"AIFF" || &b[8..12] == b"AIFC") {
        return Sniff::Aiff;
    }
    if b.starts_with(b"fLaC") {
        return Sniff::Flac;
    }
    if b.len() >= 8 && &b[4..8] == b"ftyp" {
        return Sniff::Mp4(mp4_audio_entry(b).unwrap_or([0; 4]));
    }
    if b.starts_with(b"OggS") {
        return Sniff::Lossy(ogg_label(b));
    }
    // ADTS must be checked before generic MPEG audio (both start 0xFF)
    if adts_at(&b[id3v2_skip(b)..]) {
        return Sniff::Lossy("aac");
    }
    if mpeg_audio_at(&b[id3v2_skip(b)..]) {
        return Sniff::Lossy("mp3");
    }
    Sniff::Unknown
}

/// Plausible MPEG audio frame header (any version/layer): sync, no reserved
/// fields. Lossy — encode rejects it; this check only labels the rejection.
fn mpeg_audio_at(b: &[u8]) -> bool {
    b.len() >= 4
        && b[0] == 0xFF
        && (b[1] & 0xE0) == 0xE0
        && (b[1] >> 3) & 3 != 1 // version != reserved
        && (b[1] >> 1) & 3 != 0 // layer != reserved
        && (b[2] >> 4) & 0xF != 0xF // bitrate != bad
        && (b[2] >> 2) & 3 != 3 // sampling != reserved
}

// ----------------------------------------------------- symphonia decode core

struct SymDecode {
    channels: u16,
    rate: u32,
    bits: u16,
    x: Vec<Vec<i64>>,
    tags: Vec<Tag>,
    visuals: Vec<Visual>,
}

fn alac_cookie_depth(p: &AudioCodecParameters) -> Option<u32> {
    if p.codec != well_known::CODEC_ID_ALAC {
        return None;
    }
    // magic cookie: frameLength u32, compatibleVersion u8, bitDepth u8
    p.extra_data
        .as_ref()
        .and_then(|d| d.get(5).map(|&v| u32::from(v)))
}

/// Probe + decode all packets of the first audio track. Any decode error is
/// fatal — silently skipping a packet would store shortened PCM while claiming
/// the audio is identical.
fn symphonia_decode(bytes: &[u8]) -> Result<SymDecode, String> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
    let mut fmt = get_probe()
        .probe(
            &Hint::new(),
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| format!("probe failed: {e}"))?;
    let track = fmt
        .first_track(TrackType::Audio)
        .or_else(|| fmt.tracks().first())
        .ok_or("no audio track")?;
    let tid = track.id;
    let params = match &track.codec_params {
        Some(CodecParameters::Audio(p)) => p.clone(),
        _ => return Err("no audio codec parameters".into()),
    };
    let bits = params
        .bits_per_sample
        .or_else(|| alac_cookie_depth(&params))
        .ok_or("unknown bit depth")?;
    if bits == 0 || bits >= 32 {
        return Err(format!("unsupported bit depth {bits}"));
    }
    let opts = AudioDecoderOptions::default().verify(true);
    let mut dec = get_codecs()
        .make_audio_decoder(&params, &opts)
        .map_err(|e| format!("decoder init: {e}"))?;
    let shift = 32 - bits;
    let mut x: Vec<Vec<i64>> = Vec::new();
    let mut rate = 0u32;
    loop {
        let packet = match fmt.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(e) => return Err(format!("demuxer: {e}")),
        };
        if packet.track_id != tid {
            continue;
        }
        let buf = match dec.decode(&packet) {
            Ok(b) => b,
            Err(Error::DecodeError(e)) => return Err(format!("decoder: {e}")),
            Err(Error::ResetRequired) => return Err("decoder: reset required".into()),
            Err(e) => return Err(format!("decoder: {e}")),
        };
        let GenericAudioBufferRef::S32(b) = buf else {
            return Err("unexpected decoder buffer format".into());
        };
        rate = b.spec().rate();
        let nch = b.spec().channels().count();
        if x.is_empty() {
            x = vec![Vec::new(); nch];
        } else if x.len() != nch {
            return Err("channel count changed mid-stream".into());
        }
        for (c, dst) in x.iter_mut().enumerate() {
            let plane = b.plane(c).ok_or("missing channel plane")?;
            dst.extend(plane.iter().map(|&s| i64::from(s >> shift)));
        }
    }
    let fin = dec.finalize();
    if fin.verify_ok == Some(false) {
        return Err("audio verification (MD5) failed".into());
    }
    if x.is_empty() || rate == 0 {
        return Err("no audio decoded".into());
    }
    let (tags, visuals) = match fmt.metadata().skip_to_latest() {
        Some(rev) => (rev.media.tags.clone(), rev.media.visuals.clone()),
        None => (Vec::new(), Vec::new()),
    };
    Ok(SymDecode {
        channels: x.len() as u16,
        rate,
        bits: bits as u16,
        x,
        tags,
        visuals,
    })
}

// ------------------------------------------------------------------ tag blob

/// Retained FLAC metadata blocks: everything except STREAMINFO(0), PADDING(1)
/// and SEEKTABLE(3), as (header + body) with the last-block flag cleared.
pub fn flac_tag_blocks(b: &[u8]) -> Vec<u8> {
    if !b.starts_with(b"fLaC") {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut pos = 4usize;
    while pos + 4 <= b.len() {
        let last = b[pos] & 0x80 != 0;
        let len =
            (u32::from(b[pos + 1]) << 16) | (u32::from(b[pos + 2]) << 8) | u32::from(b[pos + 3]);
        let end = pos + 4 + len as usize;
        if end > b.len() {
            break;
        }
        if !matches!(b[pos] & 0x7f, 0 | 1 | 3) {
            let at = out.len();
            out.extend(&b[pos..end]);
            out[at] &= 0x7f; // last-block flag rewritten on output
        }
        pos = end;
        if last {
            break;
        }
    }
    out
}

/// Canonical vorbis key (name, value) for a standard tag, if one of the
/// supported standard keys.
fn vorbis_key(t: &Tag) -> Option<(&'static str, String)> {
    let s = t.std.as_ref()?;
    use StandardTag as S;
    Some(match s {
        S::TrackTitle(v) | S::PartTitle(v) => ("TITLE", (**v).clone()),
        S::Artist(v) => ("ARTIST", (**v).clone()),
        S::Album(v) | S::CollectionTitle(v) => ("ALBUM", (**v).clone()),
        S::AlbumArtist(v) => ("ALBUMARTIST", (**v).clone()),
        S::ReleaseDate(v) | S::RecordingDate(v) | S::OriginalReleaseDate(v) => {
            ("DATE", (**v).clone())
        }
        S::ReleaseYear(y) | S::RecordingYear(y) | S::OriginalReleaseYear(y) => {
            ("DATE", y.to_string())
        }
        S::TrackNumber(n) => ("TRACKNUMBER", n.to_string()),
        S::DiscNumber(n) => ("DISCNUMBER", n.to_string()),
        S::Genre(v) => ("GENRE", (**v).clone()),
        S::Composer(v) => ("COMPOSER", (**v).clone()),
        S::Comment(v) | S::Description(v) => ("COMMENT", (**v).clone()),
        _ => return None,
    })
}

fn push_block(out: &mut Vec<u8>, typ: u8, body: &[u8]) {
    out.push(typ); // last flag cleared; set on output
    let len = body.len() as u32;
    out.extend([(len >> 16) as u8, (len >> 8) as u8, len as u8]);
    out.extend(body);
}

/// Convert symphonia metadata into a tag blob: one VORBIS_COMMENT block plus
/// one PICTURE block per visual (type 3 front cover).
fn symphonia_tag_blob(tags: &[Tag], visuals: &[Visual]) -> Vec<u8> {
    let mut out = Vec::new();
    let comments: Vec<(String, String)> = tags
        .iter()
        .filter_map(|t| vorbis_key(t).map(|(k, v)| (k.to_string(), v)))
        .collect();
    if !comments.is_empty() {
        let mut body = Vec::new();
        let vendor = b"flq";
        body.extend((vendor.len() as u32).to_le_bytes());
        body.extend(vendor);
        body.extend((comments.len() as u32).to_le_bytes());
        for (k, v) in &comments {
            let e = format!("{k}={v}");
            body.extend((e.len() as u32).to_le_bytes());
            body.extend(e.as_bytes());
        }
        push_block(&mut out, 4, &body);
    }
    for v in visuals {
        let mut body = Vec::new();
        body.extend(3u32.to_be_bytes()); // front cover
        let mime = v.media_type.clone().unwrap_or_else(|| "image/jpeg".into());
        body.extend((mime.len() as u32).to_be_bytes());
        body.extend(mime.as_bytes());
        body.extend(0u32.to_be_bytes()); // description len
        let (w, h) = v.dimensions.map_or((0, 0), |d| (d.width, d.height));
        body.extend(w.to_be_bytes());
        body.extend(h.to_be_bytes());
        let depth = v.color_mode.map_or(0, |m| match m {
            symphonia::core::meta::ColorMode::Direct(cm) => cm.bits_per_pixel(),
            symphonia::core::meta::ColorMode::Indexed(p) => p.color_model.bits_per_pixel(),
        });
        body.extend(depth.to_be_bytes());
        body.extend(0u32.to_be_bytes()); // palette colors
        body.extend((v.data.len() as u32).to_be_bytes());
        body.extend(&v.data);
        push_block(&mut out, 6, &body);
    }
    out
}

// ----------------------------------------------------------------- public io

/// Decoded PCM plus the preserved tag blob.
pub struct PcmSource {
    pub channels: u16,
    pub rate: u32,
    pub bits: u16,
    pub x: Vec<Vec<i64>>,
    pub tags: Vec<u8>,
}

/// STREAMINFO total_samples (bytes 18..26 of the file, low 36 bits); 0 = unknown.
fn flac_total_samples(b: &[u8]) -> u64 {
    if b.len() >= 42 && b.starts_with(b"fLaC") && b[4] & 0x7f == 0 {
        u64::from_be_bytes(b[18..26].try_into().unwrap()) & 0x000F_FFFF_FFFF
    } else {
        0
    }
}

/// FLAC input: symphonia decode (MD5-verified) + retained metadata blocks.
pub fn read_flac(bytes: &[u8]) -> Result<PcmSource, String> {
    let s = symphonia_decode(bytes)?;
    let total = flac_total_samples(bytes);
    if total != 0 && s.x[0].len() as u64 != total {
        return Err(format!(
            "decoded {} samples, STREAMINFO declares {total}",
            s.x[0].len()
        ));
    }
    Ok(PcmSource {
        channels: s.channels,
        rate: s.rate,
        bits: s.bits,
        x: s.x,
        tags: flac_tag_blocks(bytes),
    })
}

/// ALAC-in-MP4 input: symphonia decode + ilst metadata as FLAC-style blocks.
pub fn read_m4a(bytes: &[u8]) -> Result<PcmSource, String> {
    let s = symphonia_decode(bytes)?;
    Ok(PcmSource {
        channels: s.channels,
        rate: s.rate,
        bits: s.bits,
        x: s.x,
        tags: symphonia_tag_blob(&s.tags, &s.visuals),
    })
}

/// Decoded integer PCM: (channels, sample rate, bits per sample, per-channel
/// sample vectors).
pub type DecodedPcm = (u16, u32, u16, Vec<Vec<i64>>);

/// Decode a file to integer PCM streams (WAV/AIFF/FLAC/ALAC-in-MP4).
/// Used by the FLAC-output verification.
pub fn decode_pcm(bytes: &[u8]) -> Result<DecodedPcm, String> {
    match sniff(bytes) {
        Sniff::Wav => {
            let w = wav::parse(bytes)?;
            if w.tag != 1 {
                return Err("float WAV not supported by decode_pcm".into());
            }
            Ok((w.channels, w.rate, w.bits, wav::channel_ints(&w)))
        }
        Sniff::Aiff => {
            let w = aiff::parse(bytes)?;
            if w.tag != 1 {
                return Err("float AIFF not supported by decode_pcm".into());
            }
            Ok((w.channels, w.rate, w.bits, wav::channel_ints(&w)))
        }
        _ => {
            let s = symphonia_decode(bytes)?;
            Ok((s.channels, s.rate, s.bits, s.x))
        }
    }
}

// --------------------------------------------------------------- FLAC output

/// Parse the metadata-block chain; returns (end of STREAMINFO block, start of
/// first frame). STREAMINFO must be the first block.
fn split_flac(enc: &[u8]) -> Result<(usize, usize), String> {
    if !enc.starts_with(b"fLaC") || enc.len() < 8 + 34 {
        return Err("flacenc output malformed".into());
    }
    if enc[4] & 0x7f != 0 {
        return Err("flacenc: first block is not STREAMINFO".into());
    }
    let mut pos = 4usize;
    let mut si_end = 0usize;
    loop {
        if pos + 4 > enc.len() {
            return Err("flacenc: truncated metadata".into());
        }
        let last = enc[pos] & 0x80 != 0;
        let len = (u32::from(enc[pos + 1]) << 16)
            | (u32::from(enc[pos + 2]) << 8)
            | u32::from(enc[pos + 3]);
        let end = pos + 4 + len as usize;
        if end > enc.len() {
            return Err("flacenc: truncated metadata".into());
        }
        if pos == 4 {
            si_end = end;
        }
        pos = end;
        if last {
            break;
        }
    }
    Ok((si_end, pos))
}

/// Encode PCM to FLAC with flacenc, splice the preserved tag blob in after
/// STREAMINFO, then verify by re-decoding with symphonia.
pub fn encode_flac(
    x: &[Vec<i64>],
    bits: u16,
    channels: u16,
    rate: u32,
    tags: &[u8],
) -> Result<Vec<u8>, String> {
    use flacenc::bitsink::MemSink;
    use flacenc::component::BitRepr;
    use flacenc::error::Verify;
    let frames = x.first().map_or(0, |c| c.len());
    let ch = x.len();
    let mut inter = Vec::with_capacity(frames * ch);
    for f in 0..frames {
        for c in x.iter() {
            inter.push(c[f] as i32);
        }
    }
    let src = flacenc::source::MemSource::from_samples(&inter, ch, bits as usize, rate as usize);
    let cfg = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|e| format!("flacenc config: {e:?}"))?;
    let stream = flacenc::encode_with_fixed_block_size(&cfg, src, 4096)
        .map_err(|e| format!("flacenc: {e}"))?;
    let mut sink = MemSink::<u8>::new();
    stream
        .write(&mut sink)
        .map_err(|e| format!("flac write: {e}"))?;
    let mut enc = sink.into_inner();
    if enc.len() >= 12 {
        // flacenc counts the short last block in min_block_size, declaring a
        // variable-blocksize stream while still emitting fixed-strategy frames.
        // Normalize to libFLAC behavior: min = max (last block may be shorter).
        enc[8] = enc[10];
        enc[9] = enc[11];
    }
    let (si_end, frames_at) = split_flac(&enc)?;
    let mut out = Vec::with_capacity(enc.len() + tags.len());
    out.extend(b"fLaC");
    // STREAMINFO, last flag set only when no tag blocks follow
    out.extend(&enc[4..si_end]);
    if !tags.is_empty() {
        out[4] &= 0x7f; // streaminfo is no longer the last block
        // retained tag blocks; set the last-block flag on the final one
        let mut pos = 0usize;
        let mut last_at = out.len();
        while pos + 4 <= tags.len() {
            let len = (u32::from(tags[pos + 1]) << 16)
                | (u32::from(tags[pos + 2]) << 8)
                | u32::from(tags[pos + 3]);
            let end = pos + 4 + len as usize;
            if end > tags.len() {
                return Err("stored tag blob truncated".into());
            }
            last_at = out.len();
            out.extend(&tags[pos..end]);
            out[last_at] = tags[pos] & 0x7f;
            pos = end;
        }
        if pos != tags.len() {
            return Err("stored tag blob truncated".into());
        }
        out[last_at] |= 0x80;
    }
    out.extend(&enc[frames_at..]);
    // verify: re-decode and compare samples
    let (c2, r2, b2, x2) = decode_pcm(&out)?;
    if (c2, r2, b2) != (channels, rate, bits) || x2 != x {
        return Err("flac output verification failed".into());
    }
    Ok(out)
}

/// WAV header (RIFF + fmt + data tag/size) for a v1 export of `pcm_len`
/// canonical PCM bytes. The caller appends the PCM bytes (and one pad byte
/// when the count is odd). int tag 1 (8/16/24/32), float tag 3 (32/64);
/// uses WAVE_FORMAT_EXTENSIBLE (with `mask` and the matching subformat
/// GUID) when channels > 2 or a channel mask is set.
pub fn wav_header_v1(
    pcm_len: usize,
    sample_fmt: u8,
    bits: u8,
    channels: u8,
    rate: u32,
    mask: u32,
) -> Vec<u8> {
    let subtag: u16 = if sample_fmt == 1 { 3 } else { 1 };
    let align = u32::from(channels) * (u32::from(bits) / 8);
    let mut fmt = Vec::new();
    let extensible = channels > 2 || mask != 0;
    fmt.extend(if extensible { 0xFFFEu16 } else { subtag }.to_le_bytes());
    fmt.extend(u16::from(channels).to_le_bytes());
    fmt.extend(rate.to_le_bytes());
    fmt.extend((rate * align).to_le_bytes());
    fmt.extend((align as u16).to_le_bytes());
    fmt.extend(u16::from(bits).to_le_bytes());
    if extensible {
        fmt.extend(22u16.to_le_bytes()); // cbSize
        fmt.extend(u16::from(bits).to_le_bytes()); // valid bits
        fmt.extend(mask.to_le_bytes());
        // KSDATAFORMAT_SUBTYPE_PCM/IEEE_FLOAT: {tag-0000-0010-8000-00aa00389b71}
        fmt.extend(subtag.to_le_bytes());
        fmt.extend([
            0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
        ]);
    }
    let mut body = b"WAVE".to_vec();
    body.extend(b"fmt ");
    body.extend((fmt.len() as u32).to_le_bytes());
    body.extend(fmt);
    body.extend(b"data");
    body.extend((pcm_len as u32).to_le_bytes());
    let mut out = b"RIFF".to_vec();
    out.extend(((body.len() + pcm_len + (pcm_len & 1)) as u32).to_le_bytes());
    out.extend(body);
    out
}

/// WAV writer for v1 export: int tag 1 (8/16/24/32), float tag 3 (32/64).
/// Uses WAVE_FORMAT_EXTENSIBLE (with `mask` and the matching subformat GUID)
/// when channels > 2 or a channel mask is set.
pub fn make_wav_v1(
    pcm: &[u8],
    sample_fmt: u8,
    bits: u8,
    channels: u8,
    rate: u32,
    mask: u32,
) -> Vec<u8> {
    let mut out = wav_header_v1(pcm.len(), sample_fmt, bits, channels, rate, mask);
    out.extend(pcm);
    if pcm.len() % 2 == 1 {
        out.push(0); // chunks are even-padded
    }
    out
}

/// Canonical PCM WAV (tag 1, 44-byte header).
pub fn make_wav(x: &[Vec<i64>], bits: u16, channels: u16, rate: u32) -> Vec<u8> {
    let frames = x.first().map_or(0, |c| c.len());
    let pcm = wav::ints_to_pcm(x, bits, frames);
    let align = u32::from(channels) * (u32::from(bits) / 8);
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend(b"RIFF");
    out.extend((36 + pcm.len() as u32).to_le_bytes());
    out.extend(b"WAVE");
    out.extend(b"fmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(channels.to_le_bytes());
    out.extend(rate.to_le_bytes());
    out.extend((rate * align).to_le_bytes());
    out.extend((align as u16).to_le_bytes());
    out.extend(bits.to_le_bytes());
    out.extend(b"data");
    out.extend((pcm.len() as u32).to_le_bytes());
    out.extend(pcm);
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::bits::BitWriter;
    use std::f64::consts::PI;

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

    // ----------------------------------------------------- fixture builders

    /// VORBIS_COMMENT + PICTURE blocks as a FLAC-metadata blob (headers incl.).
    pub fn tag_blob_test() -> Vec<u8> {
        let mut blob = Vec::new();
        let mut vc = Vec::new();
        let vendor = b"flq-test";
        vc.extend((vendor.len() as u32).to_le_bytes());
        vc.extend(vendor);
        let comments = ["TITLE=Test Song", "ARTIST=Tester", "ALBUM=Fixtures"];
        vc.extend((comments.len() as u32).to_le_bytes());
        for c in comments {
            vc.extend((c.len() as u32).to_le_bytes());
            vc.extend(c.as_bytes());
        }
        push_block(&mut blob, 4, &vc);
        let mut pic = Vec::new();
        pic.extend(3u32.to_be_bytes()); // front cover
        pic.extend(10u32.to_be_bytes());
        pic.extend(b"image/jpeg");
        pic.extend(0u32.to_be_bytes()); // description
        pic.extend(2u32.to_be_bytes()); // w
        pic.extend(2u32.to_be_bytes()); // h
        pic.extend(24u32.to_be_bytes()); // depth
        pic.extend(0u32.to_be_bytes()); // palette
        let data = [0xDEu8, 0xAD, 0xBE, 0xEF, 1, 2, 3];
        pic.extend((data.len() as u32).to_be_bytes());
        pic.extend(data);
        push_block(&mut blob, 6, &pic);
        blob
    }

    /// A real FLAC file carrying the test tag blocks (produced by flacenc).
    pub fn flac_fixture(x: &[Vec<i64>], bits: u16, rate: u32) -> Vec<u8> {
        encode_flac(x, bits, x.len() as u16, rate, &tag_blob_test())
            .expect("flacenc fixture encode")
    }

    fn atom(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(8 + body.len());
        v.extend((body.len() as u32 + 8).to_be_bytes());
        v.extend(*id);
        v.extend(body);
        v
    }

    fn full(id: &[u8; 4], vf: u32, body: &[u8]) -> Vec<u8> {
        let mut b = vf.to_be_bytes().to_vec();
        b.extend_from_slice(body);
        atom(id, &b)
    }

    /// One ALAC packet: an uncompressed SCE/CPE element + END tag.
    fn alac_packet(x: &[Vec<i64>], bits: u16) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.write(if x.len() == 2 { 1 } else { 0 }, 3); // CPE / SCE
        w.write(0, 4); // element instance tag
        w.write(0, 12); // unused header
        w.write(1, 1); // partial frame
        w.write(0, 2); // shift
        w.write(1, 1); // uncompressed escape
        w.write(x[0].len() as u64, 32); // num_samples
        for f in 0..x[0].len() {
            for c in x.iter() {
                w.write_signed(c[f], u32::from(bits));
            }
        }
        w.write(7, 3); // END
        w.into_bytes()
    }

    /// Minimal MP4 (ftyp, moov/.../stsd['alac'], mdat) holding one ALAC packet.
    /// `entry` selects the stsd fourcc: b"alac" gets a real ALAC entry +
    /// cookie, anything else gets a bare audio sample entry (e.g. b"mp4a").
    pub fn mp4_fixture(x: &[Vec<i64>], bits: u16, rate: u32, entry: &[u8; 4]) -> Vec<u8> {
        let ch = x.len() as u16;
        let frames = x[0].len() as u32;
        let packet = if entry == b"alac" {
            alac_packet(x, bits)
        } else {
            vec![0xABu8; 64] // opaque payload for non-alac entries
        };
        let mut cookie = Vec::with_capacity(24);
        cookie.extend(frames.max(4096).to_be_bytes()); // frame_length
        cookie.push(0); // compatible version
        cookie.push(bits as u8); // bit depth
        cookie.extend([40u8, 10, 14]); // pb, mb, kb
        cookie.push(ch as u8);
        cookie.extend(255u16.to_be_bytes()); // max_run
        cookie.extend(0u32.to_be_bytes()); // max_frame_bytes
        cookie.extend(0u32.to_be_bytes()); // avg_bit_rate
        cookie.extend(rate.to_be_bytes());
        let alac_sub = atom(b"alac", &[0u32.to_be_bytes().as_slice(), &cookie].concat());
        let mut se = Vec::new();
        se.extend([0u8; 6]);
        se.extend(1u16.to_be_bytes()); // data reference index
        se.extend(0u16.to_be_bytes()); // version 0
        se.extend(0u16.to_be_bytes()); // revision
        se.extend(0u32.to_be_bytes()); // vendor
        se.extend(ch.to_be_bytes());
        se.extend(bits.to_be_bytes()); // sample size
        se.extend([0u8; 4]); // compression id + packet size
        se.extend((rate << 16).to_be_bytes());
        if entry == b"alac" {
            se.extend(&alac_sub);
        }
        let stsd = full(
            b"stsd",
            0,
            &[1u32.to_be_bytes().as_slice(), &atom(entry, &se)].concat(),
        );
        let stts = full(
            b"stts",
            0,
            &[
                1u32.to_be_bytes().as_slice(),
                &1u32.to_be_bytes(),
                &frames.to_be_bytes(),
            ]
            .concat(),
        );
        let stsc = full(
            b"stsc",
            0,
            &[
                1u32.to_be_bytes().as_slice(),
                &1u32.to_be_bytes(),
                &1u32.to_be_bytes(),
                &1u32.to_be_bytes(),
            ]
            .concat(),
        );
        let stsz = full(
            b"stsz",
            0,
            &[
                0u32.to_be_bytes().as_slice(),
                &1u32.to_be_bytes(),
                &(packet.len() as u32).to_be_bytes(),
            ]
            .concat(),
        );
        let moov = |data_off: u32| -> Vec<u8> {
            let stco = full(
                b"stco",
                0,
                &[1u32.to_be_bytes().as_slice(), &data_off.to_be_bytes()].concat(),
            );
            let stbl = atom(b"stbl", &[&stsd[..], &stts, &stsc, &stsz, &stco].concat());
            let smhd = full(b"smhd", 0, &[0u8; 4]);
            let minf = atom(b"minf", &[&smhd[..], &stbl].concat());
            let hdlr = full(
                b"hdlr",
                0,
                &[0u32.to_be_bytes().as_slice(), b"soun", &[0u8; 12]].concat(),
            );
            let mdhd = full(
                b"mdhd",
                0,
                &[
                    0u32.to_be_bytes().as_slice(),
                    &0u32.to_be_bytes(),
                    &rate.to_be_bytes(),
                    &frames.to_be_bytes(),
                    &0x55c4u16.to_be_bytes(),
                    &0u16.to_be_bytes(),
                ]
                .concat(),
            );
            let mdia = atom(b"mdia", &[&mdhd[..], &hdlr, &minf].concat());
            let matrix: Vec<u8> = [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000]
                .iter()
                .flat_map(|v| v.to_be_bytes())
                .collect();
            let tkhd = full(
                b"tkhd",
                0x0000_0007,
                &[
                    &[0u8; 8][..],
                    &1u32.to_be_bytes(),
                    &[0u8; 4],
                    &frames.to_be_bytes(),
                    &[0u8; 8],
                    &[0u8; 4],
                    &0x0100u16.to_be_bytes(),
                    &[0u8; 2],
                    &matrix,
                    &0u32.to_be_bytes(),
                    &0u32.to_be_bytes(),
                ]
                .concat(),
            );
            let trak = atom(b"trak", &[&tkhd[..], &mdia].concat());
            let mvhd = full(
                b"mvhd",
                0,
                &[
                    &[0u8; 8][..],
                    &rate.to_be_bytes(),
                    &frames.to_be_bytes(),
                    &0x0001_0000u32.to_be_bytes(),
                    &0x0100u16.to_be_bytes(),
                    &[0u8; 10],
                    &matrix,
                    &[0u8; 24],
                    &2u32.to_be_bytes(),
                ]
                .concat(),
            );
            atom(b"moov", &[&mvhd[..], &trak].concat())
        };
        let ftyp = atom(
            b"ftyp",
            &[b"M4A ".as_slice(), &0u32.to_be_bytes(), b"M4A ", b"isom"].concat(),
        );
        let m0 = moov(0);
        let data_off = (ftyp.len() + m0.len() + 8) as u32;
        let moov = moov(data_off);
        [&ftyp[..], &moov, &atom(b"mdat", &packet)].concat()
    }

    // ---------------------------------------------------------------- tests

    #[test]
    fn flac_decode_and_tags() {
        let rate = 44100;
        for (bits, ch) in [(16u16, 2usize), (24, 2), (8, 1)] {
            let sig = stereo(31, rate, 3000);
            let x: Vec<Vec<i64>> = (0..ch)
                .map(|c| {
                    sig[c % 2]
                        .iter()
                        .map(|&v| v.clamp(-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1))
                        .collect()
                })
                .collect();
            let flac = flac_fixture(&x, bits, rate);
            assert!(flac.starts_with(b"fLaC"));
            assert_eq!(sniff(&flac), Sniff::Flac);
            let s = read_flac(&flac).unwrap();
            assert_eq!((s.channels as usize, s.rate, s.bits), (ch, rate, bits));
            assert_eq!(s.x, x, "bits {bits}");
            assert_eq!(s.tags, flac_tag_blocks(&flac));
            assert_eq!(s.tags, tag_blob_test());
        }
    }

    #[test]
    fn alac_decode() {
        let rate = 44100;
        let sig = stereo(17, rate, 2000);
        let m4a = mp4_fixture(&sig, 16, rate, b"alac");
        assert_eq!(sniff(&m4a), Sniff::Mp4(*b"alac"));
        let s = read_m4a(&m4a).unwrap();
        assert_eq!((s.channels, s.rate, s.bits), (2, rate, 16));
        assert_eq!(s.x, sig);
        // mono SCE path too
        let mono = vec![sig[0].clone()];
        let m4a = mp4_fixture(&mono, 16, rate, b"alac");
        let s = read_m4a(&m4a).unwrap();
        assert_eq!(s.x, mono);
    }

    #[test]
    fn mp4_mp4a_sniff() {
        let sig = stereo(7, 44100, 100);
        let mp4 = mp4_fixture(&sig, 16, 44100, b"mp4a");
        assert_eq!(sniff(&mp4), Sniff::Mp4(*b"mp4a"));
    }

    #[test]
    fn adts_and_ogg_sniff() {
        // ADTS header + payload
        let frame = |payload: &[u8]| -> Vec<u8> {
            let len = 7 + payload.len();
            let mut h = vec![0xFFu8, 0xF1]; // sync, MPEG-4, layer 0, no CRC
            h.push(0b0100_0000); // profile AAC-LC, sf idx 4 (44100)
            h.push(0b0100_0000 | ((len >> 11) as u8));
            h.push((len >> 3) as u8);
            h.push(((len & 7) << 5) as u8 | 0b11111);
            h.push(0xFC);
            h.extend_from_slice(payload);
            h
        };
        let adts = frame(&[0x55; 100]);
        assert_eq!(sniff(&adts), Sniff::Lossy("aac"));
        // with ID3v2 prefix
        let mut tagged = b"ID3\x04\x00\x00\x00\x00\x00\x05abcde".to_vec();
        tagged.extend(&adts);
        assert_eq!(sniff(&tagged), Sniff::Lossy("aac"));
        // ogg page with OpusHead payload
        let ogg = |payload: &[u8]| -> Vec<u8> {
            assert!(payload.len() < 255);
            let mut v = b"OggS\x00\x02".to_vec();
            v.extend([0u8; 8]); // granule
            v.extend([0u8; 8]); // serial + seq
            v.extend([0u8; 4]); // crc
            v.push(1); // one segment
            v.push(payload.len() as u8); // lacing value
            v.extend(payload);
            v
        };
        let mut p = b"OpusHead".to_vec();
        p.extend([1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(sniff(&ogg(&p)), Sniff::Lossy("ogg opus"));
        let mut p = vec![1u8];
        p.extend(b"vorbis");
        assert_eq!(sniff(&ogg(&p)), Sniff::Lossy("ogg vorbis"));
        assert_eq!(sniff(&ogg(b"randomstuff")), Sniff::Lossy("ogg"));
        assert_eq!(sniff(b"garbage"), Sniff::Unknown);
    }
}
