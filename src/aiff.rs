//! AIFF/AIFF-C support: parse to WAV-convention `Wav` and convert back.
//!
//! AIFF native sample layout is big-endian (8-bit signed); WAV convention used
//! by the shared codec is little-endian (8-bit unsigned). `parse` converts the
//! SSND payload to WAV convention so `channel_ints`/`channel_words`/codec work
//! unchanged; `to_native` converts back before prefix + PCM + suffix rebuild.

use crate::wav::{Fmt, MAX_CHANNELS, Wav};

/// How native sample bytes map to WAV-convention bytes (self-inverse).
#[derive(Clone, Copy)]
struct Conv {
    be: bool,  // native samples are big-endian (reverse each container sample)
    int: bool, // integer samples; 8-bit ints get the signed<->unsigned bias
}

struct Comm {
    fmt: Fmt,
    conv: Conv,
    frames: u64, // COMM numSampleFrames
}

fn u16be(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn u32be(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// 80-bit IEEE 754 extended -> u32 (integer math, exact for integer rates).
fn ext80_to_u32(b: &[u8]) -> Result<u32, String> {
    if b.len() < 10 {
        return Err("truncated extended rate".into());
    }
    let exp = u16be(b, 0) & 0x7fff;
    let mant = u64::from_be_bytes(b[2..10].try_into().unwrap());
    if exp == 0 || mant == 0 {
        return Err("zero or denormal sample rate".into());
    }
    // value = mant * 2^(exp - 16383 - 63)
    let e = i32::from(exp) - 16383 - 63;
    let v: u128 = if e >= 0 {
        if e > 63 {
            return Err("sample rate out of range".into());
        }
        (mant as u128) << e
    } else {
        let s = (-e) as u32;
        if s >= 64 {
            return Err("sample rate out of range".into());
        }
        let mut v = mant >> s;
        if s > 0 && (mant >> (s - 1)) & 1 == 1 {
            v += 1; // round half up on the dropped top bit
        }
        u128::from(v)
    };
    if v == 0 || v > u128::from(u32::MAX) {
        return Err("sample rate out of range".into());
    }
    Ok(v as u32)
}

fn read_comm(payload: &[u8], is_aifc: bool) -> Result<Comm, String> {
    if payload.len() < 18 {
        return Err("malformed COMM chunk".into());
    }
    let channels = u16be(payload, 0);
    let frames = u64::from(u32be(payload, 2));
    let size = u16be(payload, 6);
    let rate = ext80_to_u32(&payload[8..18])?;
    if channels == 0 || channels > MAX_CHANNELS {
        return Err(format!("unsupported channel count {channels}"));
    }
    let cb = size.div_ceil(8); // sampleSize -> whole container bytes
    let (tag, cb, conv) = if is_aifc {
        if payload.len() < 22 {
            return Err("malformed AIFC COMM chunk".into());
        }
        match &payload[18..22] {
            b"NONE" | b"twos" => (
                1u16,
                cb,
                Conv {
                    be: true,
                    int: true,
                },
            ),
            b"sowt" => (
                1,
                cb,
                Conv {
                    be: false,
                    int: true,
                },
            ),
            b"fl32" | b"FL32" => (
                3,
                4,
                Conv {
                    be: true,
                    int: false,
                },
            ),
            b"fl64" | b"FL64" => (
                3,
                8,
                Conv {
                    be: true,
                    int: false,
                },
            ),
            c => {
                return Err(format!(
                    "unsupported AIFC compression {:?}",
                    String::from_utf8_lossy(c)
                ));
            }
        }
    } else {
        (
            1,
            cb,
            Conv {
                be: true,
                int: true,
            },
        )
    };
    if conv.int {
        // AIFF stores samples left-justified; keep the raw container value.
        if !(1..=4).contains(&cb) || size == 0 {
            return Err(format!("unsupported sample size {size}"));
        }
    } else if size != 8 * cb {
        return Err(format!("float sample size {size} != {}", 8 * cb));
    }
    let bits = 8 * cb;
    Ok(Comm {
        fmt: Fmt {
            tag,
            channels,
            rate,
            bits,
            block_align: channels * cb,
            channel_mask: 0,
        },
        conv,
        frames,
    })
}

/// Walk FORM chunks to COMM and SSND. Returns the COMM, the offset of the
/// first sound byte (after SSND's offset+blockSize fields and `offset` skip
/// bytes), and the SSND-declared sound byte count after it (NOT capped by
/// the buffer length — callers clamp to what the file actually holds).
fn scan(bytes: &[u8]) -> Result<(Comm, usize, usize), String> {
    if bytes.len() < 12 || &bytes[0..4] != b"FORM" {
        return Err("not a FORM file".into());
    }
    let is_aifc = match &bytes[8..12] {
        b"AIFF" => false,
        b"AIFC" => true,
        _ => return Err("not AIFF/AIFC".into()),
    };
    let mut pos = 12usize;
    let mut comm: Option<Comm> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32be(bytes, pos + 4) as usize;
        let dstart = pos + 8;
        if id == b"COMM" {
            let end = dstart.checked_add(size).ok_or("COMM size overflow")?;
            let body = bytes.get(dstart..end).ok_or("truncated COMM chunk")?;
            comm = Some(read_comm(body, is_aifc)?);
        } else if id == b"SSND" {
            let c = comm.ok_or("SSND chunk precedes COMM")?;
            if size < 8 || dstart + 8 > bytes.len() {
                return Err("malformed SSND chunk".into());
            }
            let offset = u32be(bytes, dstart) as usize;
            let snd = dstart
                .checked_add(8)
                .and_then(|s| s.checked_add(offset))
                .ok_or("SSND offset overflow")?;
            if snd > bytes.len() {
                return Err("SSND offset beyond end of file".into());
            }
            let declared = size.saturating_sub(8).saturating_sub(offset);
            return Ok((c, snd, declared));
        }
        // chunks are padded to even sizes
        let next = dstart.saturating_add(size).saturating_add(size & 1);
        if next <= pos {
            break;
        }
        pos = next;
    }
    Err("no SSND chunk".into())
}

/// Find COMM inside a stored prefix (ends at the first SSND sound byte).
fn comm_in_prefix(prefix: &[u8]) -> Result<Comm, String> {
    if prefix.len() < 12 || &prefix[0..4] != b"FORM" {
        return Err("not a FORM prefix".into());
    }
    let is_aifc = match &prefix[8..12] {
        b"AIFF" => false,
        b"AIFC" => true,
        _ => return Err("not AIFF/AIFC".into()),
    };
    let mut pos = 12usize;
    let mut comm: Option<Comm> = None;
    while pos + 8 <= prefix.len() {
        let id = &prefix[pos..pos + 4];
        let size = u32be(prefix, pos + 4) as usize;
        let dstart = pos + 8;
        if id == b"COMM" {
            let end = dstart.checked_add(size).ok_or("COMM size overflow")?;
            let body = prefix.get(dstart..end).ok_or("truncated COMM chunk")?;
            comm = Some(read_comm(body, is_aifc)?);
        } else if id == b"SSND" {
            // prefix ends at the first sound byte: SSND header + offset skips
            if size < 8 || dstart + 8 > prefix.len() {
                return Err("malformed SSND chunk".into());
            }
            let offset = u32be(prefix, dstart) as usize;
            let end = dstart + 8 + offset;
            if end != prefix.len() {
                return Err("prefix does not end at the first sound byte".into());
            }
            return comm.ok_or("SSND chunk precedes COMM".into());
        }
        let next = dstart.saturating_add(size).saturating_add(size & 1);
        if next <= pos {
            break;
        }
        pos = next;
    }
    Err("no SSND chunk in prefix".into())
}

/// Convert between native AIFF sample bytes and WAV-convention bytes.
/// Self-inverse: byte-reversal and the 8-bit bias undo themselves.
fn conv_pcm(b: &[u8], conv: Conv, cb: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    for s in b.chunks_exact(cb) {
        if conv.be && cb > 1 {
            out.extend(s.iter().rev());
        } else {
            out.extend(s);
        }
        if conv.int && cb == 1 {
            let n = out.len();
            out[n - 1] ^= 0x80;
        }
    }
    out
}

/// Parse an AIFF/AIFC file into a `Wav` in WAV convention (LE, 8-bit unsigned).
pub fn parse(bytes: &[u8]) -> Result<Wav, String> {
    let (c, snd, declared) = scan(bytes)?;
    let avail = declared.min(bytes.len() - snd);
    let align = c.fmt.block_align as usize;
    let frames = c.frames.min((avail / align) as u64) as usize;
    let pcm_len = frames * align;
    let cb = (c.fmt.bits / 8) as usize;
    let pcm = conv_pcm(&bytes[snd..snd + pcm_len], c.conv, cb);
    Ok(Wav {
        tag: c.fmt.tag,
        channels: c.fmt.channels,
        rate: c.fmt.rate,
        bits: c.fmt.bits,
        frames: frames as u64,
        prefix: bytes[..snd].to_vec(),
        pcm,
        suffix: bytes[snd + pcm_len..].to_vec(),
    })
}

/// Header result for streaming encoders: COMM fields + sound position.
/// `bytes` only has to reach the first sound byte; the payload need not be
/// in the buffer.
pub struct AiffHead {
    pub fmt: Fmt,
    pub frames: u64,      // COMM numSampleFrames
    pub pcm_start: usize, // file offset of the first sound byte
    pub avail: usize,     // SSND-declared sound bytes after pcm_start
    conv: Conv,
}

impl AiffHead {
    /// Native sound bytes -> WAV-convention bytes (per frame-aligned chunk).
    pub fn to_wav_conv(&self, b: &[u8]) -> Vec<u8> {
        conv_pcm(b, self.conv, (self.fmt.bits / 8) as usize)
    }
}

/// Header-only scan for streaming encoders.
pub fn scan_prefix(bytes: &[u8]) -> Result<AiffHead, String> {
    let (c, snd, declared) = scan(bytes)?;
    Ok(AiffHead {
        fmt: c.fmt,
        frames: c.frames,
        pcm_start: snd,
        avail: declared,
        conv: c.conv,
    })
}

/// Recover the fmt from a stored prefix (ends at the first sound byte).
pub fn parse_prefix(prefix: &[u8]) -> Result<Fmt, String> {
    Ok(comm_in_prefix(prefix)?.fmt)
}

/// Convert WAV-convention PCM bytes back to the native AIFF layout described
/// by the file's COMM chunk (for prefix + PCM + suffix rebuild on decode).
pub fn to_native(prefix: &[u8], wav_pcm: &[u8]) -> Result<Vec<u8>, String> {
    let c = comm_in_prefix(prefix)?;
    Ok(conv_pcm(wav_pcm, c.conv, (c.fmt.bits / 8) as usize))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::wav;

    /// AIFF/AIFC synthetic builder used by tests here and in formats.rs.
    pub struct AiffBuilder {
        pub aifc: bool,
        pub compression: [u8; 4],
        pub channels: u16,
        pub rate: u32,
        pub bits: u16,                 // COMM sampleSize
        pub frames: u32,               // COMM numSampleFrames
        pub pre_chunks: Vec<Vec<u8>>,  // extra chunks before COMM
        pub mid_chunks: Vec<Vec<u8>>,  // chunks between COMM and SSND
        pub post_chunks: Vec<Vec<u8>>, // chunks after SSND
        pub ssnd_offset: u32,
        pub sound: Vec<u8>, // native sound bytes
    }

    impl AiffBuilder {
        pub fn new(channels: u16, rate: u32, bits: u16, frames: u32) -> Self {
            AiffBuilder {
                aifc: false,
                compression: *b"NONE",
                channels,
                rate,
                bits,
                frames,
                pre_chunks: Vec::new(),
                mid_chunks: Vec::new(),
                post_chunks: Vec::new(),
                ssnd_offset: 0,
                sound: Vec::new(),
            }
        }

        fn ext80(rate: u32) -> [u8; 10] {
            // normalized: mantissa = rate << (63 - floor(log2 rate))
            let lz = rate.leading_zeros();
            let mant = (rate as u64) << (63 - (31 - lz));
            let exp = (16383 + 31 - lz) as u16;
            let mut out = [0u8; 10];
            out[0..2].copy_from_slice(&exp.to_be_bytes());
            out[2..10].copy_from_slice(&mant.to_be_bytes());
            out
        }

        pub fn build(&self) -> Vec<u8> {
            let mut comm = Vec::new();
            comm.extend(self.channels.to_be_bytes());
            comm.extend(self.frames.to_be_bytes());
            comm.extend(self.bits.to_be_bytes());
            comm.extend(Self::ext80(self.rate));
            if self.aifc {
                comm.extend(self.compression);
                comm.push(0); // empty pstring compression name
            }
            let chunk = |id: &[u8; 4], data: &[u8]| -> Vec<u8> {
                let mut v = Vec::new();
                v.extend(*id);
                v.extend((data.len() as u32).to_be_bytes());
                v.extend(data);
                if data.len() % 2 == 1 {
                    v.push(0);
                }
                v
            };
            let mut ssnd = Vec::new();
            ssnd.extend(self.ssnd_offset.to_be_bytes());
            ssnd.extend(0u32.to_be_bytes()); // blockSize
            ssnd.extend(vec![0xEEu8; self.ssnd_offset as usize]);
            ssnd.extend(&self.sound);
            let mut body = if self.aifc {
                b"AIFC".to_vec()
            } else {
                b"AIFF".to_vec()
            };
            for (i, c) in self.pre_chunks.iter().enumerate() {
                body.extend(chunk(b"JUNK", c));
                let _ = i;
            }
            body.extend(chunk(b"COMM", &comm));
            for c in &self.mid_chunks {
                body.extend(chunk(b"MARK", c));
            }
            body.extend(chunk(b"SSND", &ssnd));
            for c in &self.post_chunks {
                body.extend(chunk(b"ANNO", c));
            }
            let mut out = b"FORM".to_vec();
            out.extend((body.len() as u32).to_be_bytes());
            out.extend(body);
            out
        }
    }

    /// Native AIFF bytes for integer samples (BE, left-justified container).
    pub fn be_int_pcm(samples: &[Vec<i64>], bits: u16) -> Vec<u8> {
        let ch = samples.len();
        let frames = samples.first().map_or(0, |s| s.len());
        let cb = (bits / 8) as usize;
        let mut pcm = vec![0u8; frames * ch * cb];
        for f in 0..frames {
            for (c, s) in samples.iter().enumerate() {
                let at = (f * ch + c) * cb;
                let v = s[f];
                match bits {
                    8 => pcm[at] = v as i8 as u8, // signed, not biased
                    16 => pcm[at..at + 2].copy_from_slice(&(v as i16).to_be_bytes()),
                    24 => pcm[at..at + 3].copy_from_slice(&(v as i32).to_be_bytes()[1..4]),
                    _ => pcm[at..at + 4].copy_from_slice(&(v as i32).to_be_bytes()),
                }
            }
        }
        pcm
    }

    pub fn be_float_pcm(words: &[Vec<u64>], bits: u16) -> Vec<u8> {
        let ch = words.len();
        let frames = words.first().map_or(0, |s| s.len());
        let cb = (bits / 8) as usize;
        let mut pcm = vec![0u8; frames * ch * cb];
        for f in 0..frames {
            for (c, s) in words.iter().enumerate() {
                let at = (f * ch + c) * cb;
                let b = s[f].to_be_bytes();
                pcm[at..at + cb].copy_from_slice(&b[8 - cb..]);
            }
        }
        pcm
    }

    #[test]
    fn ext80_rate() {
        for r in [8000u32, 44100, 48000, 96000, 22050] {
            assert_eq!(ext80_to_u32(&AiffBuilder::ext80(r)).unwrap(), r);
        }
    }

    #[test]
    fn int_roundtrip() {
        for bits in [8u16, 16, 24, 32] {
            let (lo, hi) = (-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1);
            let sig: Vec<Vec<i64>> = vec![
                vec![-100i64, 0, 100, -32768, 32767, -1, 1, 55],
                vec![5i64, -5, 500, -500, 0, 7, -9, 11],
            ]
            .into_iter()
            .map(|c| c.iter().map(|&v| v.clamp(lo, hi)).collect())
            .collect();
            let mut b = AiffBuilder::new(2, 44100, bits, 8);
            b.sound = be_int_pcm(&sig, bits);
            let file = b.build();
            let w = parse(&file).unwrap();
            assert_eq!(w.channels, 2);
            assert_eq!(w.rate, 44100);
            assert_eq!(w.bits, bits);
            assert_eq!(w.frames, 8);
            let ints = wav::channel_ints(&w);
            assert_eq!(ints, sig, "bits {bits}");
            // native conversion back is byte-exact
            let nat = to_native(&w.prefix, &w.pcm).unwrap();
            assert_eq!(nat, b.sound);
            let mut rebuilt = w.prefix.clone();
            rebuilt.extend(&nat);
            rebuilt.extend(&w.suffix);
            assert_eq!(rebuilt, file);
        }
    }

    #[test]
    fn aifc_sowt() {
        let sig = vec![vec![1i64, -200, 300, -4000]];
        let mut pcm = Vec::new();
        for &v in &sig[0] {
            pcm.extend((v as i16).to_le_bytes()); // sowt = little-endian
        }
        let mut b = AiffBuilder::new(1, 22050, 16, 4);
        b.aifc = true;
        b.compression = *b"sowt";
        b.sound = pcm;
        let file = b.build();
        let w = parse(&file).unwrap();
        assert_eq!(wav::channel_ints(&w), sig);
        let nat = to_native(&w.prefix, &w.pcm).unwrap();
        let mut rebuilt = w.prefix.clone();
        rebuilt.extend(&nat);
        rebuilt.extend(&w.suffix);
        assert_eq!(rebuilt, file);
    }

    #[test]
    fn aifc_float() {
        for (bits, comp) in [(32u16, *b"fl32"), (64, *b"fl64")] {
            let words: Vec<Vec<u64>> = vec![vec![
                1.5f32.to_bits() as u64,
                (-0.25f32).to_bits() as u64,
                12345.678f32.to_bits() as u64,
            ]]
            .into_iter()
            .map(|c| {
                c.iter()
                    .map(|&w| {
                        if bits == 64 {
                            ((w as u32 as i32) as f64).to_bits()
                        } else {
                            w
                        }
                    })
                    .collect()
            })
            .collect();
            let mut b = AiffBuilder::new(1, 48000, bits, 3);
            b.aifc = true;
            b.compression = comp;
            b.sound = be_float_pcm(&words, bits);
            let file = b.build();
            let w = parse(&file).unwrap();
            assert_eq!(w.tag, 3);
            assert_eq!(w.bits, bits);
            assert_eq!(wav::channel_words(&w), words, "bits {bits}");
            let nat = to_native(&w.prefix, &w.pcm).unwrap();
            let mut rebuilt = w.prefix.clone();
            rebuilt.extend(&nat);
            rebuilt.extend(&w.suffix);
            assert_eq!(rebuilt, file);
        }
    }

    #[test]
    fn extra_chunks_offset_padding() {
        let sig = vec![vec![10i64, 20, 30, -40, 50]];
        let mut b = AiffBuilder::new(1, 44100, 16, 5);
        b.pre_chunks = vec![vec![1, 2, 3]]; // odd size -> pad byte
        b.mid_chunks = vec![vec![9; 7]]; // odd size -> pad byte
        b.post_chunks = vec![vec![8; 6]];
        b.ssnd_offset = 4;
        b.sound = be_int_pcm(&sig, 16);
        let file = b.build();
        let w = parse(&file).unwrap();
        assert_eq!(wav::channel_ints(&w), sig);
        // prefix must contain the extra chunks and the offset bytes
        assert!(w.prefix.windows(4).any(|s| s == b"JUNK"));
        assert!(w.prefix.windows(4).any(|s| s == b"MARK"));
        assert!(w.prefix.ends_with(&[0xEE; 4]));
        assert!(w.suffix.windows(4).any(|s| s == b"ANNO"));
        let nat = to_native(&w.prefix, &w.pcm).unwrap();
        let mut rebuilt = w.prefix.clone();
        rebuilt.extend(&nat);
        rebuilt.extend(&w.suffix);
        assert_eq!(rebuilt, file);
        // odd-length sound data: partial frame goes to suffix
        let mut b2 = AiffBuilder::new(1, 44100, 16, 5);
        b2.sound = be_int_pcm(&sig, 16);
        b2.sound.push(0x77); // half a frame
        let file2 = b2.build();
        let w2 = parse(&file2).unwrap();
        assert_eq!(w2.frames, 5);
        assert_eq!(w2.suffix, vec![0x77, 0]); // tail byte + pad
        let nat2 = to_native(&w2.prefix, &w2.pcm).unwrap();
        let mut rebuilt2 = w2.prefix.clone();
        rebuilt2.extend(&nat2);
        rebuilt2.extend(&w2.suffix);
        assert_eq!(rebuilt2, file2);
    }

    #[test]
    fn comm_frames_cap() {
        // COMM declares fewer frames than the SSND data holds -> suffix gets the rest
        let sig = vec![vec![1i64, 2, 3, 4, 5, 6]];
        let mut b = AiffBuilder::new(1, 44100, 16, 4);
        b.sound = be_int_pcm(&sig, 16); // 6 frames of data
        let file = b.build();
        let w = parse(&file).unwrap();
        assert_eq!(w.frames, 4);
        assert_eq!(
            w.suffix.len(),
            4,
            "2 extra frames go to suffix (even, no pad)"
        );
        let ints = wav::channel_ints(&w);
        assert_eq!(ints, vec![vec![1i64, 2, 3, 4]]);
    }

    #[test]
    fn rejects() {
        assert!(parse(b"not aiff").is_err());
        // ulaw compression -> unsupported
        let mut b = AiffBuilder::new(1, 8000, 8, 4);
        b.aifc = true;
        b.compression = *b"ulaw";
        b.sound = vec![0; 4];
        let r = parse(&b.build());
        match r {
            Err(e) => assert!(e.contains("compression")),
            Ok(_) => panic!("ulaw should not parse"),
        }
        // 20-bit in a 3-byte container is supported; 40-bit would not be
        let mut b2 = AiffBuilder::new(1, 8000, 40, 4);
        b2.sound = vec![0; 20];
        assert!(parse(&b2.build()).is_err());
    }

    #[test]
    fn prefix_parse() {
        let sig = vec![vec![1i64, 2, 3]];
        let mut b = AiffBuilder::new(2, 96000, 24, 3);
        b.sound = be_int_pcm(&sig, 24);
        let w = parse(&b.build()).unwrap();
        let f = parse_prefix(&w.prefix).unwrap();
        assert_eq!(f.channels, 2);
        assert_eq!(f.rate, 96000);
        assert_eq!(f.bits, 24);
        assert!(parse_prefix(&w.prefix[..w.prefix.len() - 1]).is_err());
    }
}
