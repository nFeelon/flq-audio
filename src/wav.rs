//! RIFF/WAVE parsing: prefix / PCM / suffix split and channel deinterleave.

pub const MAX_CHANNELS: u16 = 16;

pub struct Wav {
    pub tag: u16, // 1 PCM int, 3 IEEE float
    pub channels: u16,
    pub rate: u32,
    pub bits: u16,
    pub frames: u64,
    pub prefix: Vec<u8>, // everything before PCM (ends right after the `data` header)
    pub pcm: Vec<u8>,    // whole frames only
    pub suffix: Vec<u8>, // partial frame tail + pad + any later chunks
}

#[derive(Clone, Copy)]
pub struct Fmt {
    pub tag: u16,
    pub channels: u16,
    pub rate: u32,
    pub bits: u16,
    pub block_align: u16,
}

fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Walk RIFF chunks until `data`; returns the parsed fmt and the data header position.
/// Works on a whole file and on a bare prefix (where `data` has no payload).
fn scan(bytes: &[u8]) -> Result<(Fmt, usize, u32), String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    let mut pos = 12usize;
    let mut fmt: Option<Fmt> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32le(bytes, pos + 4);
        let dstart = pos + 8;
        if id == b"fmt " {
            if size < 16 || dstart + 16 > bytes.len() {
                return Err("malformed fmt chunk".into());
            }
            let mut tag = u16le(bytes, dstart);
            if tag == 0xFFFE {
                if size < 40 || dstart + 40 > bytes.len() {
                    return Err("malformed extensible fmt chunk".into());
                }
                tag = u16le(bytes, dstart + 24); // subformat GUID tag
            }
            let channels = u16le(bytes, dstart + 2);
            let rate = u32le(bytes, dstart + 4);
            let block_align = u16le(bytes, dstart + 12);
            let bits = u16le(bytes, dstart + 14);
            fmt = Some(Fmt { tag, channels, rate, bits, block_align });
        } else if id == b"data" {
            let f = fmt.ok_or("data chunk precedes fmt")?;
            return Ok((f, pos, size));
        }
        // chunks are padded to even sizes
        let next = dstart.saturating_add(size as usize).saturating_add((size & 1) as usize);
        if next <= pos {
            break;
        }
        pos = next;
    }
    Err("no data chunk".into())
}

fn check_fmt(f: &Fmt) -> Result<(), String> {
    let ok = match f.tag {
        1 => matches!(f.bits, 8 | 16 | 24 | 32),
        3 => matches!(f.bits, 32 | 64),
        _ => false,
    };
    if !ok {
        return Err(format!(
            "unsupported WAV format (tag {}, {} bits)",
            f.tag, f.bits
        ));
    }
    if f.channels == 0 || f.channels > MAX_CHANNELS {
        return Err(format!("unsupported channel count {}", f.channels));
    }
    let want = u32::from(f.channels) * (u32::from(f.bits) / 8);
    if u32::from(f.block_align) != want {
        return Err(format!(
            "block align {} != {} expected",
            f.block_align, want
        ));
    }
    Ok(())
}

pub fn parse(bytes: &[u8]) -> Result<Wav, String> {
    let (f, data_pos, size) = scan(bytes)?;
    check_fmt(&f)?;
    let pcm_start = data_pos + 8;
    let avail = (size as usize).min(bytes.len().saturating_sub(pcm_start));
    let frames = avail / f.block_align as usize;
    let pcm_len = frames * f.block_align as usize;
    Ok(Wav {
        tag: f.tag,
        channels: f.channels,
        rate: f.rate,
        bits: f.bits,
        frames: frames as u64,
        prefix: bytes[..pcm_start].to_vec(),
        pcm: bytes[pcm_start..pcm_start + pcm_len].to_vec(),
        suffix: bytes[pcm_start + pcm_len..].to_vec(),
    })
}

/// Recover the fmt from a stored prefix (ends exactly after the `data` header).
pub fn parse_prefix(prefix: &[u8]) -> Result<Fmt, String> {
    if prefix.len() < 8 || &prefix[prefix.len() - 8..prefix.len() - 4] != b"data" {
        return Err("prefix does not end at a data header".into());
    }
    let (f, data_pos, _) = scan(prefix)?;
    if data_pos + 8 != prefix.len() {
        return Err("prefix does not end at a data header".into());
    }
    check_fmt(&f)?;
    Ok(f)
}

/// Deinterleave integer PCM into per-channel signed streams (u8 centered at -128).
pub fn channel_ints(w: &Wav) -> Vec<Vec<i64>> {
    let ch = w.channels as usize;
    let frames = w.frames as usize;
    let mut out = vec![vec![0i64; frames]; ch];
    let bps = (w.bits / 8) as usize;
    for (f, frame) in w.pcm.chunks_exact(ch * bps).enumerate() {
        for (c, oc) in out.iter_mut().enumerate() {
            let s = &frame[c * bps..(c + 1) * bps];
            oc[f] = match w.bits {
                8 => i64::from(s[0]) - 128,
                16 => i64::from(i16::from_le_bytes([s[0], s[1]])),
                24 => {
                    let v = u32::from_le_bytes([s[0], s[1], s[2], 0]);
                    i64::from((v << 8) as i32 >> 8)
                }
                _ => i64::from(i32::from_le_bytes([s[0], s[1], s[2], s[3]])),
            };
        }
    }
    out
}

/// Deinterleave float PCM into per-channel raw words (u32 or u64 in low bits).
pub fn channel_words(w: &Wav) -> Vec<Vec<u64>> {
    let ch = w.channels as usize;
    let frames = w.frames as usize;
    let mut out = vec![vec![0u64; frames]; ch];
    let bps = (w.bits / 8) as usize;
    for (f, frame) in w.pcm.chunks_exact(ch * bps).enumerate() {
        for (c, oc) in out.iter_mut().enumerate() {
            let s = &frame[c * bps..(c + 1) * bps];
            oc[f] = if bps == 4 {
                u64::from(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
            } else {
                u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]])
            };
        }
    }
    out
}

/// Re-interleave signed channel streams into PCM bytes.
pub fn ints_to_pcm(ints: &[Vec<i64>], bits: u16, frames: usize) -> Vec<u8> {
    let ch = ints.len();
    let bps = (bits / 8) as usize;
    let mut pcm = vec![0u8; frames * ch * bps];
    for f in 0..frames {
        for (c, s) in ints.iter().enumerate() {
            let at = (f * ch + c) * bps;
            let v = s[f];
            match bits {
                8 => pcm[at] = (v + 128) as u8,
                16 => pcm[at..at + 2].copy_from_slice(&(v as i16).to_le_bytes()),
                24 => {
                    let b = (v as i32).to_le_bytes();
                    pcm[at..at + 3].copy_from_slice(&b[..3]);
                }
                _ => pcm[at..at + 4].copy_from_slice(&(v as i32).to_le_bytes()),
            }
        }
    }
    pcm
}

/// Re-interleave raw word streams into PCM bytes (4 or 8 bytes per sample).
pub fn words_to_pcm(words: &[Vec<u64>], bits: u16, frames: usize) -> Vec<u8> {
    let ch = words.len();
    let bps = (bits / 8) as usize;
    let mut pcm = vec![0u8; frames * ch * bps];
    for f in 0..frames {
        for (c, s) in words.iter().enumerate() {
            let at = (f * ch + c) * bps;
            let b = s[f].to_le_bytes();
            pcm[at..at + bps].copy_from_slice(&b[..bps]);
        }
    }
    pcm
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fmt chunk payload (16 bytes).
    pub fn fmt_chunk(tag: u16, ch: u16, rate: u32, bits: u16) -> Vec<u8> {
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

    pub fn riff(chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
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

    fn int_wav(ch: u16, rate: u32, bits: u16, frames: usize, fill: i64) -> Vec<u8> {
        let bps = (bits / 8) as usize;
        let mut pcm = vec![0u8; frames * ch as usize * bps];
        for i in 0..frames * ch as usize {
            let at = i * bps;
            let v = fill + i as i64;
            match bits {
                8 => pcm[at] = (v + 128) as u8,
                16 => pcm[at..at + 2].copy_from_slice(&(v as i16).to_le_bytes()),
                24 => pcm[at..at + 3].copy_from_slice(&(v as i32).to_le_bytes()[..3]),
                _ => pcm[at..at + 4].copy_from_slice(&(v as i32).to_le_bytes()),
            }
        }
        riff(&[(b"fmt ", fmt_chunk(1, ch, rate, bits)), (b"data", pcm)])
    }

    #[test]
    fn roundtrip_int_formats() {
        for bits in [8u16, 16, 24, 32] {
            for ch in [1u16, 2, 3] {
                let w = int_wav(ch, 44100, bits, 100, -50);
                let p = parse(&w).unwrap();
                assert_eq!(p.bits, bits);
                assert_eq!(p.channels, ch);
                assert_eq!(p.frames, 100);
                assert_eq!(p.prefix.len() + p.pcm.len() + p.suffix.len(), w.len());
                let ints = channel_ints(&p);
                let pcm = ints_to_pcm(&ints, bits, 100);
                assert_eq!(pcm, p.pcm, "bits {bits} ch {ch} PCM mismatch");
            }
        }
    }

    #[test]
    fn float_words_roundtrip() {
        let mut pcm = Vec::new();
        for v in [1.5f64, -0.25, 12345.678] {
            pcm.extend(v.to_le_bytes());
        }
        let w = riff(&[(b"fmt ", fmt_chunk(3, 1, 48000, 64)), (b"data", pcm)]);
        let p = parse(&w).unwrap();
        let words = channel_words(&p);
        assert_eq!(words_to_pcm(&words, 64, 3), p.pcm);
    }

    #[test]
    fn chunks_before_and_after_data() {
        // LIST before data, odd data size + pad + trailing chunk
        let pcm = vec![1u8, 2, 3]; // odd, 1ch u8 -> 3 frames + partial? align=1, 3 frames
        let w = riff(&[
            (b"fmt ", fmt_chunk(1, 1, 8000, 8)),
            (b"LIST", vec![9u8; 10]),
            (b"data", pcm),
            (b"JUNK", vec![5u8; 6]),
        ]);
        let p = parse(&w).unwrap();
        assert_eq!(p.frames, 3);
        assert_eq!(&p.prefix[..4], b"RIFF");
        assert!(p.prefix.ends_with(b"data\x03\x00\x00\x00"));
        assert!(p.suffix.starts_with(&[0][..]) || p.suffix.starts_with(b"JUNK"), "suffix = pad + JUNK");
        assert!(p.suffix.windows(4).any(|s| s == b"JUNK"));
        let mut rebuilt = p.prefix.clone();
        rebuilt.extend(&p.pcm);
        rebuilt.extend(&p.suffix);
        assert_eq!(rebuilt, w);
    }

    #[test]
    fn partial_frame_goes_to_suffix() {
        // data size 7 with align 4 (2ch i16): 1 frame + 3 tail bytes
        let w = {
            let mut body = b"WAVE".to_vec();
            body.extend(b"fmt ");
            body.extend(16u32.to_le_bytes());
            body.extend(fmt_chunk(1, 2, 8000, 16));
            body.extend(b"data");
            body.extend(7u32.to_le_bytes());
            body.extend([1u8, 0, 2, 0, 9, 9, 9]);
            body.push(0); // pad for odd size
            let mut out = b"RIFF".to_vec();
            out.extend((body.len() as u32).to_le_bytes());
            out.extend(body);
            out
        };
        let p = parse(&w).unwrap();
        assert_eq!(p.frames, 1);
        assert_eq!(p.pcm, vec![1, 0, 2, 0]);
        assert_eq!(p.suffix, vec![9, 9, 9, 0]);
        let mut rebuilt = p.prefix.clone();
        rebuilt.extend(&p.pcm);
        rebuilt.extend(&p.suffix);
        assert_eq!(rebuilt, w);
    }

    #[test]
    fn extensible_fmt() {
        // WAVE_FORMAT_EXTENSIBLE (0xFFFE) wrapping PCM subformat
        let mut payload = Vec::new();
        payload.extend(0xFFFEu16.to_le_bytes());
        payload.extend(2u16.to_le_bytes());
        payload.extend(48000u32.to_le_bytes());
        payload.extend(192000u32.to_le_bytes());
        payload.extend(4u16.to_le_bytes());
        payload.extend(16u16.to_le_bytes());
        payload.extend(22u16.to_le_bytes()); // cbSize
        payload.extend(16u16.to_le_bytes()); // valid bits
        payload.extend(3u32.to_le_bytes()); // channel mask
        payload.extend(1u16.to_le_bytes()); // subformat tag = PCM
        payload.extend([0u8; 14]); // rest of GUID
        let w = riff(&[(b"fmt ", payload), (b"data", vec![0u8; 8])]);
        let p = parse(&w).unwrap();
        assert_eq!(p.tag, 1);
        assert_eq!(p.channels, 2);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse(b"not a wav").is_err());
        assert!(parse(&riff(&[(b"data", vec![0u8; 8])])).is_err()); // no fmt
        // ADPCM tag 2
        assert!(parse(&riff(&[(b"fmt ", fmt_chunk(2, 1, 8000, 4)), (b"data", vec![0u8; 4])])).is_err());
        // missing data
        assert!(parse(&riff(&[(b"fmt ", fmt_chunk(1, 1, 8000, 8))])).is_err());
    }

    #[test]
    fn prefix_parse() {
        let w = int_wav(2, 48000, 16, 10, 0);
        let p = parse(&w).unwrap();
        let f = parse_prefix(&p.prefix).unwrap();
        assert_eq!(f.channels, 2);
        assert_eq!(f.rate, 48000);
        assert_eq!(f.bits, 16);
        assert!(parse_prefix(&p.prefix[..p.prefix.len() - 1]).is_err());
    }
}
