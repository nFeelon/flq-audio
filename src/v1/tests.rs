use super::dec::{Reader, Samples, frames_sequential, samples_to_pcm};
use super::enc::{V1Input, V1Opts, encode};
use super::*;
use crate::bits::crc32;
use crate::formats;
use crate::wav::tests::{fmt_chunk, riff};
use std::io::Cursor;
use std::path::PathBuf;

// ---------------------------------------------------------------- builders

static TMP_N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn tmp(path_tag: &str, bytes: &[u8]) -> PathBuf {
    let n = TMP_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "flqv1_{}_{}_{n}",
        std::process::id(),
        path_tag.replace(['/', '\\', ':'], "_")
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("input.bin");
    std::fs::write(&p, bytes).unwrap();
    p
}

fn enc(files: &[(&str, Vec<u8>)], opts: &V1Opts) -> Vec<u8> {
    let inputs: Vec<V1Input> = files
        .iter()
        .map(|(n, b)| V1Input {
            name: n.to_string(),
            path: tmp(&format!("{}_{}", n.replace('.', "_"), opts.preset), b),
        })
        .collect();
    let mut cur = Cursor::new(Vec::new());
    encode(&inputs, &mut cur, opts).unwrap();
    cur.into_inner()
}

fn enc_err(files: &[(&str, Vec<u8>)], opts: &V1Opts) -> String {
    let inputs: Vec<V1Input> = files
        .iter()
        .map(|(n, b)| V1Input {
            name: n.to_string(),
            path: tmp(n, b),
        })
        .collect();
    let mut cur = Cursor::new(Vec::new());
    encode(&inputs, &mut cur, opts).err().unwrap()
}

fn rd(bytes: &[u8]) -> Reader<Cursor<&[u8]>> {
    Reader::open(Cursor::new(bytes)).unwrap()
}

fn opts() -> V1Opts {
    V1Opts {
        preset: 8,
        threads: 1,
        frame_len: 4096,
        index: true,
    }
}

/// Pseudo-random int samples inside the bit range.
fn int_samples(ch: usize, bits: u16, frames: usize, seed: u64) -> Vec<Vec<i64>> {
    let half = 1i64 << (bits - 1);
    let mut s = seed;
    let mut x = vec![vec![0i64; frames]; ch];
    for xc in x.iter_mut() {
        for v in xc.iter_mut() {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *v = ((s >> 33) as i64).rem_euclid(2 * half) - half;
        }
    }
    x
}

fn mk_wav(tag: u16, ch: u16, rate: u32, bits: u16, mask: u32, pcm: Vec<u8>) -> Vec<u8> {
    if mask == 0 {
        return riff(&[(b"fmt ", fmt_chunk(tag, ch, rate, bits)), (b"data", pcm)]);
    }
    // WAVE_FORMAT_EXTENSIBLE
    let align = ch * (bits / 8);
    let mut f = Vec::new();
    f.extend(0xFFFEu16.to_le_bytes());
    f.extend(ch.to_le_bytes());
    f.extend(rate.to_le_bytes());
    f.extend((rate * u32::from(align)).to_le_bytes());
    f.extend(align.to_le_bytes());
    f.extend(bits.to_le_bytes());
    f.extend(22u16.to_le_bytes());
    f.extend(bits.to_le_bytes());
    f.extend(mask.to_le_bytes());
    f.extend(tag.to_le_bytes());
    f.extend([
        0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
    ]);
    riff(&[(b"fmt ", f), (b"data", pcm)])
}

fn int_wav(ch: u16, bits: u16, mask: u32, frames: usize, seed: u64) -> (Vec<u8>, Vec<Vec<i64>>) {
    let x = int_samples(ch as usize, bits, frames, seed);
    let pcm = crate::wav::ints_to_pcm(&x, bits, frames);
    (mk_wav(1, ch, 44100, bits, mask, pcm), x)
}

fn float_wav(ch: u16, bits: u16, words: &[Vec<u64>]) -> Vec<u8> {
    let frames = words[0].len();
    let pcm = crate::wav::words_to_pcm(words, bits, frames);
    mk_wav(3, ch, 48000, bits, 0, pcm)
}

fn expect_int(s: Samples) -> Vec<Vec<i64>> {
    match s {
        Samples::Int(x) => x,
        _ => panic!("expected int samples"),
    }
}
fn expect_float(s: Samples) -> Vec<Vec<u64>> {
    match s {
        Samples::Float(w) => w,
        _ => panic!("expected float samples"),
    }
}

// ---------------------------------------------------------------- roundtrip

#[test]
fn roundtrip_int_formats() {
    for bits in [8u16, 16, 24, 32] {
        for &(ch, mask) in &[(1u16, 0u32), (2, 0), (6, 0x3f)] {
            for &frames in &[1usize, 4095, 4096, 4097, 3 * 4096 + 123] {
                let (w, x) = int_wav(ch, bits, mask, frames, bits as u64 * 100 + ch as u64);
                let file = enc(&[("t.wav", w)], &opts());
                let mut r = rd(&file);
                let m = &r.tracks()[0];
                assert_eq!(m.channels as u16, ch);
                assert_eq!(m.bits as u16, bits);
                assert_eq!(m.channel_mask, mask);
                assert_eq!(m.frames, frames as u64);
                assert_eq!(
                    expect_int(r.decode_track(0).unwrap()),
                    x,
                    "bits {bits} ch {ch} n {frames}"
                );
            }
        }
    }
}

#[test]
fn roundtrip_zero_frames() {
    let (w, _) = int_wav(2, 16, 0, 0, 7);
    let file = enc(&[("e.wav", w)], &opts());
    let mut r = rd(&file);
    assert_eq!(r.tracks()[0].nframes, 0);
    assert_eq!(expect_int(r.decode_track(0).unwrap()), vec![vec![]; 2]);
}

#[test]
fn roundtrip_float() {
    let f32w: Vec<Vec<u64>> = (0..2)
        .map(|c| {
            (0..5000)
                .map(|i| {
                    u64::from(
                        f32::from_bits(0x3f000000 + (i * 977 + c * 131) as u32 % 0x00400000)
                            .to_bits(),
                    )
                })
                .collect()
        })
        .collect();
    let file = enc(&[("f.wav", float_wav(2, 32, &f32w))], &opts());
    assert_eq!(expect_float(rd(&file).decode_track(0).unwrap()), f32w);

    // f64, all f32-exact -> stored as fmode 2
    let exact: Vec<Vec<u64>> = (0..2)
        .map(|c| {
            (0..5000)
                .map(|i| {
                    let f = f32::from_bits(0x3f000000 + (i * 61 + c * 97) as u32 % 0x00400000);
                    (f as f64).to_bits()
                })
                .collect()
        })
        .collect();
    let file = enc(&[("d.wav", float_wav(2, 64, &exact))], &opts());
    let mut r = rd(&file);
    assert_eq!(r.tracks()[0].bits, 64);
    assert_eq!(expect_float(r.decode_track(0).unwrap()), exact);

    // true f64 (1.1 etc. are not f32-exact) -> fmode 3
    let tru: Vec<Vec<u64>> = (0..2)
        .map(|c| {
            (0..5000)
                .map(|i| (1.1f64 + (i as f64) * 0.00006103515625001337 + c as f64).to_bits())
                .collect()
        })
        .collect();
    let file = enc(&[("d.wav", float_wav(2, 64, &tru))], &opts());
    assert_eq!(expect_float(rd(&file).decode_track(0).unwrap()), tru);
}

// ------------------------------------------------------------------- range

#[test]
fn decode_range_equals_slice() {
    let (w, x) = int_wav(2, 16, 0, 3 * 4096 + 123, 9);
    let file = enc(&[("t.wav", w)], &opts());
    for &(start, len) in &[
        (0u64, 100u64),  // start
        (5000, 2000),    // middle
        (12000, 411),    // end
        (4000, 200),     // spanning a frame boundary
        (100, u64::MAX), // past end (clamped)
    ] {
        let mut r = rd(&file);
        let part = expect_int(r.decode_range(0, start, len).unwrap());
        let want_len = (x[0].len() as u64 - start.min(x[0].len() as u64)).min(len) as usize;
        let start = start as usize;
        for c in 0..2 {
            assert_eq!(
                part[c],
                x[c][start..start + want_len],
                "range {start}+{len} ch {c}"
            );
        }
    }
    // beyond end -> error
    assert!(rd(&file).decode_range(0, 12411, 1).is_err());
}

// ----------------------------------------------------------------- threads

#[test]
fn thread_parity() {
    let (a, _) = int_wav(2, 16, 0, 3 * 4096 + 123, 3);
    let (b, _) = int_wav(1, 24, 0, 4097, 5);
    let files = [("a.wav", a), ("b.wav", b)];
    let one = enc(&files, &opts());
    let four = enc(
        &files,
        &V1Opts {
            threads: 4,
            ..opts()
        },
    );
    assert_eq!(one, four, "thread counts must produce identical bytes");
}

// --------------------------------------------------------------- corrupt

fn index_offsets(file: &[u8], track: usize, nframes: &[u32]) -> Vec<u64> {
    let at = u64le(file, 16) as usize;
    let base: usize = nframes[..track].iter().map(|&n| n as usize * 8).sum();
    (0..nframes[track] as usize)
        .map(|i| u64le(file, at + base + i * 8))
        .collect()
}

#[test]
fn corrupt_frame_isolated() {
    let (a, _) = int_wav(2, 16, 0, 3 * 4096, 3);
    let (b, xb) = int_wav(1, 16, 0, 2 * 4096, 5);
    let mut file = enc(&[("a.wav", a), ("b.wav", b)], &opts());
    let nframes: Vec<u32> = rd(&file).tracks().iter().map(|t| t.nframes).collect();
    let t0 = index_offsets(&file, 0, &nframes);
    // corrupt one payload byte of track 0 frame 1
    file[t0[1] as usize + FRAME_HDR_LEN] ^= 0xFF;

    let mut r = rd(&file);
    let e = r.read_frame(0, 1).err().unwrap();
    assert!(e.contains("track 0 frame 1"), "{e}");
    // other track-0 frames and all track-1 frames still decode
    assert_eq!(r.read_frame(0, 0).unwrap().frames(), 4096);
    assert_eq!(r.read_frame(0, 2).unwrap().frames(), 4096);
    for f in 0..nframes[1] {
        r.read_frame(1, f).unwrap();
    }
    assert_eq!(expect_int(r.decode_track(1).unwrap()), xb);
    // track 0's whole-track decode fails on the missing frame
    assert!(r.decode_track(0).is_err());

    // sequential scan resyncs and reports the skipped region (frames end at
    // the index offset, not at EOF)
    let meta_end = HEADER_LEN as u64 + u64::from(u32le(&file, 12));
    let frames_end = u64le(&file, 16);
    let tracks = r.tracks().to_vec();
    let mut cur = Cursor::new(&file[..]);
    let scan = frames_sequential(&mut cur, meta_end, frames_end, 4096, &tracks, |_| true).unwrap();
    let t1 = index_offsets(&file, 1, &nframes);
    let skipped: Vec<(u64, u64)> = scan.frames.iter().filter_map(|f| f.skipped).collect();
    assert_eq!(
        skipped.len(),
        1,
        "exactly the corrupt frame should be skipped"
    );
    assert_eq!(skipped[0], (t0[1], t1[1]));
    let t1: Vec<u32> = scan
        .frames
        .iter()
        .filter(|f| f.head.track == 1)
        .map(|f| f.head.frame_no)
        .collect();
    assert_eq!(t1, vec![0, 1]);
    // track 0 frame 1 is absent from the scan
    assert!(
        scan.frames
            .iter()
            .all(|f| !(f.head.track == 0 && f.head.frame_no == 1))
    );
}

#[test]
fn sequential_indexed_no_trailing() {
    let (a, _) = int_wav(2, 16, 0, 2 * 4096, 3);
    let (b, _) = int_wav(1, 24, 0, 4097, 5);
    let file = enc(&[("a.wav", a), ("b.wav", b)], &opts());
    let r = rd(&file);
    let tracks = r.tracks().to_vec();
    let total: usize = tracks.iter().map(|t| t.nframes as usize).sum();
    let meta_end = HEADER_LEN as u64 + u64::from(u32le(&file, 12));
    let frames_end = u64le(&file, 16); // index_off: the scan must stop there
    let mut cur = Cursor::new(&file[..]);
    let scan = frames_sequential(&mut cur, meta_end, frames_end, 4096, &tracks, |_| true).unwrap();
    assert_eq!(scan.frames.len(), total);
    assert!(scan.frames.iter().all(|f| f.skipped.is_none()));
    assert!(scan.frames.iter().all(|f| f.payload.is_some()));
    assert!(scan.trailing_skip.is_none());
}

#[test]
fn corrupt_all_track0_frames() {
    let (a, _) = int_wav(2, 16, 0, 2 * 4096, 3);
    let (b, xb) = int_wav(1, 16, 0, 2 * 4096, 5);
    let mut file = enc(&[("a.wav", a), ("b.wav", b)], &opts());
    let nframes: Vec<u32> = rd(&file).tracks().iter().map(|t| t.nframes).collect();
    for off in index_offsets(&file, 0, &nframes) {
        file[off as usize + FRAME_HDR_LEN] ^= 0xFF;
    }
    // decode_track(1) via the index never touches track 0's frames
    let mut r = rd(&file);
    assert_eq!(expect_int(r.decode_track(1).unwrap()), xb);
    assert!(r.read_frame(0, 0).is_err());
}

// ---------------------------------------------------------------- no index

#[test]
fn no_index_matches() {
    let (a, xa) = int_wav(2, 16, 0, 2 * 4096 + 77, 3);
    let (b, xb) = int_wav(1, 24, 0, 4097, 5);
    let files = [("a.wav", a), ("b.wav", b)];
    let indexed = enc(&files, &opts());
    let plain = enc(
        &files,
        &V1Opts {
            index: false,
            ..opts()
        },
    );
    assert!(!rd(&plain).has_index());
    let mut r = rd(&plain);
    assert_eq!(expect_int(r.decode_track(0).unwrap()), xa);
    assert_eq!(expect_int(r.decode_track(1).unwrap()), xb);
    // same logical results as the indexed file
    let mut ri = rd(&indexed);
    assert_eq!(
        expect_int(ri.decode_track(0).unwrap()),
        expect_int(rd(&plain).decode_track(0).unwrap())
    );
    assert_eq!(
        expect_int(r.decode_range(1, 100, 3000).unwrap()),
        expect_int(ri.decode_range(1, 100, 3000).unwrap())
    );
}

// ------------------------------------------------------------------- lossy

#[test]
fn lossy_rejected() {
    // MPEG-1 L3 frame header (FF FB = sync + MPEG-1 + Layer III) + payload
    let mut mp3 = vec![0xFFu8, 0xFB, 0x90, 0x00];
    mp3.extend([0u8; 413]);
    let e = enc_err(&[("song.mp3", mp3)], &opts());
    assert!(e.contains("lossless only"), "{e}");
}

// ----------------------------------------------------------------- crafted

fn fix_header_crc(file: &mut [u8]) {
    let c = crc32(&file[..24]);
    file[24..28].copy_from_slice(&c.to_le_bytes());
}

fn fix_meta_crc(file: &mut [u8]) {
    let meta_len = u32le(file, 12) as usize;
    let c = crc32(&file[HEADER_LEN..HEADER_LEN + meta_len - 4]);
    file[HEADER_LEN + meta_len - 4..HEADER_LEN + meta_len].copy_from_slice(&c.to_le_bytes());
}

fn fix_frame_crc(file: &mut [u8], off: usize) {
    let plen = u32le(file, off + 14) as usize;
    let Some(end) = off
        .checked_add(FRAME_HDR_LEN)
        .and_then(|e| e.checked_add(plen))
    else {
        return;
    };
    if end + 4 > file.len() {
        return;
    }
    let c = crc32(&file[off..end]);
    file[end..end + 4].copy_from_slice(&c.to_le_bytes());
}

#[test]
fn crafted_rejections() {
    let (w, _) = int_wav(1, 16, 0, 5000, 1);
    let file = enc(&[("t.wav", w)], &opts());

    // flags = 1
    let mut f = file.clone();
    f[5] = 1;
    fix_header_crc(&mut f);
    assert!(Reader::open(Cursor::new(&f[..])).is_err());

    // header crc mismatch
    let mut f = file.clone();
    f[24] ^= 0xFF;
    assert!(Reader::open(Cursor::new(&f[..])).is_err());

    // meta_len past EOF
    let mut f = file.clone();
    let big = f.len() as u32 + 100;
    f[12..16].copy_from_slice(&big.to_le_bytes());
    fix_header_crc(&mut f);
    assert!(Reader::open(Cursor::new(&f[..])).is_err());

    // frame n > frame_len
    let mut f = file.clone();
    let nframes = [rd(&file).tracks()[0].nframes];
    let off = index_offsets(&file, 0, &nframes)[0] as usize;
    f[off + 10..off + 14].copy_from_slice(&99999u32.to_le_bytes());
    fix_frame_crc(&mut f, off);
    let mut r = rd(&f);
    assert!(r.read_frame(0, 0).is_err());

    // index offset past EOF
    let mut f = file.clone();
    let big = f.len() as u64 + 1000;
    f[16..24].copy_from_slice(&big.to_le_bytes());
    fix_header_crc(&mut f);
    assert!(Reader::open(Cursor::new(&f[..])).is_err());

    // pcm_crc mismatch -> decode_track fails (header/meta crcs stay valid)
    let mut f = file.clone();
    let nl = u16le(&f, HEADER_LEN) as usize;
    let crc_at = HEADER_LEN + 2 + nl + 26;
    f[crc_at] ^= 0xFF;
    fix_meta_crc(&mut f);
    let mut r = rd(&f);
    assert!(r.decode_track(0).is_err());
}

// -------------------------------------------------------------- wav export

#[test]
fn wav_export_reparses() {
    // integer 6ch with mask -> extensible output preserving the mask
    let (w, x) = int_wav(6, 16, 0x3f, 5000, 11);
    let file = enc(&[("s.wav", w)], &opts());
    let mut r = rd(&file);
    let m = r.tracks()[0].clone();
    let s = r.decode_track(0).unwrap();
    let pcm = samples_to_pcm(&s, m.bits);
    let out = formats::make_wav_v1(
        &pcm,
        m.sample_fmt,
        m.bits,
        m.channels,
        m.rate,
        m.channel_mask,
    );
    let p = crate::wav::parse(&out).unwrap();
    assert_eq!(p.channels, 6);
    assert_eq!(crate::wav::channel_ints(&p), x);
    let (f2, _, _) = crate::wav::scan_prefix(&out).unwrap();
    assert_eq!(f2.channel_mask, 0x3f);
    assert_eq!(f2.tag, 1);

    // float output reparses to identical words
    let f32w: Vec<Vec<u64>> = vec![
        (0..3000)
            .map(|i| u64::from(f32::from_bits(0x3f800000 + i as u32 * 7 % 0x100000).to_bits()))
            .collect(),
    ];
    let file = enc(&[("f.wav", float_wav(1, 32, &f32w))], &opts());
    let mut r = rd(&file);
    let m = r.tracks()[0].clone();
    let s = r.decode_track(0).unwrap();
    let pcm = samples_to_pcm(&s, m.bits);
    let out = formats::make_wav_v1(&pcm, m.sample_fmt, m.bits, m.channels, m.rate, 0);
    let p = crate::wav::parse(&out).unwrap();
    assert_eq!(p.tag, 3);
    assert_eq!(crate::wav::channel_words(&p), f32w);
}

// --------------------------------------------------------------- streaming

#[test]
fn streamed_decode_matches_whole() {
    let (a, _) = int_wav(2, 16, 0, 3 * 4096 + 123, 3);
    let f32w: Vec<Vec<u64>> = (0..2)
        .map(|c| {
            (0..5000)
                .map(|i| {
                    u64::from(
                        f32::from_bits(0x3f000000 + (i * 977 + c * 131) as u32 % 0x00400000)
                            .to_bits(),
                    )
                })
                .collect()
        })
        .collect();
    let file = enc(&[("a.wav", a), ("f.wav", float_wav(2, 32, &f32w))], &opts());
    for t in 0..2 {
        let mut r = rd(&file);
        let m = r.tracks()[t].clone();
        let whole_pcm = samples_to_pcm(&r.decode_track(t).unwrap(), m.bits);
        // streamed whole track: concatenated per-frame PCM == whole PCM
        let mut pcm = Vec::new();
        r.decode_range_with(t, 0, m.frames, |s| {
            pcm.extend(samples_to_pcm(&s, m.bits));
            Ok(())
        })
        .unwrap();
        assert_eq!(pcm, whole_pcm, "track {t}: streamed bytes differ");
        // streaming-assembled WAV == make_wav_v1 on the whole PCM
        let mut wavb = formats::wav_header_v1(
            pcm.len(),
            m.sample_fmt,
            m.bits,
            m.channels,
            m.rate,
            m.channel_mask,
        );
        wavb.extend(&pcm);
        if pcm.len() % 2 == 1 {
            wavb.push(0);
        }
        assert_eq!(
            wavb,
            formats::make_wav_v1(
                &pcm,
                m.sample_fmt,
                m.bits,
                m.channels,
                m.rate,
                m.channel_mask
            )
        );
        // streamed range spanning a frame boundary == slice of the full PCM
        let (start, len) = (4000u64, 3000u64); // crosses the 4096 boundary
        let mut rp = Vec::new();
        r.decode_range_with(t, start, len, |s| {
            rp.extend(samples_to_pcm(&s, m.bits));
            Ok(())
        })
        .unwrap();
        let per = usize::from(m.bits / 8) * usize::from(m.channels);
        let end = (start + len).min(m.frames) as usize;
        assert_eq!(
            rp,
            pcm[start as usize * per..end * per],
            "track {t}: streamed range differs"
        );
    }
}

#[test]
fn crafted_out_of_range_bits() {
    // a 24-bit WAV whose samples exceed the 16-bit range, then rewrite the
    // track's meta bits to 16 (meta crc fixed up)
    let (w, _) = int_wav(1, 24, 0, 5000, 13);
    let mut file = enc(&[("t.wav", w)], &opts());
    let nl = u16le(&file, HEADER_LEN) as usize;
    let bits_at = HEADER_LEN + 2 + nl + 11; // name_len+name+origin+rate+ch+mask+fmt
    file[bits_at] = 16;
    fix_meta_crc(&mut file);
    let mut r = rd(&file);
    let e = r.read_frame(0, 0).err().unwrap();
    assert!(e.contains("out of range"), "{e}");
    assert!(e.contains("track 0 frame 0"), "{e}");
}

// ----------------------------------------------------------------- warmup

#[test]
fn warmup_sine_roundtrip() {
    // smooth sines: the planner picks LPC orders > 0 in the frame's first
    // block, so the warm-up path (verbatim first `order` residuals) is real
    let n = 3 * 4096 + 77;
    let x: Vec<Vec<i64>> = vec![
        (0..n)
            .map(|i| (8000.0 * (i as f64 * 0.05).sin() + 3000.0 * (i as f64 * 0.013).cos()) as i64)
            .collect(),
        (0..n)
            .map(|i| (6000.0 * (i as f64 * 0.031).cos()) as i64)
            .collect(),
    ];
    let w = mk_wav(1, 2, 44100, 16, 0, crate::wav::ints_to_pcm(&x, 16, n));
    let file = enc(&[("s.wav", w)], &opts());
    let mut r = rd(&file);
    assert_eq!(expect_int(r.decode_track(0).unwrap()), x);
    // also on the no-index path
    let file = enc(
        &[(
            "s.wav",
            mk_wav(1, 2, 44100, 16, 0, crate::wav::ints_to_pcm(&x, 16, n)),
        )],
        &V1Opts {
            index: false,
            ..opts()
        },
    );
    let mut r = rd(&file);
    assert_eq!(expect_int(r.decode_track(0).unwrap()), x);
}

// ------------------------------------------------------------ mutation smoke

/// Mutations must produce Ok or Err — never a panic.
#[test]
fn v1_mutation_smoke() {
    let (a, _) = int_wav(2, 16, 0, 3 * 4096 + 123, 3);
    let (b, _) = int_wav(1, 24, 0, 4097, 5);
    let f32w: Vec<Vec<u64>> = vec![
        (0..3000)
            .map(|i| u64::from(f32::from_bits(0x3f800000 + i as u32 * 7 % 0x100000).to_bits()))
            .collect(),
    ];
    let vectors: Vec<Vec<u8>> = vec![
        enc(&[("a.wav", a)], &opts()),
        enc(
            &[("a.wav", b.clone()), ("f.wav", float_wav(1, 32, &f32w))],
            &opts(),
        ),
        enc(
            &[("a.wav", b)],
            &V1Opts {
                index: false,
                ..opts()
            },
        ),
    ];
    let mut st = 0x9E3779B97F4A7C15u64;
    let mut rng = move || {
        st ^= st << 13;
        st ^= st >> 7;
        st ^= st << 17;
        st
    };
    // fields an attacker would target (relative offsets per variant)
    let mut mutations = 0u32;
    for orig in &vectors {
        // random corruptions: bit flips, byte overwrites, truncations
        for _ in 0..1000 {
            let mut f = orig.clone();
            match rng() % 10 {
                0 | 1 => {
                    // bit flip
                    let i = (rng() as usize) % f.len();
                    f[i] ^= 1 << (rng() % 8);
                }
                2 | 3 => {
                    // byte overwrite, biased to headers/meta/index
                    let i = (rng() as usize) % f.len().min(512);
                    f[i] = rng() as u8;
                }
                4 => {
                    // random byte overwrite anywhere
                    let i = (rng() as usize) % f.len();
                    f[i] = rng() as u8;
                }
                5 | 6 => {
                    // truncate
                    let nl = 28 + (rng() as usize) % (f.len() - 28);
                    f.truncate(nl);
                }
                7 => {
                    // u32/u64 field smash in header+meta region
                    let i = (rng() as usize) % (f.len().min(300) - 4);
                    let v = match rng() % 4 {
                        0 => u32::MAX,
                        1 => u32::MAX - 1,
                        2 => 1u32 << 20,
                        _ => 0,
                    };
                    f[i..i + 4].copy_from_slice(&v.to_le_bytes());
                }
                _ => {
                    // n/payload_len fields of a frame region byte
                    let i = (rng() as usize) % f.len();
                    f[i] = rng() as u8;
                    let j = (rng() as usize) % f.len();
                    f[j] ^= 0x80;
                }
            }
            exercise(&f);
            mutations += 1;
        }
        // targeted length/count edits with fixed-up CRCs where reachable
        for &off in &[12usize, 16] {
            // meta_len, index_off
            for &v in &[
                0u64,
                1,
                64,
                u64::from(u32::MAX),
                orig.len() as u64,
                u64::MAX / 2,
            ] {
                let mut f = orig.clone();
                let nb = if off == 12 { 4 } else { 8 };
                f[off..off + nb].copy_from_slice(&v.to_le_bytes()[..nb]);
                fix_header_crc(&mut f);
                exercise(&f);
                mutations += 1;
            }
        }
        // per-track meta count fields (frames/nframes/pcm_crc/tags_len)
        let base = HEADER_LEN + 2 + u16le(orig, HEADER_LEN) as usize;
        for (rel, nb) in [(12usize, 8usize), (20, 4), (28, 4)] {
            for &v in &[0u64, 1, 4096, 1 << 20, u64::from(u32::MAX)] {
                let mut f = orig.clone();
                f[base + rel..base + rel + nb].copy_from_slice(&v.to_le_bytes()[..nb]);
                fix_meta_crc(&mut f);
                exercise(&f);
                mutations += 1;
            }
        }
        // frame n / payload_len / wbits / partition-order smashes on the
        // first frame (offset via index when present)
        let idx = u64le(orig, 16) as usize;
        if idx > 0 {
            let fo = u64le(orig, idx) as usize;
            for &(rel, v) in &[
                (10usize, 0u32), // n = 0
                (10, 999999),    // n huge
                (14, 0),         // payload_len = 0
                (14, u32::MAX),  // payload_len huge
                (14, orig.len() as u32),
            ] {
                let mut f = orig.clone();
                f[fo + rel..fo + rel + 4].copy_from_slice(&v.to_le_bytes());
                fix_frame_crc(&mut f, fo);
                exercise(&f);
                mutations += 1;
            }
        }
    }
    assert!(mutations > 2500, "too few mutations: {mutations}");

    fn exercise(f: &[u8]) {
        // must not panic; Err is fine
        let Ok(mut r) = Reader::open(Cursor::new(f)) else {
            return;
        };
        for t in 0..r.tracks().len().min(4) {
            let _ = r.decode_track(t);
            let _ = r.read_frame(t, 0);
            let _ = r.decode_range(t, 0, 8);
        }
        let _ = r.coded_bytes(0);
    }
}

#[test]
fn warmup_i24_roundtrip() {
    // high-amplitude narrowband mono i24 at frame_len 4096: block 0 picks a
    // real LPC order and carries explicit warm-up fields
    let n = 3 * 4096 + 123;
    let x: Vec<Vec<i64>> = vec![
        (0..n)
            .map(|i| {
                let f = i as f64;
                (2_000_000.0 * (0.020 * f).sin()
                    + 1_500_000.0 * (0.031 * f).cos()
                    + 1_000_000.0 * (0.043 * f).sin()
                    + 800_000.0 * (0.057 * f).cos()
                    + 500_000.0 * (0.071 * f).sin()
                    + 300_000.0 * (0.089 * f).cos()
                    + ((i * 37) % 97) as f64 * 40.0) as i64
            })
            .collect(),
    ];
    let w = mk_wav(1, 1, 48000, 24, 0, crate::wav::ints_to_pcm(&x, 24, n));
    let file = enc(&[("s.wav", w)], &opts());
    let mut r = rd(&file);
    assert_eq!(expect_int(r.decode_track(0).unwrap()), x);
}

// ------------------------------------------------------- conformance vectors

struct VSpec {
    name: &'static str,
    files: Vec<(&'static str, Vec<u8>)>,
    opts: V1Opts,
}

/// Smooth deterministic tone: compressible, exercises LPC + warm-up.
fn tone(ch: usize, bits: u16, frames: usize, seed: u64) -> Vec<Vec<i64>> {
    let half = 1i64 << (bits - 1);
    let amp = half as f64 * 0.6;
    (0..ch)
        .map(|c| {
            (0..frames)
                .map(|i| {
                    let f = i as f64;
                    let v = amp * (0.013 * f + c as f64 * 0.7).sin()
                        + amp * 0.4 * (0.031 * f + seed as f64).cos()
                        + ((i * 37 + c * 11) % 97) as f64 * half as f64 * 1e-4;
                    v.round().clamp(-(half as f64), half as f64 - 1.0) as i64
                })
                .collect()
        })
        .collect()
}

fn int_vwav(ch: u16, bits: u16, mask: u32, frames: usize, seed: u64) -> Vec<u8> {
    let x = tone(ch as usize, bits, frames, seed);
    mk_wav(
        1,
        ch,
        44100,
        bits,
        mask,
        crate::wav::ints_to_pcm(&x, bits, frames),
    )
}

fn ftone(ch: usize, frames: usize, f32_exact: bool) -> Vec<Vec<u64>> {
    (0..ch)
        .map(|c| {
            (0..frames)
                .map(|i| {
                    let f = i as f64;
                    let v = 0.5 * (0.013 * f + c as f64).sin()
                        + 0.2 * (0.031 * f).cos()
                        + ((i * 37 + c * 11) % 97) as f64 * 1e-5;
                    if f32_exact {
                        f64::from(v as f32).to_bits()
                    } else {
                        (v + i as f64 * 1e-12).to_bits()
                    }
                })
                .collect()
        })
        .collect()
}

fn ftone32(ch: usize, frames: usize) -> Vec<Vec<u64>> {
    (0..ch)
        .map(|c| {
            (0..frames)
                .map(|i| {
                    let f = i as f32;
                    (0.5f32 * (0.013 * f + c as f32).sin()
                        + 0.2 * (0.031 * f).cos()
                        + ((i * 37 + c * 11) % 97) as f32 * 1e-5)
                        .to_bits() as u64
                })
                .collect()
        })
        .collect()
}

/// FLAC-style metadata block chain: one VORBIS_COMMENT block.
fn tag_blob() -> Vec<u8> {
    let mut body = Vec::new();
    let vendor = b"flq-conformance";
    body.extend((vendor.len() as u32).to_le_bytes());
    body.extend(vendor);
    let comments: [&[u8]; 2] = [b"TITLE=conformance", b"ARTIST=flq"];
    body.extend((comments.len() as u32).to_le_bytes());
    for c in comments {
        body.extend((c.len() as u32).to_le_bytes());
        body.extend(c);
    }
    let mut blob = vec![0x04u8];
    blob.extend([
        (body.len() >> 16) as u8,
        (body.len() >> 8) as u8,
        body.len() as u8,
    ]);
    blob.extend(body);
    blob
}

fn vector_specs() -> Vec<VSpec> {
    let o = |frame_len: u32, index: bool, preset: u8| V1Opts {
        preset,
        threads: 1,
        frame_len,
        index,
    };
    let d = 16384u32; // CLI default frame_len
    let i16x = tone(2, 16, 10000, 3);
    let i16w = mk_wav(
        1,
        2,
        44100,
        16,
        0,
        crate::wav::ints_to_pcm(&i16x, 16, 10000),
    );
    let flac_pcm = tone(2, 16, 4000, 9);
    let flac = formats::encode_flac(&flac_pcm, 16, 2, 44100, &tag_blob()).unwrap();
    vec![
        VSpec {
            name: "v1_i8_mono.flq",
            files: vec![("i8.wav", int_vwav(1, 8, 0, 8000, 1))],
            opts: o(d, true, 5),
        },
        VSpec {
            name: "v1_i16_stereo.flq",
            files: vec![("i16.wav", i16w.clone())],
            opts: o(d, true, 5),
        },
        VSpec {
            name: "v1_i16_stereo_p0.flq",
            files: vec![("i16.wav", i16w.clone())],
            opts: o(d, true, 0),
        },
        VSpec {
            name: "v1_i16_stereo_p8.flq",
            files: vec![("i16.wav", i16w)],
            opts: o(d, true, 8),
        },
        VSpec {
            name: "v1_i24_6ch.flq",
            files: vec![("i24.wav", int_vwav(6, 24, 0x3f, 4500, 2))],
            opts: o(d, true, 5),
        },
        VSpec {
            name: "v1_i32_stereo.flq",
            files: vec![("i32.wav", int_vwav(2, 32, 0, 5000, 4))],
            opts: o(d, true, 5),
        },
        VSpec {
            name: "v1_f32_stereo.flq",
            files: vec![("f32.wav", float_wav(2, 32, &ftone32(2, 5000)))],
            opts: o(d, true, 5),
        },
        VSpec {
            name: "v1_f64_asf32.flq",
            files: vec![("f64.wav", float_wav(2, 64, &ftone(2, 4000, true)))],
            opts: o(d, true, 5),
        },
        VSpec {
            name: "v1_f64_true.flq",
            files: vec![("f64.wav", float_wav(2, 64, &ftone(2, 4000, false)))],
            opts: o(d, true, 5),
        },
        VSpec {
            name: "v1_one_sample.flq",
            files: vec![("one.wav", int_vwav(1, 16, 0, 1, 5))],
            opts: o(d, true, 5),
        },
        VSpec {
            name: "v1_partial_frame.flq",
            files: vec![("p.wav", int_vwav(2, 16, 0, 16384 + 123, 6))],
            opts: o(d, true, 5),
        },
        VSpec {
            name: "v1_two_track_tags.flq",
            files: vec![("w.wav", int_vwav(2, 16, 0, 4000, 7)), ("t.flac", flac)],
            opts: o(d, true, 5),
        },
        VSpec {
            name: "v1_frame4096.flq",
            files: vec![("f.wav", int_vwav(2, 16, 0, 9000, 8))],
            opts: o(4096, true, 5),
        },
        VSpec {
            name: "v1_no_index.flq",
            files: vec![("n.wav", int_vwav(2, 16, 0, 9000, 8))],
            opts: o(d, false, 5),
        },
    ]
}

/// Writes testdata/v1/*.flq + MANIFEST.txt. Run:
/// `cargo test --release -- --ignored gen_v1_vectors`
#[test]
#[ignore]
fn gen_v1_vectors() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/v1");
    std::fs::create_dir_all(&dir).unwrap();
    let mut man = String::from(
        "# flq v1 conformance vectors\n\
         # name preset frame_len index ntracks tracks(frames:nframes:pcm_crc,...) file_crc32\n",
    );
    for v in vector_specs() {
        let bytes = enc(
            &v.files
                .iter()
                .map(|(n, b)| (*n, b.clone()))
                .collect::<Vec<_>>(),
            &v.opts,
        );
        std::fs::write(dir.join(v.name), &bytes).unwrap();
        let r = rd(&bytes);
        let tracks = r
            .tracks()
            .iter()
            .map(|m| format!("{}:{}:{:08x}", m.frames, m.nframes, m.pcm_crc))
            .collect::<Vec<_>>()
            .join(",");
        man += &format!(
            "{} preset={} frame_len={} index={} ntracks={} tracks={} crc32={:08x}\n",
            v.name,
            v.opts.preset,
            v.opts.frame_len,
            v.opts.index as u8,
            v.files.len(),
            tracks,
            crc32(&bytes)
        );
    }
    std::fs::write(dir.join("MANIFEST.txt"), man).unwrap();
}

/// Every committed vector decodes correctly AND re-encoding the same
/// synthetic input reproduces it byte-for-byte (encoder output frozen).
#[test]
fn v1_vectors_conform() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/v1");
    let man = std::fs::read_to_string(dir.join("MANIFEST.txt")).unwrap();
    let mut n = 0;
    for line in man.lines().filter(|l| !l.starts_with('#') && !l.is_empty()) {
        let name = line.split_whitespace().next().unwrap();
        let committed = std::fs::read(dir.join(name)).unwrap();
        let spec = vector_specs()
            .into_iter()
            .find(|v| v.name == name)
            .unwrap_or_else(|| panic!("manifest names unknown vector {name}"));
        let rebuilt = enc(
            &spec
                .files
                .iter()
                .map(|(n, b)| (*n, b.clone()))
                .collect::<Vec<_>>(),
            &spec.opts,
        );
        assert_eq!(rebuilt, committed, "{name}: encoder output drifted");
        // decode: pcm_crc + frame counts verified
        let expect: Vec<(u64, u32)> = line
            .split_whitespace()
            .find_map(|t| t.strip_prefix("tracks="))
            .unwrap()
            .split(',')
            .map(|t| {
                let mut p = t.split(':');
                (
                    p.next().unwrap().parse().unwrap(),
                    p.next().unwrap().parse().unwrap(),
                )
            })
            .collect();
        let mut r = rd(&committed);
        assert_eq!(r.tracks().len(), expect.len(), "{name}");
        for (t, &(frames, nframes)) in expect.iter().enumerate() {
            assert_eq!(r.tracks()[t].frames, frames, "{name} t{t}");
            assert_eq!(r.tracks()[t].nframes, nframes, "{name} t{t}");
            r.decode_track(t)
                .unwrap_or_else(|e| panic!("{name} t{t}: {e}"));
        }
        n += 1;
    }
    assert_eq!(n, vector_specs().len(), "manifest/spec count mismatch");
}

/// Deterministic demos for the web player: "drums.wav" (kick/hat
/// pattern, mono) + "pad.wav" (Am chord, stereo), ~14 s at 44.1 kHz —
/// and "stems.flq", an 8-track session bundle (~16 s) with one f32
/// track, showing off multi-track access and tonal compression.
/// `cargo test --release -- --ignored gen_web_demo`
#[test]
#[ignore]
fn gen_web_demo() {
    let rate = 44100usize;
    let secs = 14;
    let n = rate * secs;
    // drums: kick = decaying 55 Hz sine on beats (120 bpm), hat = short
    // deterministic noise burst on eighth off-beats
    let mut rng = 0x2545F4914F6CDD1Du64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        ((rng >> 33) as i32) as f64 / i32::MAX as f64
    };
    let beat = rate / 2; // 0.5 s
    let drums: Vec<i64> = (0..n)
        .map(|i| {
            let tb = i % beat;
            let kick = (2.0 * std::f64::consts::PI * 55.0 * tb as f64 / rate as f64).sin()
                * (-(tb as f64) / (rate as f64 * 0.18)).exp();
            let th = i % (beat / 2);
            let hat = if th < 400 && i % beat >= beat / 2 {
                next() * (-(th as f64) / 60.0).exp() * 0.4
            } else {
                0.0
            };
            ((kick * 0.8 + hat) * 28000.0) as i64
        })
        .collect();
    // pad: A minor chord, slow tremolo
    let f = [220.0f64, 261.63, 329.63];
    let pad: Vec<Vec<i64>> = (0..2)
        .map(|c| {
            (0..n)
                .map(|i| {
                    let t = i as f64 / rate as f64;
                    let trem = 0.75 + 0.25 * (2.0 * std::f64::consts::PI * 0.5 * t).sin();
                    let v: f64 = f
                        .iter()
                        .enumerate()
                        .map(|(k, &fq)| {
                            (2.0 * std::f64::consts::PI * (fq + c as f64 * 0.3) * t + k as f64)
                                .sin()
                        })
                        .sum::<f64>()
                        / f.len() as f64;
                    (v * trem * 20000.0) as i64
                })
                .collect()
        })
        .collect();
    let files = [
        (
            "drums.wav",
            mk_wav(
                1,
                1,
                rate as u32,
                16,
                0,
                crate::wav::ints_to_pcm(&[drums], 16, n),
            ),
        ),
        (
            "pad.wav",
            mk_wav(
                1,
                2,
                rate as u32,
                16,
                0,
                crate::wav::ints_to_pcm(&pad, 16, n),
            ),
        ),
    ];
    let bytes = enc(
        &files,
        &V1Opts {
            preset: 5,
            threads: 1,
            frame_len: 16384,
            index: true,
        },
    );
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web/samples");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("demo.flq"), &bytes).unwrap();

    // ---------------- stems.flq: 8-track session bundle ----------------
    // Am — F — C — G, 4 s per chord, 120 bpm (beat = 0.5 s). Mostly
    // sustained tones: LTP + LPC shine, so the archive lands at a few %
    // of the ~20 MB of raw PCM. Track 8 is f32 — float coverage demo.
    let pi = std::f64::consts::PI;
    let mut rng2 = 0x9E3779B97F4A7C15u64;
    let mut next2 = || {
        rng2 ^= rng2 << 13;
        rng2 ^= rng2 >> 7;
        rng2 ^= rng2 << 17;
        ((rng2 >> 33) as i32) as f64 / i32::MAX as f64
    };
    let n2 = rate * 16;
    let chord = |i: usize| [55.0, 43.65, 65.41, 49.0][i / (rate * 4) % 4];
    let triad = |i: usize| {
        [
            [220.0, 261.63, 329.63],
            [174.61, 220.0, 261.63],
            [130.81, 164.81, 196.0],
            [196.0, 246.94, 293.66],
        ][i / (rate * 4) % 4]
    };

    let kick: Vec<i64> = (0..n2)
        .map(|i| {
            let tb = (i % beat) as f64 / rate as f64; // seconds into the beat
            let ph = 2.0 * pi * (48.0 * tb + 3.0 * (1.0 - (-30.0 * tb).exp()));
            (ph.sin() * (-(tb / 0.20)).exp() * 30000.0) as i64
        })
        .collect();
    let snare: Vec<i64> = (0..n2)
        .map(|i| {
            let sb = (i % beat) as f64 / rate as f64;
            if (i / beat) % 2 == 1 && sb < 0.25 {
                let noise = next2() * (-(sb / 0.09)).exp() * 0.5;
                let tone = (2.0 * pi * 185.0 * sb).sin() * (-(sb / 0.12)).exp();
                ((noise + tone) * 12000.0) as i64
            } else {
                0
            }
        })
        .collect();
    let hats: Vec<i64> = (0..n2)
        .map(|i| {
            let hb = (i % (beat / 2)) as f64 / rate as f64;
            let dec = if (i / (beat / 2)) % 8 == 7 {
                0.22
            } else {
                0.03
            };
            if hb < dec * 3.0 {
                (next2() * (-(hb / dec)).exp() * 5500.0) as i64
            } else {
                0
            }
        })
        .collect();
    let bass: Vec<i64> = (0..n2)
        .map(|i| {
            let tbar = (i % (rate * 4)) as f64 / rate as f64;
            let atk = (tbar / 0.02).min(1.0);
            let r = chord(i);
            let v =
                (2.0 * pi * r * tbar).sin() * atk + 0.35 * (2.0 * pi * r * 2.0 * tbar).sin() * atk;
            (v * 0.55 * 26000.0) as i64
        })
        .collect();
    let chords: Vec<Vec<i64>> = (0..2)
        .map(|c| {
            (0..n2)
                .map(|i| {
                    let t = i as f64 / rate as f64;
                    let tbar = (i % (rate * 4)) as f64 / rate as f64;
                    let atk = (tbar / 0.30).min(1.0);
                    let trem = 0.8 + 0.2 * (2.0 * pi * 0.5 * t).sin();
                    let v: f64 = triad(i)
                        .iter()
                        .enumerate()
                        .map(|(k, &fq)| (2.0 * pi * (fq + c as f64 * 0.25) * t + k as f64).sin())
                        .sum::<f64>()
                        / 3.0;
                    (v * trem * atk * 16000.0) as i64
                })
                .collect()
        })
        .collect();
    const MEL: [f64; 16] = [
        440.0, 523.25, 587.33, 659.26, 783.99, 659.26, 587.33, 523.25, 440.0, 587.33, 659.26,
        783.99, 880.0, 783.99, 659.26, 587.33,
    ];
    let lead: Vec<Vec<i64>> = (0..2)
        .map(|c| {
            (0..n2)
                .map(|i| {
                    let nb = (i % (beat / 2)) as f64 / rate as f64; // 8th notes
                    let vib = 1.0 + 0.004 * (2.0 * pi * 5.5 * i as f64 / rate as f64).sin();
                    let env = (nb / 0.01).min(1.0) * ((0.20 - nb) / 0.05).clamp(0.0, 1.0);
                    let v = (2.0 * pi * MEL[(i / (beat / 2)) % 16] * vib * nb).sin() * env;
                    (v * (1.0 - c as f64 * 0.15) * 20000.0) as i64
                })
                .collect()
        })
        .collect();
    let arp: Vec<Vec<i64>> = (0..2)
        .map(|c| {
            (0..n2)
                .map(|i| {
                    let st = i / (beat / 4); // 16th notes
                    let ab = (i % (beat / 4)) as f64 / rate as f64;
                    let f = triad(i)[st % 3] * if st % 2 == 1 { 2.0 } else { 1.0 };
                    let env = (ab / 0.005).min(1.0) * (-(ab / 0.10)).exp();
                    let v =
                        ((2.0 * pi * f * ab).sin() + 0.3 * (2.0 * pi * 3.0 * f * ab).sin()) * env;
                    let pan = if st % 2 == (c & 1) { 1.0 } else { 0.7 };
                    (v * pan * 12000.0) as i64
                })
                .collect()
        })
        .collect();
    let keys: Vec<Vec<u64>> = (0..2)
        .map(|c| {
            (0..n2)
                .map(|i| {
                    let t = i as f64 / rate as f64;
                    let tbar = (i % (rate * 4)) as f64 / rate as f64;
                    let atk = (tbar / 0.04).min(1.0);
                    let trem = 0.85 + 0.15 * (2.0 * pi * 4.2 * t + c as f64).sin();
                    let v: f64 = triad(i)
                        .iter()
                        .enumerate()
                        .map(|(k, &fq)| {
                            (2.0 * pi * (fq * 2.0 + c as f64 * 0.4) * t + k as f64).sin()
                                + 0.2 * (2.0 * pi * (fq * 4.0) * t).sin()
                        })
                        .sum::<f64>()
                        / 3.0;
                    ((v * trem * atk * 0.32) as f32).to_bits() as u64
                })
                .collect()
        })
        .collect();

    let stems = [
        (
            "01_kick.wav",
            mk_wav(
                1,
                1,
                rate as u32,
                16,
                0,
                crate::wav::ints_to_pcm(&[kick], 16, n2),
            ),
        ),
        (
            "02_snare.wav",
            mk_wav(
                1,
                1,
                rate as u32,
                16,
                0,
                crate::wav::ints_to_pcm(&[snare], 16, n2),
            ),
        ),
        (
            "03_hats.wav",
            mk_wav(
                1,
                1,
                rate as u32,
                16,
                0,
                crate::wav::ints_to_pcm(&[hats], 16, n2),
            ),
        ),
        (
            "04_bass.wav",
            mk_wav(
                1,
                1,
                rate as u32,
                16,
                0,
                crate::wav::ints_to_pcm(&[bass], 16, n2),
            ),
        ),
        (
            "05_chords.wav",
            mk_wav(
                1,
                2,
                rate as u32,
                16,
                0,
                crate::wav::ints_to_pcm(&chords, 16, n2),
            ),
        ),
        (
            "06_lead.wav",
            mk_wav(
                1,
                2,
                rate as u32,
                16,
                0,
                crate::wav::ints_to_pcm(&lead, 16, n2),
            ),
        ),
        (
            "07_arp.wav",
            mk_wav(
                1,
                2,
                rate as u32,
                16,
                0,
                crate::wav::ints_to_pcm(&arp, 16, n2),
            ),
        ),
        (
            "08_keys_f32.wav",
            mk_wav(
                3,
                2,
                rate as u32,
                32,
                0,
                crate::wav::words_to_pcm(&keys, 32, n2),
            ),
        ),
    ];
    let bytes = enc(
        &stems,
        &V1Opts {
            preset: 5,
            threads: 1,
            frame_len: 16384,
            index: true,
        },
    );
    std::fs::write(dir.join("stems.flq"), &bytes).unwrap();
    eprintln!("stems.flq: {} B", bytes.len());
}
