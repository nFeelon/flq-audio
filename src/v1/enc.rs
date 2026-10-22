//! v1 encoder: streams WAV/AIFF from disk in frame-sized chunks (FLAC/ALAC
//! decoded whole via symphonia), codes each frame independently through the
//! shared codec, patches the placeholder fields at finish.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::bits::{BitWriter, Crc32, crc32};
use crate::codec::{self, DBit, FloatData, TrackCtx};
use crate::formats::{self, Sniff};
use crate::v1::*;
use crate::{aiff, wav};

#[derive(Clone, Debug)]
pub struct V1Opts {
    pub preset: u8,     // 0, 5 or 8
    pub threads: usize, // 0 = all cores
    pub frame_len: u32, // multiple of 4096, 4096..=262144
    pub index: bool,
}

impl Default for V1Opts {
    fn default() -> Self {
        Self {
            preset: 5,
            threads: 0,
            frame_len: 16384,
            index: true,
        }
    }
}

pub struct V1Input {
    pub name: String,
    pub path: PathBuf,
}

pub struct V1TrackStats {
    pub name: String,
    pub input_bytes: u64,
    /// Sum of the track's frame records (headers + payloads + crcs).
    pub coded_bytes: u64,
    pub frames: u64,
    pub nframes: u32,
    pub rate: u32,
    pub channels: u8,
    pub sample_fmt: u8,
    pub bits: u8,
}

pub struct V1Stats {
    pub tracks: Vec<V1TrackStats>,
    /// Header + meta + index bytes (everything that is not a frame record).
    pub header_meta_index_bytes: u64,
    pub total_bytes: u64,
}

// ------------------------------------------------------------------ sources

enum Src {
    /// Streaming PCM from disk: `r` sits at the first PCM byte; `left` counts
    /// readable whole-frame bytes. `aiff` converts native chunks to the WAV
    /// convention.
    Stream {
        r: BufReader<File>,
        left: u64,
        align: usize,
        aiff: Option<aiff::AiffHead>,
    },
    /// Whole track already decoded (FLAC/ALAC) — integer samples only.
    /// shortcut: holds all samples in memory; streaming a symphonia source
    /// frame-by-frame is possible but not needed yet.
    Mem { x: Vec<Vec<i64>>, pos: usize },
}

/// (frames actually read, integer samples, float words when FMT_FLOAT).
type FramePcm = (usize, Vec<Vec<i64>>, Option<Vec<Vec<u64>>>);

struct TrackSrc {
    name: String,
    origin: u8,
    rate: u32,
    channels: u8,
    mask: u32,
    sample_fmt: u8,
    bits: u8,
    frames: u64,
    tags: Vec<u8>,
    input_bytes: u64,
    src: Src,
}

impl TrackSrc {
    fn nframes(&self, frame_len: u32) -> u32 {
        self.frames.div_ceil(u64::from(frame_len)) as u32
    }

    /// Up to `n` samples per channel: integer samples, or float words.
    /// Returns the actual whole-frame count (0 on exhausted input).
    fn read_frame(&mut self, n: usize) -> Result<FramePcm, String> {
        let ch = self.channels as usize;
        match &mut self.src {
            Src::Mem { x, pos } => {
                let out: Vec<Vec<i64>> = x.iter().map(|c| c[*pos..*pos + n].to_vec()).collect();
                *pos += n;
                Ok((n, out, None))
            }
            Src::Stream {
                r,
                left,
                align,
                aiff,
            } => {
                let want = ((n * *align) as u64).min(*left);
                let mut buf = vec![0u8; want as usize];
                let mut got = 0usize;
                while got < want as usize {
                    let n2 = r
                        .read(&mut buf[got..])
                        .map_err(|e| format!("{}: read: {e}", self.name))?;
                    if n2 == 0 {
                        break;
                    }
                    got += n2;
                }
                let whole = got - got % *align;
                *left = left.saturating_sub(whole as u64);
                buf.truncate(whole);
                if let Some(h) = aiff {
                    buf = h.to_wav_conv(&buf);
                }
                let n = whole / *align;
                Ok(if self.sample_fmt == FMT_FLOAT {
                    (
                        n,
                        Vec::new(),
                        Some(words_of(&buf, (self.bits / 8) as usize, ch)),
                    )
                } else {
                    (n, ints_of(&buf, self.bits, ch), None)
                })
            }
        }
    }
}

fn ints_of(b: &[u8], bits: u8, ch: usize) -> Vec<Vec<i64>> {
    let bps = (bits / 8) as usize;
    let frames = b.len().checked_div(ch * bps).unwrap_or(0);
    let mut out = vec![vec![0i64; frames]; ch];
    for (f, frame) in b.chunks_exact(ch * bps).enumerate() {
        for (c, oc) in out.iter_mut().enumerate() {
            let s = &frame[c * bps..(c + 1) * bps];
            oc[f] = match bits {
                8 => i64::from(s[0]) - 128,
                16 => i64::from(i16::from_le_bytes([s[0], s[1]])),
                24 => i64::from((u32::from_le_bytes([s[0], s[1], s[2], 0]) << 8) as i32 >> 8),
                _ => i64::from(i32::from_le_bytes([s[0], s[1], s[2], s[3]])),
            };
        }
    }
    out
}

fn words_of(b: &[u8], cb: usize, ch: usize) -> Vec<Vec<u64>> {
    let frames = b.len().checked_div(ch * cb).unwrap_or(0);
    let mut out = vec![vec![0u64; frames]; ch];
    for (f, frame) in b.chunks_exact(ch * cb).enumerate() {
        for (c, oc) in out.iter_mut().enumerate() {
            let s = &frame[c * cb..(c + 1) * cb];
            oc[f] = match cb {
                4 => u64::from(u32::from_le_bytes([s[0], s[1], s[2], s[3]])),
                _ => u64::from_le_bytes(s[..8].try_into().unwrap()),
            };
        }
    }
    out
}

fn lossy_err(name: &str, kind: &str) -> String {
    format!("{name}: lossy input ({kind}) not supported — flq is lossless only")
}

const HEAD_READ: usize = 1 << 16;

fn open_input(inp: &V1Input) -> Result<TrackSrc, String> {
    if !valid_name(&inp.name) {
        return Err(format!("invalid track name {:?}", inp.name));
    }
    let f = File::open(&inp.path).map_err(|e| format!("{}: {e}", inp.name))?;
    let file_len = f
        .metadata()
        .map_err(|e| format!("{}: {e}", inp.name))?
        .len();
    let mut br = BufReader::with_capacity(1 << 20, f);
    let mut head = vec![0u8; HEAD_READ.min(file_len as usize)];
    br.read_exact(&mut head)
        .map_err(|e| format!("{}: read header: {e}", inp.name))?;
    match formats::sniff(&head) {
        Sniff::Wav => {
            let (fmt, pcm_start, size) =
                wav::scan_prefix(&head).map_err(|e| format!("{}: {e}", inp.name))?;
            if fmt.rate == 0 {
                return Err(format!("{}: sample rate 0", inp.name));
            }
            let avail = u64::from(size).min(file_len - pcm_start as u64);
            let frames = avail / u64::from(fmt.block_align);
            br.seek(SeekFrom::Start(pcm_start as u64))
                .map_err(|e| format!("{}: seek: {e}", inp.name))?;
            Ok(TrackSrc {
                name: inp.name.clone(),
                origin: ORIGIN_WAV,
                rate: fmt.rate,
                channels: fmt.channels as u8,
                mask: fmt.channel_mask,
                sample_fmt: if fmt.tag == 3 { FMT_FLOAT } else { FMT_INT },
                bits: fmt.bits as u8,
                frames,
                tags: Vec::new(),
                input_bytes: file_len,
                src: Src::Stream {
                    r: br,
                    left: frames * u64::from(fmt.block_align),
                    align: fmt.block_align as usize,
                    aiff: None,
                },
            })
        }
        Sniff::Aiff => {
            let h = aiff::scan_prefix(&head).map_err(|e| format!("{}: {e}", inp.name))?;
            if h.fmt.rate == 0 {
                return Err(format!("{}: sample rate 0", inp.name));
            }
            if h.fmt.channels == 0 || h.fmt.channels > u16::from(MAX_CHANNELS) {
                return Err(format!(
                    "{}: bad channel count {}",
                    inp.name, h.fmt.channels
                ));
            }
            let align = u64::from(h.fmt.block_align);
            let avail = (h.avail as u64).min(file_len - h.pcm_start as u64);
            let frames = avail / align;
            br.seek(SeekFrom::Start(h.pcm_start as u64))
                .map_err(|e| format!("{}: seek: {e}", inp.name))?;
            Ok(TrackSrc {
                name: inp.name.clone(),
                origin: ORIGIN_AIFF,
                rate: h.fmt.rate,
                channels: h.fmt.channels as u8,
                mask: h.fmt.channel_mask,
                sample_fmt: if h.fmt.tag == 3 { FMT_FLOAT } else { FMT_INT },
                bits: h.fmt.bits as u8,
                frames,
                tags: Vec::new(),
                input_bytes: file_len,
                src: Src::Stream {
                    r: br,
                    left: frames * align,
                    align: align as usize,
                    aiff: Some(h),
                },
            })
        }
        Sniff::Flac => mem_track(inp, br, head, file_len, ORIGIN_FLAC),
        Sniff::Mp4(e) if &e == b"alac" => mem_track(inp, br, head, file_len, ORIGIN_ALAC),
        Sniff::Mp4(entry) => Err(lossy_err(
            &inp.name,
            if &entry == b"mp4a" { "m4a aac" } else { "m4a" },
        )),
        Sniff::Lossy(label) => Err(lossy_err(&inp.name, label)),
        Sniff::Unknown => Err(formats::UNSUPPORTED_MSG.into()),
    }
}

/// FLAC/ALAC input: decode the whole file via symphonia into a memory source.
/// shortcut: holds all samples in memory (streaming the decoder is possible
/// but not needed yet).
fn mem_track(
    inp: &V1Input,
    mut br: BufReader<File>,
    mut all: Vec<u8>,
    file_len: u64,
    origin: u8,
) -> Result<TrackSrc, String> {
    br.read_to_end(&mut all)
        .map_err(|e| format!("{}: read: {e}", inp.name))?;
    let s = if origin == ORIGIN_FLAC {
        formats::read_flac(&all).map_err(|e| format!("{}: {e}", inp.name))?
    } else {
        formats::read_m4a(&all).map_err(|e| format!("{}: {e}", inp.name))?
    };
    if s.channels == 0 || s.channels > u16::from(MAX_CHANNELS) || s.rate == 0 {
        return Err(format!("{}: bad channel/rate", inp.name));
    }
    if !matches!(s.bits, 8 | 16 | 24 | 32) {
        return Err(format!("{}: unsupported bit depth {}", inp.name, s.bits));
    }
    let frames = s.x.first().map_or(0, |c| c.len()) as u64;
    Ok(TrackSrc {
        name: inp.name.clone(),
        origin,
        rate: s.rate,
        channels: s.channels as u8,
        mask: 0,
        sample_fmt: FMT_INT,
        bits: s.bits as u8,
        frames,
        tags: s.tags,
        input_bytes: file_len,
        src: Src::Mem { x: s.x, pos: 0 },
    })
}

// ------------------------------------------------------------------ frames

/// One frame's gathered input: code streams + canonical PCM bytes.
struct Job {
    track: usize,
    frame_no: u32,
    n: u32,
    fmode: u8,
    e: i16,
    word_bits: u32,
    x: Vec<Vec<i64>>,
    d: Option<Vec<Vec<DBit>>>,
}

/// Per-frame float split:
/// f32 -> fmode 1 (mw 23, stream 32); f64 f32-exact in THIS frame -> fmode 2
/// (coded as f32, stream 32, pcm 64); otherwise fmode 3 (mw 52, stream 64).
fn float_frame(words: &[Vec<u64>], bits: u8) -> (u8, i16, Vec<Vec<i64>>, Vec<Vec<DBit>>, u32) {
    let (fmode, FloatData { e, streams, d }, word_bits) = if bits == 64 {
        let exact = words.iter().flatten().all(|&u| {
            let f = f64::from_bits(u);
            f.is_finite() && ((f as f32) as f64).to_bits() == u
        });
        if exact {
            let w32: Vec<Vec<u64>> = words
                .iter()
                .map(|c| {
                    c.iter()
                        .map(|&u| u64::from((f64::from_bits(u) as f32).to_bits()))
                        .collect()
                })
                .collect();
            (2u8, codec::split_float(&w32, 23), 32u32)
        } else {
            (3u8, codec::split_float(words, 52), 64u32)
        }
    } else {
        (1u8, codec::split_float(words, 23), 32u32)
    };
    (fmode, e, streams, d, word_bits)
}

/// Gather one frame: read its samples, feed the canonical PCM bytes into the
/// track crc, split floats per frame. `None` when the source is exhausted.
fn gather(
    t: usize,
    f: u32,
    n: usize,
    src: &mut TrackSrc,
    pcm_crc: &mut Crc32,
) -> Result<Option<Job>, String> {
    let (n, x, words) = src.read_frame(n)?;
    if n == 0 {
        return Ok(None);
    }
    let (fmode, e, x, d, word_bits, canon) = if let Some(w) = words {
        let canon = wav::words_to_pcm(&w, u16::from(src.bits), w.first().map_or(0, |c| c.len()));
        let (fmode, e, x, d, wb) = float_frame(&w, src.bits);
        (fmode, e, x, Some(d), wb, canon)
    } else {
        let canon = wav::ints_to_pcm(&x, u16::from(src.bits), x.first().map_or(0, |c| c.len()));
        (0u8, 0i16, x, None, 0u32, canon)
    };
    pcm_crc.update(&canon);
    Ok(Some(Job {
        track: t,
        frame_no: f,
        n: n as u32,
        fmode,
        e,
        word_bits,
        x,
        d,
    }))
}

fn code_job(j: &Job, preset: u8) -> Vec<u8> {
    let ctx = TrackCtx {
        x: &j.x,
        d: j.d.as_deref(),
        float_word_bits: j.word_bits,
    };
    let plan = codec::plan_track_with(&ctx, preset, 1);
    let mut bw = BitWriter::new();
    codec::write_track(&mut bw, &ctx, &plan);
    bw.into_bytes()
}

fn frame_record(j: &Job, payload: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(FRAME_HDR_LEN + payload.len() + 4);
    b.extend(FRAME_SYNC);
    b.push(j.track as u8);
    b.push(j.fmode);
    b.extend(j.e.to_le_bytes());
    b.extend(j.frame_no.to_le_bytes());
    b.extend(j.n.to_le_bytes());
    b.extend((payload.len() as u32).to_le_bytes());
    b.extend(payload);
    b.extend(crc32(&b).to_le_bytes());
    b
}

// ------------------------------------------------------------------ encode

pub fn encode<W: Write + Seek>(
    inputs: &[V1Input],
    out: &mut W,
    opts: &V1Opts,
) -> Result<V1Stats, String> {
    if inputs.is_empty() || inputs.len() > 255 {
        return Err("expected 1..=255 inputs".into());
    }
    if !matches!(opts.preset, 0 | 5 | 8) {
        return Err("preset must be 0, 5 or 8".into());
    }
    if !(MIN_FRAME_LEN..=MAX_FRAME_LEN).contains(&opts.frame_len) || opts.frame_len % 4096 != 0 {
        return Err("frame_len must be a multiple of 4096 within 4096..=262144".into());
    }
    let threads = if opts.threads == 0 {
        std::thread::available_parallelism().map_or(1, |n| n.get())
    } else {
        opts.threads
    };
    let fl = opts.frame_len;

    // Open all inputs up front (errors before writing anything).
    let mut srcs: Vec<TrackSrc> = inputs.iter().map(open_input).collect::<Result<_, _>>()?;

    // Meta with placeholders for frames/nframes/pcm_crc.
    let mut meta = Vec::new();
    let mut field_at: Vec<(usize, usize, usize)> = Vec::with_capacity(srcs.len());
    for s in &srcs {
        let info = TrackInfo {
            name: s.name.clone(),
            origin: s.origin,
            rate: s.rate,
            channels: s.channels,
            channel_mask: s.mask,
            sample_fmt: s.sample_fmt,
            bits: s.bits,
            frames: 0,
            nframes: 0,
            pcm_crc: 0,
            tags: s.tags.clone(),
        };
        field_at.push(put_track_meta(&mut meta, &info)?);
    }
    if meta.len() as u64 + 4 > MAX_META_LEN {
        return Err("meta exceeds 2^26 bytes".into());
    }
    let meta_len = (meta.len() + 4) as u32;

    // Header (index_off patched at finish).
    let mut hdr = Vec::with_capacity(HEADER_LEN);
    hdr.extend(MAGIC);
    hdr.push(VERSION);
    hdr.push(0u8); // flags
    hdr.push(srcs.len() as u8);
    hdr.push(0u8); // reserved
    hdr.extend(fl.to_le_bytes());
    hdr.extend(meta_len.to_le_bytes());
    hdr.extend(0u64.to_le_bytes()); // index_off placeholder
    hdr.extend(crc32(&hdr).to_le_bytes());
    out.write_all(&hdr).map_err(|e| format!("write: {e}"))?;
    let meta_abs = HEADER_LEN as u64;
    out.write_all(&meta).map_err(|e| format!("write: {e}"))?;
    out.write_all(&0u32.to_le_bytes())
        .map_err(|e| format!("write: {e}"))?; // meta crc placeholder

    // Frame loop: gather frame-major/track-minor jobs in batches of
    // 2*threads, code them in parallel, write in order — byte-identical for
    // any thread count.
    let mut pcm_crcs: Vec<Crc32> = srcs.iter().map(|_| Crc32::new()).collect();
    let mut done: Vec<u64> = vec![0; srcs.len()]; // frames consumed per track
    let mut enc_n: Vec<u64> = vec![0; srcs.len()]; // samples actually coded
    let mut coded: Vec<u64> = vec![0; srcs.len()]; // frame record bytes per track
    let mut index: Vec<Vec<u64>> = srcs
        .iter()
        .map(|s| Vec::with_capacity(s.nframes(fl) as usize))
        .collect();
    let batch_n = (2 * threads).max(1);
    let mut batch: Vec<Job> = Vec::with_capacity(batch_n);

    let flush = |batch: &mut Vec<Job>,
                 out: &mut W,
                 coded: &mut [u64],
                 index: &mut [Vec<u64>]|
     -> Result<(), String> {
        let payloads: Vec<Vec<u8>> = if threads > 1 && batch.len() > 1 {
            let chunk = batch.len().div_ceil(threads.min(batch.len()));
            let mut slots: Vec<Option<Vec<u8>>> = batch.iter().map(|_| None).collect();
            let jobs = &*batch;
            std::thread::scope(|sc| {
                let mut at = 0usize;
                for slice in slots.chunks_mut(chunk) {
                    let base = at;
                    at += slice.len();
                    sc.spawn(move || {
                        for (i, slot) in slice.iter_mut().enumerate() {
                            *slot = Some(code_job(&jobs[base + i], opts.preset));
                        }
                    });
                }
            });
            slots.into_iter().map(|s| s.unwrap()).collect()
        } else {
            batch.iter().map(|j| code_job(j, opts.preset)).collect()
        };
        for (j, p) in batch.drain(..).zip(payloads) {
            let off = out.stream_position().map_err(|e| format!("seek: {e}"))?;
            let rec = frame_record(&j, &p);
            coded[j.track] += rec.len() as u64;
            index[j.track].push(off);
            out.write_all(&rec).map_err(|e| format!("write: {e}"))?;
        }
        Ok(())
    };

    let mut f = 0u32;
    'frames: loop {
        let mut any = false;
        for (t, s) in srcs.iter_mut().enumerate() {
            if u64::from(f) >= s.nframes(fl) as u64 || done[t] >= s.frames {
                continue;
            }
            let n = (s.frames - done[t]).min(u64::from(fl)) as usize;
            match gather(t, f, n, s, &mut pcm_crcs[t])? {
                Some(job) => {
                    done[t] += u64::from(job.n);
                    enc_n[t] += u64::from(job.n);
                    batch.push(job);
                }
                None => done[t] = s.frames,
            }
            any = true;
        }
        if !any {
            break 'frames;
        }
        if batch.len() >= batch_n {
            flush(&mut batch, out, &mut coded, &mut index)?;
        }
        f += 1;
    }
    flush(&mut batch, out, &mut coded, &mut index)?;

    // Index.
    let index_off = if opts.index {
        let at = out.stream_position().map_err(|e| format!("seek: {e}"))?;
        let mut ix = Vec::new();
        for offs in &index {
            for &o in offs {
                put_u64(&mut ix, o);
            }
        }
        let c = crc32(&ix);
        out.write_all(&ix).map_err(|e| format!("write: {e}"))?;
        out.write_all(&c.to_le_bytes())
            .map_err(|e| format!("write: {e}"))?;
        at
    } else {
        0
    };

    // Patch the placeholders in the in-memory meta (`done`/`index` hold what
    // was actually written), then rewrite meta + crc in one go.
    for (t, ((frames_at, nframes_at, crc_at), crc)) in field_at.iter().zip(pcm_crcs).enumerate() {
        meta[*frames_at..*frames_at + 8].copy_from_slice(&enc_n[t].to_le_bytes());
        meta[*nframes_at..*nframes_at + 4].copy_from_slice(&(index[t].len() as u32).to_le_bytes());
        meta[*crc_at..*crc_at + 4].copy_from_slice(&crc.finish().to_le_bytes());
    }
    let end = out.stream_position().map_err(|e| format!("seek: {e}"))?;
    out.seek(SeekFrom::Start(meta_abs))
        .map_err(|e| format!("seek: {e}"))?;
    out.write_all(&meta).map_err(|e| format!("write: {e}"))?;
    out.write_all(&crc32(&meta).to_le_bytes())
        .map_err(|e| format!("write: {e}"))?;
    // header: index_off + recomputed crc
    out.seek(SeekFrom::Start(0))
        .map_err(|e| format!("seek: {e}"))?;
    let mut h2 = hdr;
    h2[16..24].copy_from_slice(&index_off.to_le_bytes());
    let hcrc = crc32(&h2[..24]);
    h2[24..28].copy_from_slice(&hcrc.to_le_bytes());
    out.write_all(&h2).map_err(|e| format!("write: {e}"))?;
    out.seek(SeekFrom::Start(end))
        .map_err(|e| format!("seek: {e}"))?;

    let tracks = srcs
        .iter()
        .zip(&coded)
        .zip(&index)
        .enumerate()
        .map(|(t, ((s, &c), ix))| V1TrackStats {
            name: s.name.clone(),
            input_bytes: s.input_bytes,
            coded_bytes: c,
            frames: enc_n[t],
            nframes: ix.len() as u32,
            rate: s.rate,
            channels: s.channels,
            sample_fmt: s.sample_fmt,
            bits: s.bits,
        })
        .collect::<Vec<_>>();
    let frame_bytes: u64 = coded.iter().sum();
    let ix_bytes = if opts.index {
        index.iter().map(|o| o.len() as u64 * 8).sum::<u64>() + 4
    } else {
        0
    };
    Ok(V1Stats {
        tracks,
        header_meta_index_bytes: HEADER_LEN as u64 + u64::from(meta_len) + ix_bytes,
        total_bytes: HEADER_LEN as u64 + u64::from(meta_len) + frame_bytes + ix_bytes,
    })
}
