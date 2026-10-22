//! v1 reader: header/meta/index validation, indexed + scanning frame access
//! with per-frame CRC checks and corruption resync, and PCM decoding.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};

use crate::bits::{BitReader, crc32};
use crate::codec::{self, FloatMeta, TrackMeta};
use crate::v1::*;

pub use crate::bits::Crc32; // incremental pcm_crc verification on streamed decode

/// Decoded channel samples: integer i64s, or IEEE words in the track's
/// declared width (`u64` carrying f32 bits zero-extended or f64 bits).
pub enum Samples {
    Int(Vec<Vec<i64>>),
    Float(Vec<Vec<u64>>),
}

impl Samples {
    pub fn channels(&self) -> usize {
        match self {
            Samples::Int(v) => v.len(),
            Samples::Float(v) => v.len(),
        }
    }
    pub fn frames(&self) -> usize {
        match self {
            Samples::Int(v) => v.first().map_or(0, |c| c.len()),
            Samples::Float(v) => v.first().map_or(0, |c| c.len()),
        }
    }
}

/// Canonical interleaved PCM bytes (`wav::ints_to_pcm`/`words_to_pcm`
/// convention: LE, 8-bit unsigned).
pub fn samples_to_pcm(s: &Samples, bits: u8) -> Vec<u8> {
    match s {
        Samples::Int(x) => {
            crate::wav::ints_to_pcm(x, u16::from(bits), x.first().map_or(0, |c| c.len()))
        }
        Samples::Float(w) => {
            crate::wav::words_to_pcm(w, u16::from(bits), w.first().map_or(0, |c| c.len()))
        }
    }
}

// ------------------------------------------------------------------ reader

pub struct Reader<R: Read + Seek> {
    r: R,
    frame_len: u32,
    tracks: Vec<TrackInfo>,
    index: Option<Vec<Vec<u64>>>,
    meta_end: u64,
    /// End of the frame region: `index_off` when present, else file_len.
    frames_end: u64,
    file_len: u64,
}

struct FrameRec {
    head: FrameHead,
    payload: Vec<u8>,
}

/// Upper bound on up-front sample reservations — declared `frames` comes
/// from meta and must not drive a large allocation before decode proves
/// the data exists.
const MAX_RESERVE: u64 = 1 << 20;

impl<R: Read + Seek> Reader<R> {
    /// Parse the header + meta, then the index when `index_off != 0`.
    pub fn open(mut r: R) -> Result<Self, String> {
        let file_len = r.seek(SeekFrom::End(0)).map_err(|e| format!("seek: {e}"))?;
        r.seek(SeekFrom::Start(0))
            .map_err(|e| format!("seek: {e}"))?;
        let mut hdr = [0u8; HEADER_LEN];
        r.read_exact(&mut hdr)
            .map_err(|_| "truncated header".to_string())?;
        let h = parse_header(&hdr)?;
        let meta_len = u64::from(h.meta_len);
        if !(4..=MAX_META_LEN).contains(&meta_len) {
            return Err("meta_len out of range".into());
        }
        let meta_end = HEADER_LEN as u64 + meta_len;
        if meta_end > file_len {
            return Err("meta_len extends past end of file".into());
        }
        let mut meta = vec![0u8; meta_len as usize];
        r.read_exact(&mut meta)
            .map_err(|e| format!("read meta: {e}"))?;
        let (body, tail) = meta.split_at(meta.len() - 4);
        if crc32(body) != u32le(tail, 0) {
            return Err("meta crc mismatch".into());
        }
        let tracks = parse_meta(body, h.ntracks as usize)?;
        for (t, m) in tracks.iter().enumerate() {
            if m.frames > 0 && m.nframes == 0 {
                return Err(format!("meta: track {t}: nframes is 0 for nonempty track"));
            }
            if m.nframes > 0 && u64::from(m.nframes) != m.frames.div_ceil(u64::from(h.frame_len)) {
                return Err(format!("meta: track {t}: nframes disagrees with frames"));
            }
        }
        if h.index_off != 0 && h.index_off < meta_end {
            return Err("index_off overlaps header/meta".into());
        }
        let frames_end = if h.index_off != 0 {
            h.index_off
        } else {
            file_len
        };
        // every frame costs >= header + crc bytes in the frame region, so a
        // declared nframes above this bound can never be satisfied
        let frame_span = frames_end.min(file_len).saturating_sub(meta_end);
        for (t, m) in tracks.iter().enumerate() {
            if u64::from(m.nframes) > frame_span / (FRAME_HDR_LEN as u64 + 4) {
                return Err(format!("meta: track {t}: nframes exceeds frame region"));
            }
        }
        let mut rd = Reader {
            r,
            frame_len: h.frame_len,
            tracks,
            index: None,
            meta_end,
            frames_end,
            file_len,
        };
        if h.index_off != 0 {
            rd.index = Some(rd.read_index(h.index_off)?);
        }
        Ok(rd)
    }

    fn read_index(&mut self, at: u64) -> Result<Vec<Vec<u64>>, String> {
        let want: u64 = self
            .tracks
            .iter()
            .map(|t| u64::from(t.nframes) * 8)
            .sum::<u64>()
            + 4;
        let end = at.checked_add(want).ok_or("index offset overflows")?;
        if end > self.file_len {
            return Err("index extends past end of file".into());
        }
        self.r
            .seek(SeekFrom::Start(at))
            .map_err(|e| format!("seek index: {e}"))?;
        let mut buf = vec![0u8; want as usize];
        self.r
            .read_exact(&mut buf)
            .map_err(|e| format!("read index: {e}"))?;
        let (body, tail) = buf.split_at(buf.len() - 4);
        if crc32(body) != u32le(tail, 0) {
            return Err("index crc mismatch".into());
        }
        let mut index = Vec::with_capacity(self.tracks.len());
        let mut pos = 0usize;
        for (t, m) in self.tracks.iter().enumerate() {
            let mut offs = Vec::with_capacity(m.nframes as usize);
            for f in 0..m.nframes {
                let off = u64le(body, pos);
                pos += 8;
                if off < self.meta_end || off + FRAME_HDR_LEN as u64 + 4 > at {
                    return Err(format!(
                        "index: track {t} frame {f} offset {off} out of bounds"
                    ));
                }
                offs.push(off);
            }
            index.push(offs);
        }
        Ok(index)
    }

    pub fn tracks(&self) -> &[TrackInfo] {
        &self.tracks
    }
    pub fn frame_len(&self) -> u32 {
        self.frame_len
    }
    pub fn has_index(&self) -> bool {
        self.index.is_some()
    }

    /// Sequential scan of the whole frame region (resynchronizing past
    /// corruption), collecting every frame header + wanted payload.
    pub fn scan(&mut self) -> Result<SeqScan, String> {
        frames_sequential(
            &mut self.r,
            self.meta_end,
            self.frames_end,
            self.frame_len,
            &self.tracks,
            |_| true,
        )
    }

    /// Read, CRC-check, validate and decode one frame.
    pub fn read_frame(&mut self, track: usize, frame_no: u32) -> Result<Samples, String> {
        let mut recs = self.collect(track, frame_no, frame_no)?;
        let rec = recs.remove(0);
        let m = &self.tracks[track];
        decode_frame(track, m, &rec.head, &rec.payload)
    }

    /// Fetch frames f0..=f1 of `track` (index when present, else one
    /// sequential scan), CRC-verified and header-validated.
    fn collect(&mut self, track: usize, f0: u32, f1: u32) -> Result<Vec<FrameRec>, String> {
        let m = self
            .tracks
            .get(track)
            .cloned()
            .ok_or_else(|| format!("track {track} out of range"))?;
        if f1 >= m.nframes {
            return Err(format!("track {track} frame {f1}: beyond nframes"));
        }
        if self.index.is_some() {
            let offs: Vec<u64> =
                self.index.as_ref().unwrap()[track][f0 as usize..=f1 as usize].to_vec();
            let mut out = Vec::with_capacity(offs.len());
            for (i, at) in offs.iter().enumerate() {
                let f = f0 + i as u32;
                let rec = self.read_frame_at(*at)?;
                if rec.head.track as usize != track || rec.head.frame_no != f {
                    return Err(format!(
                        "track {track} frame {f}: index points at track {} frame {}",
                        rec.head.track, rec.head.frame_no
                    ));
                }
                out.push(rec);
            }
            return Ok(out);
        }
        // No index: one sequential scan, CRC-verifying only this track.
        let scan = frames_sequential(
            &mut self.r,
            self.meta_end,
            self.frames_end,
            self.frame_len,
            &self.tracks,
            |t| t as usize == track,
        )?;
        let mut got: Vec<Option<FrameRec>> = (0..(f1 - f0 + 1) as usize).map(|_| None).collect();
        for sf in scan.frames {
            if sf.head.track as usize == track && sf.head.frame_no >= f0 && sf.head.frame_no <= f1 {
                let i = (sf.head.frame_no - f0) as usize;
                if got[i].is_none() {
                    got[i] = Some(FrameRec {
                        head: sf.head,
                        payload: sf.payload.ok_or_else(|| {
                            format!("track {track} frame {}: payload unread", sf.head.frame_no)
                        })?,
                    });
                }
            }
        }
        let mut out = Vec::with_capacity(got.len());
        for (i, g) in got.into_iter().enumerate() {
            match g {
                Some(r) => out.push(r),
                None => {
                    return Err(format!(
                        "track {track} frame {}: lost (corrupt or truncated)",
                        f0 + i as u32
                    ));
                }
            }
        }
        Ok(out)
    }

    /// Read + CRC one frame at a known offset; header fields are validated
    /// against the track's meta (n, fmode, e).
    fn read_frame_at(&mut self, off: u64) -> Result<FrameRec, String> {
        let mut head = [0u8; FRAME_HDR_LEN];
        self.r
            .seek(SeekFrom::Start(off))
            .map_err(|e| format!("seek: {e}"))?;
        self.r
            .read_exact(&mut head)
            .map_err(|e| format!("frame at {off}: header: {e}"))?;
        let h = parse_frame_head(&head).map_err(|e| format!("frame at {off}: {e}"))?;
        let ctx = format!("track {} frame {}", h.track, h.frame_no);
        let m = self
            .tracks
            .get(h.track as usize)
            .cloned()
            .ok_or_else(|| format!("{ctx}: track index out of range"))?;
        check_frame_head(&h, &m, self.frame_len).map_err(|e| format!("{ctx}: {e}"))?;
        let total = (FRAME_HDR_LEN as u64)
            .checked_add(u64::from(h.payload_len))
            .and_then(|x| x.checked_add(4))
            .ok_or_else(|| format!("{ctx}: frame size overflow"))?;
        if off + total > self.file_len {
            return Err(format!("{ctx}: frame extends past end of file"));
        }
        let mut buf = vec![0u8; total as usize];
        self.r
            .seek(SeekFrom::Start(off))
            .map_err(|e| format!("{ctx}: seek: {e}"))?;
        self.r
            .read_exact(&mut buf)
            .map_err(|e| format!("{ctx}: read: {e}"))?;
        let (body, tail) = buf.split_at(buf.len() - 4);
        if crc32(body) != u32le(tail, 0) {
            return Err(format!("{ctx}: crc mismatch"));
        }
        Ok(FrameRec {
            head: h,
            payload: body[FRAME_HDR_LEN..].to_vec(),
        })
    }

    /// Decode samples [start, start+len) of a track (clamped to the track),
    /// passing each frame's clamped slice to `sink` — one frame at a time
    /// (index: per-frame reads; no index: one sequential scan decoding wanted
    /// frames as they come). No pcm_crc check — it covers the whole track;
    /// `decode_track` verifies.
    pub fn decode_range_with(
        &mut self,
        track: usize,
        start: u64,
        len: u64,
        sink: impl FnMut(Samples) -> Result<(), String>,
    ) -> Result<(), String> {
        self.decode_range_with_par(track, start, len, 1, sink)
    }

    /// `decode_range_with` with frame decode spread over `threads` scoped
    /// workers when an index exists and the range spans several frames:
    /// a window of `2*threads` frame payloads is read sequentially, decoded
    /// in parallel, then handed to `sink` in order. Falls back to the
    /// sequential path when `threads <= 1`, the index is absent, or the
    /// range covers fewer than 4 frames.
    pub fn decode_range_with_par(
        &mut self,
        track: usize,
        start: u64,
        len: u64,
        threads: usize,
        mut sink: impl FnMut(Samples) -> Result<(), String>,
    ) -> Result<(), String> {
        let m = self
            .tracks
            .get(track)
            .cloned()
            .ok_or_else(|| format!("track {track} out of range"))?;
        if start > m.frames || (start == m.frames && len > 0) {
            return Err(format!("track {track}: range start beyond track end"));
        }
        let len = len.min(m.frames - start);
        if len == 0 {
            return Ok(());
        }
        let fl = u64::from(self.frame_len);
        let f0 = (start / fl) as u32;
        let f1 = ((start + len - 1) / fl) as u32;
        let end = start + len;
        let sl = Slice {
            track,
            m: &m,
            start,
            end,
            fl,
        };
        if self.index.is_some() {
            let offs: Vec<u64> =
                self.index.as_ref().unwrap()[track][f0 as usize..=f1 as usize].to_vec();
            if threads > 1 && offs.len() >= 4 {
                // Bounded window: read payloads sequentially, decode on
                // scoped workers, sink in frame order. Memory stays
                // O(threads * frame).
                let win = (threads * 2).max(4).min(offs.len());
                let m = sl.m;
                let mut w0 = 0usize;
                while w0 < offs.len() {
                    let w1 = (w0 + win).min(offs.len());
                    let mut recs = Vec::with_capacity(w1 - w0);
                    for (i, &at) in offs[w0..w1].iter().enumerate() {
                        let f = f0 + (w0 + i) as u32;
                        let rec = self.read_frame_at(at)?;
                        if rec.head.track as usize != track || rec.head.frame_no != f {
                            return Err(format!(
                                "track {track} frame {f}: index points at track {} frame {}",
                                rec.head.track, rec.head.frame_no
                            ));
                        }
                        recs.push(rec);
                    }
                    let wlen = recs.len();
                    let chunk = wlen.div_ceil(threads.min(wlen));
                    let mut out: Vec<Option<Result<Samples, String>>> =
                        (0..wlen).map(|_| None).collect();
                    std::thread::scope(|sc| {
                        for (ci, slice) in out.chunks_mut(chunk).enumerate() {
                            let base = ci * chunk;
                            let recs = &recs;
                            sc.spawn(move || {
                                for (i, slot) in slice.iter_mut().enumerate() {
                                    let rec = &recs[base + i];
                                    *slot = Some(decode_frame(track, m, &rec.head, &rec.payload));
                                }
                            });
                        }
                    });
                    for (slot, rec) in out.into_iter().zip(recs.iter()) {
                        let s =
                            slot.ok_or_else(|| format!("track {track}: worker left slot empty"))??;
                        feed_decoded(&sl, &rec.head, s, &mut sink)?;
                    }
                    w0 = w1;
                }
                return Ok(());
            }
            for (i, at) in offs.iter().enumerate() {
                let f = f0 + i as u32;
                let rec = self.read_frame_at(*at)?;
                if rec.head.track as usize != track || rec.head.frame_no != f {
                    return Err(format!(
                        "track {track} frame {f}: index points at track {} frame {}",
                        rec.head.track, rec.head.frame_no
                    ));
                }
                feed_frame(&sl, &rec.head, &rec.payload, &mut sink)?;
            }
            return Ok(());
        }
        // No index: one sequential scan over the frame region, decoding this
        // track's wanted frames as they arrive.
        let mut seen = vec![false; (f1 - f0 + 1) as usize];
        let meta_end = self.meta_end;
        let frames_end = self.frames_end;
        let frame_len = self.frame_len;
        walk_frames(
            &mut self.r,
            meta_end,
            frames_end,
            frame_len,
            &self.tracks,
            |t| t as usize == track,
            &mut |sf: SeqFrame| -> Result<(), String> {
                let f = sf.head.frame_no;
                if sf.head.track as usize == track && (f0..=f1).contains(&f) {
                    let payload = sf
                        .payload
                        .as_deref()
                        .ok_or_else(|| format!("track {track} frame {f}: payload unread"))?;
                    feed_frame(&sl, &sf.head, payload, &mut sink)?;
                    seen[(f - f0) as usize] = true;
                }
                Ok(())
            },
        )?;
        if let Some(miss) = seen.iter().position(|s| !*s) {
            return Err(format!(
                "track {track} frame {}: lost (corrupt or truncated)",
                f0 + miss as u32
            ));
        }
        Ok(())
    }

    /// Decode samples [start, start+len) of a track (clamped to the track).
    /// No pcm_crc check — it covers the whole track; `decode_track` verifies.
    pub fn decode_range(&mut self, track: usize, start: u64, len: u64) -> Result<Samples, String> {
        self.decode_range_par(track, start, len, 1)
    }

    /// `decode_range` with parallel frame decode — see
    /// `decode_range_with_par` for the scheduling rules.
    pub fn decode_range_par(
        &mut self,
        track: usize,
        start: u64,
        len: u64,
        threads: usize,
    ) -> Result<Samples, String> {
        let m = self
            .tracks
            .get(track)
            .cloned()
            .ok_or_else(|| format!("track {track} out of range"))?;
        let ch = m.channels as usize;
        // declared frames/nframes come from meta — reserve only a bounded
        // amount up front; extend_from_slice grows as frames actually decode
        let cap = len.min(m.frames.saturating_sub(start)).min(MAX_RESERVE) as usize;
        let mut out_i: Vec<Vec<i64>> = vec![Vec::with_capacity(cap); ch];
        let mut out_f: Vec<Vec<u64>> = vec![Vec::with_capacity(cap); ch];
        self.decode_range_with_par(track, start, len, threads, |s| {
            match s {
                Samples::Int(v) => {
                    for (c, x) in v.iter().enumerate() {
                        out_i[c].extend_from_slice(x);
                    }
                }
                Samples::Float(v) => {
                    for (c, x) in v.iter().enumerate() {
                        out_f[c].extend_from_slice(x);
                    }
                }
            }
            Ok(())
        })?;
        Ok(if m.sample_fmt == FMT_INT {
            Samples::Int(out_i)
        } else {
            Samples::Float(out_f)
        })
    }

    /// Total coded bytes of a track's frame records (headers + payloads +
    /// crcs). Reads frame headers via the index when present, else a
    /// sequential header walk.
    pub fn coded_bytes(&mut self, track: usize) -> Result<u64, String> {
        if self.tracks.get(track).is_none() {
            return Err(format!("track {track} out of range"));
        }
        let mut sum = 0u64;
        if let Some(ix) = &self.index {
            for &off in &ix[track] {
                let h = read_head(&mut self.r, off)?;
                sum += FRAME_HDR_LEN as u64 + u64::from(h.payload_len) + 4;
            }
            return Ok(sum);
        }
        let scan = frames_sequential(
            &mut self.r,
            self.meta_end,
            self.frames_end,
            self.frame_len,
            &self.tracks,
            |_| false,
        )?;
        for sf in scan.frames {
            if sf.head.track as usize == track {
                sum += FRAME_HDR_LEN as u64 + u64::from(sf.head.payload_len) + 4;
            }
        }
        Ok(sum)
    }

    /// Decode a whole track and verify its pcm_crc incrementally — each
    /// frame's canonical PCM bytes are hashed as produced, no full-track
    /// byte buffer.
    pub fn decode_track(&mut self, track: usize) -> Result<Samples, String> {
        self.decode_track_par(track, 1)
    }

    /// `decode_track` with parallel frame decode — see
    /// `decode_range_with_par` for the scheduling rules. pcm_crc still
    /// verifies over the ordered stream.
    pub fn decode_track_par(&mut self, track: usize, threads: usize) -> Result<Samples, String> {
        let m = self
            .tracks
            .get(track)
            .cloned()
            .ok_or_else(|| format!("track {track} out of range"))?;
        let ch = m.channels as usize;
        let cap = m.frames.min(MAX_RESERVE) as usize;
        let mut out_i: Vec<Vec<i64>> = vec![Vec::with_capacity(cap); ch];
        let mut out_f: Vec<Vec<u64>> = vec![Vec::with_capacity(cap); ch];
        let mut crc = Crc32::new();
        self.decode_range_with_par(track, 0, m.frames, threads, |s| {
            crc.update(&samples_to_pcm(&s, m.bits));
            match s {
                Samples::Int(v) => {
                    for (c, x) in v.iter().enumerate() {
                        out_i[c].extend_from_slice(x);
                    }
                }
                Samples::Float(v) => {
                    for (c, x) in v.iter().enumerate() {
                        out_f[c].extend_from_slice(x);
                    }
                }
            }
            Ok(())
        })?;
        if crc.finish() != m.pcm_crc {
            return Err(format!("track {track}: pcm_crc mismatch"));
        }
        Ok(if m.sample_fmt == FMT_INT {
            Samples::Int(out_i)
        } else {
            Samples::Float(out_f)
        })
    }
}

/// The `[start, end)` slice parameters shared by `feed_frame` calls.
struct Slice<'a> {
    track: usize,
    m: &'a TrackInfo,
    start: u64,
    end: u64,
    fl: u64,
}

/// Decode one frame payload and pass its `[start, end)` slice to `sink`.
fn feed_frame(
    sl: &Slice<'_>,
    head: &FrameHead,
    payload: &[u8],
    sink: &mut impl FnMut(Samples) -> Result<(), String>,
) -> Result<(), String> {
    feed_decoded(sl, head, decode_frame(sl.track, sl.m, head, payload)?, sink)
}

/// Pass the `[start, end)` slice of an already-decoded frame to `sink`.
fn feed_decoded(
    sl: &Slice<'_>,
    head: &FrameHead,
    s: Samples,
    sink: &mut impl FnMut(Samples) -> Result<(), String>,
) -> Result<(), String> {
    let base = u64::from(head.frame_no) * sl.fl;
    let lo = sl.start.saturating_sub(base) as usize;
    let hi = sl.end.saturating_sub(base) as usize;
    sink(match s {
        Samples::Int(v) => Samples::Int(
            v.iter()
                .map(|c| c[lo.min(c.len())..hi.min(c.len())].to_vec())
                .collect(),
        ),
        Samples::Float(v) => Samples::Float(
            v.iter()
                .map(|c| c[lo.min(c.len())..hi.min(c.len())].to_vec())
                .collect(),
        ),
    })
}

/// `n` must equal the meta-implied length (frame_len, or the remainder on the
/// track's last frame) and fmode/e must agree with the declared format.
fn check_frame_head(h: &FrameHead, m: &TrackInfo, frame_len: u32) -> Result<(), String> {
    let expect = m
        .frames
        .saturating_sub(u64::from(h.frame_no) * u64::from(frame_len))
        .min(u64::from(frame_len));
    if expect == 0 || u64::from(h.n) != expect {
        return Err("n disagrees with meta".into());
    }
    if !fmode_ok(m, h.fmode) {
        return Err(format!("fmode {} inconsistent with fmt", h.fmode));
    }
    if h.fmode == 0 && h.e != 0 {
        return Err("nonzero e on int frame".into());
    }
    Ok(())
}

/// Decode one frame payload through the shared codec.
fn decode_frame(
    track: usize,
    m: &TrackInfo,
    head: &FrameHead,
    payload: &[u8],
) -> Result<Samples, String> {
    let ctx = format!("track {track} frame {}", head.frame_no);
    let float = match head.fmode {
        0 => None,
        1 | 2 => Some(FloatMeta {
            mw: 23,
            e: head.e,
            stream_bits: 32,
            pcm_bits: u16::from(m.bits),
        }),
        3 => Some(FloatMeta {
            mw: 52,
            e: head.e,
            stream_bits: 64,
            pcm_bits: u16::from(m.bits),
        }),
        other => return Err(format!("{ctx}: unknown fmode {other}")),
    };
    let meta = TrackMeta {
        channels: m.channels as usize,
        frames: head.n as usize,
        float,
    };
    let mut br = BitReader::new(payload);
    let core = codec::read_track(&mut br, &meta).map_err(|e| format!("{ctx}: {e}"))?;
    if br.overrun {
        return Err(format!("{ctx}: bitstream overrun"));
    }
    Ok(if m.sample_fmt == FMT_INT {
        let half = 1i64 << (u32::from(m.bits) - 1);
        if core
            .x
            .iter()
            .flatten()
            .any(|&v| !(-half..half).contains(&v))
        {
            return Err(format!("{ctx}: sample out of range for {}-bit", m.bits));
        }
        Samples::Int(core.x)
    } else {
        Samples::Float(core.words.ok_or_else(|| format!("{ctx}: no float words"))?)
    })
}

// ------------------------------------------------------------- sequential

/// One frame seen while scanning the file's frame region in order.
pub struct SeqFrame {
    pub off: u64,
    pub head: FrameHead,
    /// Bytes skipped while resynchronizing before this frame, if any.
    pub skipped: Option<(u64, u64)>,
    /// CRC-verified payload (only for tracks `want`ed by the caller).
    pub payload: Option<Vec<u8>>,
}

pub struct SeqScan {
    pub frames: Vec<SeqFrame>,
    /// Bytes dropped at the end of the stream while resynchronizing, if any.
    pub trailing_skip: Option<(u64, u64)>,
}

fn read_head<R: Read + Seek>(r: &mut R, off: u64) -> Result<FrameHead, String> {
    let mut b = [0u8; FRAME_HDR_LEN];
    r.seek(SeekFrom::Start(off))
        .map_err(|e| format!("seek: {e}"))?;
    r.read_exact(&mut b).map_err(|e| format!("read: {e}"))?;
    parse_frame_head(&b)
}

/// Plausibility for resync/walking: bounds + consistency with the metas.
fn plausible(h: &FrameHead, tracks: &[TrackInfo], frame_len: u32, off: u64, end: u64) -> bool {
    let Some(m) = tracks.get(h.track as usize) else {
        return false;
    };
    let total = (FRAME_HDR_LEN as u64)
        .checked_add(u64::from(h.payload_len))
        .and_then(|x| x.checked_add(4));
    total.is_some_and(|t| off + t <= end) && check_frame_head(h, m, frame_len).is_ok()
}

/// Scan forward from `off` for the next position whose header is plausible
/// AND whose frame CRC verifies. Returns (offset, head, full frame bytes).
fn resync<R: Read + Seek>(
    r: &mut R,
    mut off: u64,
    end: u64,
    frame_len: u32,
    tracks: &[TrackInfo],
) -> Result<Option<(u64, FrameHead, Vec<u8>)>, String> {
    while off + FRAME_HDR_LEN as u64 + 4 <= end {
        if let Ok(h) = read_head(r, off)
            && plausible(&h, tracks, frame_len, off, end)
        {
            let total = FRAME_HDR_LEN as u64 + u64::from(h.payload_len) + 4;
            let mut buf = vec![0u8; total as usize];
            r.seek(SeekFrom::Start(off))
                .map_err(|e| format!("seek: {e}"))?;
            r.read_exact(&mut buf).map_err(|e| format!("read: {e}"))?;
            let (body, tail) = buf.split_at(buf.len() - 4);
            if crc32(body) == u32le(tail, 0) {
                return Ok(Some((off, h, buf)));
            }
        }
        off += 1;
    }
    Ok(None)
}

/// The frame-walk engine behind `frames_sequential`: hands each accepted
/// frame to `cb` and returns the trailing skipped byte range, if any. The
/// walk covers `[meta_end, frames_end)` — `frames_end` is the file's
/// `index_off` when an index exists (frames end where the index starts),
/// else the file length.
fn walk_frames<R: Read + Seek>(
    r: &mut R,
    meta_end: u64,
    frames_end: u64,
    frame_len: u32,
    tracks: &[TrackInfo],
    want: impl Fn(u8) -> bool,
    cb: &mut impl FnMut(SeqFrame) -> Result<(), String>,
) -> Result<Option<(u64, u64)>, String> {
    let mut expected: HashMap<u8, u32> = HashMap::new();
    let mut off = meta_end;
    let mut gap_allowed = false; // a skip was just reported: next frame may miss predecessors
    let mut trailing_skip = None;
    type Accepted = (u64, FrameHead, Vec<u8>, Option<(u64, u64)>);
    'outer: while off + FRAME_HDR_LEN as u64 + 4 <= frames_end {
        // Try to accept the frame at `off`.
        let mut accepted: Option<Accepted> = None;
        if let Ok(h) = read_head(r, off)
            && plausible(&h, tracks, frame_len, off, frames_end)
        {
            let seq_ok = h.frame_no == *expected.get(&h.track).unwrap_or(&0);
            if !seq_ok && !gap_allowed {
                return Err(format!(
                    "track {} frame {}: out of sequence",
                    h.track, h.frame_no
                ));
            }
            let total = FRAME_HDR_LEN as u64 + u64::from(h.payload_len) + 4;
            if want(h.track) || !seq_ok {
                let mut buf = vec![0u8; total as usize];
                r.seek(SeekFrom::Start(off))
                    .map_err(|e| format!("seek: {e}"))?;
                r.read_exact(&mut buf).map_err(|e| format!("read: {e}"))?;
                let (body, tail) = buf.split_at(buf.len() - 4);
                if crc32(body) == u32le(tail, 0) {
                    accepted = Some((off, h, buf, None));
                }
                // else: fall through to resync below
            } else {
                // Unwanted, in-sequence: walk past without reading.
                accepted = Some((off, h, Vec::new(), None));
            }
        }
        // Resync when the frame at `off` was implausible or failed CRC.
        if accepted.is_none() {
            match resync(r, off + 1, frames_end, frame_len, tracks)? {
                Some((foff, h, buf)) => accepted = Some((foff, h, buf, Some((off, foff)))),
                None => {
                    trailing_skip = Some((off, frames_end));
                    break 'outer;
                }
            }
        }
        let (foff, head, body, skipped) = accepted.unwrap();
        if skipped.is_some() {
            gap_allowed = true;
        }
        let payload = if want(head.track) {
            Some(body[FRAME_HDR_LEN..body.len() - 4].to_vec())
        } else {
            None
        };
        expected.insert(head.track, head.frame_no + 1);
        off = foff + FRAME_HDR_LEN as u64 + u64::from(head.payload_len) + 4;
        cb(SeqFrame {
            off: foff,
            head,
            skipped,
            payload,
        })?;
    }
    if off < frames_end && trailing_skip.is_none() {
        trailing_skip = Some((off, frames_end));
    }
    Ok(trailing_skip)
}

/// Iterate frames in file order from `meta_end` up to `frames_end` (the
/// index offset when the file has one, else the file length). Each emitted
/// frame has a plausible header; `want`ed frames additionally carry a
/// CRC-verified payload. On a parse/CRC failure, resynchronizes by scanning
/// forward byte-by-byte for the next frame with a plausible header *and* a
/// valid CRC; the skipped byte range is reported on the following
/// `SeqFrame` (or as `trailing_skip`). Per-track `frame_no` sequences are
/// enforced: a gap is only accepted when a skip was just reported (a lost
/// frame), otherwise it is an error.
///
/// Unwanted frames are walked via header+payload_len alone (no payload
/// read), which is what makes `--only` cheap.
pub fn frames_sequential<R: Read + Seek>(
    r: &mut R,
    meta_end: u64,
    frames_end: u64,
    frame_len: u32,
    tracks: &[TrackInfo],
    want: impl Fn(u8) -> bool,
) -> Result<SeqScan, String> {
    let mut out = Vec::new();
    let trailing_skip = walk_frames(r, meta_end, frames_end, frame_len, tracks, want, &mut |f| {
        out.push(f);
        Ok(())
    })?;
    Ok(SeqScan {
        frames: out,
        trailing_skip,
    })
}
