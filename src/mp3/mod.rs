//! MP3 container-level parsing: items (Raw | Run of frames), bit-exact
//! rebuild, and exact bit accounting stats. MPEG-1 Layer III only;
//! anything else lands in `Raw` untouched.

pub mod frame;
pub mod huffman;
pub mod model;
mod rc;
pub mod tables;

use crate::bits::{BitReader, BitWriter};
use frame::{Hdr, ParsedGrCh, SideInfo, decode_grch, encode_grch, header, parse_side, write_side};

/// Packed MSB-first bit string.
pub struct PackedBits {
    pub nbits: u64,
    pub bytes: Vec<u8>,
}

impl PackedBits {
    fn take(m: &[u8], start: u64, n: u64) -> PackedBits {
        let mut r = reader_at(m, start);
        let mut bw = BitWriter::new();
        for _ in 0..n {
            bw.write(r.read(1), 1);
        }
        PackedBits { nbits: n, bytes: bw.into_bytes() }
    }
}

fn reader_at(m: &[u8], bit: u64) -> BitReader<'_> {
    let byte = (bit / 8) as usize;
    let mut r = BitReader::new(m.get(byte..).unwrap_or(&[]));
    let _ = r.read((bit % 8) as u32);
    r
}

pub enum Item {
    Raw(Vec<u8>),
    Run(Run),
}

pub struct Run {
    pub frames: Vec<Frame>,
    /// Coverage of the main-data stream M in bit order: parsed granule
    /// channels and verbatim gap bit strings.
    pub chunks: Vec<Chunk>,
    /// Length of M in bytes (= sum of frame slot lengths).
    pub m_bytes: usize,
}

pub enum Chunk {
    Gap(PackedBits),
    GrCh { frame: usize, granule: usize, ch: usize, data: Box<ParsedGrCh> },
}

pub struct Frame {
    pub hdr: u32,
    pub crc: Option<u16>,
    pub side: SideInfo,
    pub slot: usize, // bytes contributed to M
    pub hdr_fields: Hdr,
    /// bit position in M where this frame's granule data starts (<0 if
    /// it begins before the run)
    pub data_start: i64,
    pub parsed: bool, // granule coverage was attempted
    pub parsed_grch: u8, // how many of the frame's granule channels parsed
}

pub struct Mp3File {
    pub items: Vec<Item>,
    /// valid-looking frame headers rejected during scan (isolated or truncated)
    pub invalid_frames: u64,
}

#[derive(Default, Clone, Debug)]
pub struct Mp3Stats {
    pub frames: u64,
    pub rate: u32,
    pub channels: u8,
    pub bitrate_min: u32,
    pub bitrate_max: u32,
    pub raw_bytes: u64,
    pub header_crc_bits: u64,
    pub side_bits: u64,
    pub scalefac_bits: u64,
    pub bigval_code_bits: u64,
    pub linbits_bits: u64,
    pub sign_bits: u64,
    pub count1_code_bits: u64,
    pub gap_bits: u64,
    pub unparsed_grch: u64,
    pub invalid_frames: u64,
    /// 0 = stored raw, 1 = modeled, 2 = modeled chained into the
    /// cross-track shared model (stage-2 recompression; v3 only)
    pub mode: u8,
    /// bytes actually stored in the container
    pub payload_bytes: u64,
    /// ideal -log2(p) bit cost per `model::cat` category (mode 1)
    pub coded: [f64; 10],
}

pub fn sniff(b: &[u8]) -> bool {
    if b.starts_with(b"ID3") {
        return true;
    }
    let Some(h) = header(b, 0) else { return false };
    let e = h.len;
    e == b.len() || header(b, e).is_some()
}

fn id3v2_len(b: &[u8]) -> Option<usize> {
    if b.len() < 10 || !b.starts_with(b"ID3") {
        return None;
    }
    let size = (u64::from(b[6] & 0x7f) << 21)
        | (u64::from(b[7] & 0x7f) << 14)
        | (u64::from(b[8] & 0x7f) << 7)
        | u64::from(b[9] & 0x7f);
    let footer = if b[5] & 0x10 != 0 { 10 } else { 0 };
    let total = 10usize.checked_add(size as usize)?.checked_add(footer)?;
    (total <= b.len()).then_some(total)
}

/// Split the file into items. Never fails: every byte lands somewhere.
pub fn parse(b: &[u8]) -> Mp3File {
    let mut items: Vec<Item> = Vec::new();
    let mut pos = id3v2_len(b).unwrap_or(0);
    let mut raw_start = 0; // ID3v2 tag itself is part of the first Raw item
    let mut invalid = 0u64;
    while pos < b.len() {
        let run_start = match header(b, pos) {
            Some(h)
                if pos + h.len <= b.len()
                    && (pos + h.len == b.len() || header(b, pos + h.len).is_some()) =>
            {
                pos
            }
            Some(_) => {
                invalid += 1;
                pos += 1;
                continue;
            }
            None => {
                pos += 1;
                continue;
            }
        };
        if pos > raw_start {
            items.push(Item::Raw(b[raw_start..pos].to_vec()));
        }
        // maximal run of consecutive valid frames
        let mut frames = Vec::new();
        let mut main_data = Vec::new();
        let mut p = run_start;
        while let Some(h) = header(b, p) {
            if p + h.len > b.len() {
                break;
            }
            let crc_len = if h.has_crc { 2 } else { 0 };
            let word = u32::from_be_bytes(b[p..p + 4].try_into().unwrap());
            let crc = h.has_crc.then(|| u16::from_be_bytes([b[p + 4], b[p + 5]]));
            let side_at = p + 4 + crc_len;
            let side = parse_side(&b[side_at..side_at + h.side_len], h.nch);
            let slot = h.len - 4 - crc_len - h.side_len;
            frames.push(Frame {
                hdr: word,
                crc,
                side,
                slot,
                hdr_fields: h,
                data_start: 0,
                parsed: false,
                parsed_grch: 0,
            });
            main_data.extend_from_slice(&b[side_at + h.side_len..p + h.len]);
            p += h.len;
        }
        // granule coverage over M
        let m_bytes = main_data.len();
        let chunks = cover(&mut frames, &main_data);
        items.push(Item::Run(Run { frames, chunks, m_bytes }));
        pos = p;
        raw_start = p;
    }
    if pos > raw_start {
        items.push(Item::Raw(b[raw_start..pos].to_vec()));
    }
    Mp3File { items, invalid_frames: invalid }
}

/// Parse granule data of every eligible frame and build gap coverage.
fn cover(frames: &mut [Frame], m: &[u8]) -> Vec<Chunk> {
    let m_bits = (m.len() * 8) as u64;
    // (start_bit, end_bit, frame, granule, ch, data), already in bit order
    let mut parsed: Vec<(u64, u64, usize, usize, usize, Box<ParsedGrCh>)> = Vec::new();
    let mut prev_end = 0i64;
    let mut off = 0usize;
    for (fi, f) in frames.iter_mut().enumerate() {
        f.data_start = 8 * (off as i64 - i64::from(f.side.mdb));
        let nch = f.hdr_fields.nch;
        let total: i64 = (0..2)
            .map(|g| (0..nch).map(|c| i64::from(f.side.gr[g][c].part2_3)).sum::<i64>())
            .sum();
        let end = f.data_start + total;
        let eligible = f.data_start >= 0
            && f.data_start >= prev_end
            && end <= 8 * (off + f.slot) as i64;
        f.parsed = eligible;
        if eligible {
            let mut cur = f.data_start;
            for g in 0..2 {
                for c in 0..nch {
                    let cs = &f.side.gr[g][c];
                    let p23 = i64::from(cs.part2_3);
                    if let Some(d) = decode_grch_at(
                        m, cur as u64, cs, &f.side.scfsi[c], g, f.hdr_fields.rate_idx,
                    ) {
                        f.parsed_grch += 1;
                        parsed.push((cur as u64, (cur + p23) as u64, fi, g, c, Box::new(d)));
                    }
                    cur += p23;
                }
            }
            prev_end = end;
        }
        off += f.slot;
    }
    // chunks in position order with gap fills
    let mut chunks = Vec::new();
    let mut cursor = 0u64;
    for (start, end, fi, g, c, d) in parsed {
        if start > cursor {
            chunks.push(Chunk::Gap(PackedBits::take(m, cursor, start - cursor)));
        }
        chunks.push(Chunk::GrCh { frame: fi, granule: g, ch: c, data: d });
        cursor = end;
    }
    if cursor < m_bits {
        chunks.push(Chunk::Gap(PackedBits::take(m, cursor, m_bits - cursor)));
    }
    chunks
}

/// Decode one gr/ch at bit `pos`, verify exact consumption AND bit-exact
/// re-encodability; either failure leaves it unparsed.
fn decode_grch_at(
    m: &[u8],
    pos: u64,
    cs: &frame::ChSide,
    scfsi: &[bool; 4],
    granule: usize,
    rate_idx: usize,
) -> Option<ParsedGrCh> {
    let p23 = i64::from(cs.part2_3);
    let mut r = reader_at(m, pos);
    let d = decode_grch(&mut r, cs, scfsi, granule, rate_idx)?;
    // verify: re-encoding must reproduce exactly these bits
    let mut bw = BitWriter::new();
    encode_grch(&mut bw, cs, scfsi, granule, rate_idx, &d);
    if bw.bits != p23 as u64 {
        return None;
    }
    let enc = bw.into_bytes();
    let mut cr = reader_at(m, pos);
    let mut rr = BitReader::new(&enc);
    for _ in 0..p23 {
        if cr.read(1) != rr.read(1) {
            return None;
        }
    }
    Some(d)
}

pub fn rebuild(f: &Mp3File) -> Vec<u8> {
    let mut out = Vec::new();
    for item in &f.items {
        match item {
            Item::Raw(b) => out.extend_from_slice(b),
            Item::Run(run) => {
                let mut bw = BitWriter::new();
                for ch in &run.chunks {
                    match ch {
                        Chunk::Gap(g) => {
                            let mut r = BitReader::new(&g.bytes);
                            for _ in 0..g.nbits {
                                bw.write(r.read(1), 1);
                            }
                        }
                        Chunk::GrCh { frame, granule, ch, data } => {
                            let fr = &run.frames[*frame];
                            let cs = &fr.side.gr[*granule][*ch];
                            encode_grch(&mut bw, cs, &fr.side.scfsi[*ch], *granule, fr.hdr_fields.rate_idx, data);
                        }
                    }
                }
                let m = bw.into_bytes();
                debug_assert_eq!(m.len(), run.m_bytes);
                let mut pos = 0usize;
                for fr in &run.frames {
                    out.extend(fr.hdr.to_be_bytes());
                    if let Some(c) = fr.crc {
                        out.extend(c.to_be_bytes());
                    }
                    let mut sbw = BitWriter::new();
                    write_side(&mut sbw, &fr.side, fr.hdr_fields.nch);
                    debug_assert_eq!(sbw.bits, fr.hdr_fields.side_len as u64 * 8);
                    out.extend(sbw.into_bytes());
                    out.extend(&m[pos..pos + fr.slot]);
                    pos += fr.slot;
                }
            }
        }
    }
    out
}

pub fn stats(f: &Mp3File) -> Mp3Stats {
    let mut s = Mp3Stats { bitrate_min: u32::MAX, ..Default::default() };
    for item in &f.items {
        match item {
            Item::Raw(b) => s.raw_bytes += b.len() as u64,
            Item::Run(run) => {
                for fr in &run.frames {
                    s.frames += 1;
                    s.rate = fr.hdr_fields.rate;
                    s.channels = fr.hdr_fields.nch as u8;
                    s.bitrate_min = s.bitrate_min.min(fr.hdr_fields.bitrate);
                    s.bitrate_max = s.bitrate_max.max(fr.hdr_fields.bitrate);
                    s.header_crc_bits += 32 + u64::from(fr.crc.is_some()) * 16;
                    s.side_bits += fr.hdr_fields.side_len as u64 * 8;
                }
                for ch in &run.chunks {
                    match ch {
                        Chunk::Gap(g) => s.gap_bits += g.nbits,
                        Chunk::GrCh { data, .. } => {
                            s.scalefac_bits += u64::from(data.sf_bits);
                            s.bigval_code_bits += u64::from(data.code_bits);
                            s.linbits_bits += u64::from(data.lin_bits);
                            s.sign_bits += u64::from(data.sign_bits);
                            s.count1_code_bits += u64::from(data.quad_code_bits);
                        }
                    }
                }
                // gr/ch that were never parsed
                for fr in &run.frames {
                    s.unparsed_grch +=
                        (2 * fr.hdr_fields.nch) as u64 - u64::from(fr.parsed_grch);
                }
            }
        }
    }
    if s.bitrate_min == u32::MAX {
        s.bitrate_min = 0;
    }
    s.invalid_frames = f.invalid_frames;
    s
}

#[cfg(test)]
pub(crate) mod tests;
