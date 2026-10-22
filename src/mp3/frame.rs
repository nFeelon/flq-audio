//! MPEG-1 Layer III frame: header, side info, granule-channel decode/encode.

use crate::bits::{BitReader, BitWriter};
use crate::mp3::huffman::{decode_symbol, encode_symbol};
use crate::mp3::tables::{SCALE_FACTOR_BAND_INDICES, SCALE_FACTOR_SIZES};

const BITRATES: [u32; 16] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
];
const RATES: [u32; 3] = [44100, 48000, 32000];

#[derive(Clone, Copy)]
pub struct Hdr {
    pub len: usize,      // whole frame length in bytes
    pub has_crc: bool,
    pub bitrate: u32,    // kbps
    pub rate: u32,
    pub rate_idx: usize, // scalefactor-band table row
    pub nch: usize,
    pub side_len: usize, // bytes after header+CRC
}

/// Valid MPEG-1 Layer III header at `b[pos]` -> frame layout.
pub fn header(b: &[u8], pos: usize) -> Option<Hdr> {
    let h = b.get(pos..pos + 4)?;
    if h[0] != 0xff || h[1] & 0xe0 != 0xe0 {
        return None;
    }
    if (h[1] >> 3) & 3 != 3 || (h[1] >> 1) & 3 != 1 {
        return None; // not MPEG-1 Layer III
    }
    let has_crc = h[1] & 1 == 0;
    let bri = (h[2] >> 4) as usize;
    let sri = ((h[2] >> 2) & 3) as usize;
    let bitrate = *BITRATES.get(bri)?;
    if bitrate == 0 || sri == 3 || h[3] & 3 == 2 {
        return None; // free format, reserved rate, reserved emphasis
    }
    let rate = RATES[sri];
    let pad = usize::from(h[2] >> 1 & 1);
    let len = 144 * 1000 * bitrate as usize / rate as usize + pad;
    let mode = h[3] >> 6;
    let nch = if mode == 3 { 1 } else { 2 };
    Some(Hdr {
        len,
        has_crc,
        bitrate,
        rate,
        rate_idx: sri,
        nch,
        side_len: if nch == 1 { 17 } else { 32 },
    })
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Block {
    Long { region0: u8, region1: u8 },
    Switched { block_type: u8, mixed: bool },
}

impl Default for Block {
    fn default() -> Self {
        Block::Long { region0: 0, region1: 0 }
    }
}

#[derive(Clone, Default)]
pub struct ChSide {
    pub part2_3: u16,
    pub big_values: u16,
    pub global_gain: u8,
    pub scalefac_compress: u8,
    pub window_switching: bool,
    pub block: Block,
    pub tabs: [u8; 3], // [2] ignored for switched blocks
    pub subblock: [u8; 3],
    pub preflag: bool,
    pub scalefac_scale: bool,
    pub count1: bool,
}

pub struct SideInfo {
    pub mdb: u16,
    pub private: u16, // 5 bits mono / 3 bits stereo, raw value
    pub scfsi: [[bool; 4]; 2],
    pub gr: [[ChSide; 2]; 2], // [granule][channel]
}

pub fn parse_side(b: &[u8], nch: usize) -> SideInfo {
    let mut r = BitReader::new(b);
    let mdb = r.read(9) as u16;
    let private = r.read(if nch == 1 { 5 } else { 3 }) as u16;
    let mut scfsi = [[false; 4]; 2];
    for row in scfsi.iter_mut().take(nch) {
        for band in row {
            *band = r.read(1) == 1;
        }
    }
    let mut gr: [[ChSide; 2]; 2] = [
        [ChSide::default(), ChSide::default()],
        [ChSide::default(), ChSide::default()],
    ];
    for g in &mut gr[..] {
        for cs in &mut g[..nch] {
            let part2_3 = r.read(12) as u16;
            let big_values = r.read(9) as u16;
            let global_gain = r.read(8) as u8;
            let scalefac_compress = r.read(4) as u8;
            let ws = r.read(1) == 1;
            let (block, tabs, subblock) = if ws {
                let bt = r.read(2) as u8;
                let mixed = r.read(1) == 1;
                let tabs = [r.read(5) as u8, r.read(5) as u8, 0];
                let subblock = [r.read(3) as u8, r.read(3) as u8, r.read(3) as u8];
                (Block::Switched { block_type: bt, mixed }, tabs, subblock)
            } else {
                let tabs = [r.read(5) as u8, r.read(5) as u8, r.read(5) as u8];
                let region0 = r.read(4) as u8;
                let region1 = r.read(3) as u8;
                (Block::Long { region0, region1 }, tabs, [0; 3])
            };
            *cs = ChSide {
                part2_3,
                big_values,
                global_gain,
                scalefac_compress,
                window_switching: ws,
                block,
                tabs,
                subblock,
                preflag: r.read(1) == 1,
                scalefac_scale: r.read(1) == 1,
                count1: r.read(1) == 1,
            };
        }
    }
    SideInfo { mdb, private, scfsi, gr }
}

/// Re-serialize parsed side info. `bw` must end byte-aligned.
pub fn write_side(bw: &mut BitWriter, s: &SideInfo, nch: usize) {
    bw.write(u64::from(s.mdb), 9);
    bw.write(u64::from(s.private), if nch == 1 { 5 } else { 3 });
    for c in 0..nch {
        for &band in &s.scfsi[c] {
            bw.write(u64::from(band), 1);
        }
    }
    for g in &s.gr[..] {
        for cs in &g[..nch] {
            bw.write(u64::from(cs.part2_3), 12);
            bw.write(u64::from(cs.big_values), 9);
            bw.write(u64::from(cs.global_gain), 8);
            bw.write(u64::from(cs.scalefac_compress), 4);
            bw.write(u64::from(cs.window_switching), 1);
            match cs.block {
                Block::Switched { block_type, mixed } => {
                    bw.write(u64::from(block_type), 2);
                    bw.write(u64::from(mixed), 1);
                    bw.write(u64::from(cs.tabs[0]), 5);
                    bw.write(u64::from(cs.tabs[1]), 5);
                    for &v in &cs.subblock {
                        bw.write(u64::from(v), 3);
                    }
                }
                Block::Long { region0, region1 } => {
                    for &t in &cs.tabs {
                        bw.write(u64::from(t), 5);
                    }
                    bw.write(u64::from(region0), 4);
                    bw.write(u64::from(region1), 3);
                }
            }
            bw.write(u64::from(cs.preflag), 1);
            bw.write(u64::from(cs.scalefac_scale), 1);
            bw.write(u64::from(cs.count1), 1);
        }
    }
}

/// Transmitted scalefactor (len, count) groups in bitstream order.
/// `scfsi` only applies to long blocks of granule 1.
pub fn scalefac_layout(cs: &ChSide, scfsi: &[bool; 4], granule: usize) -> [(u8, u16); 4] {
    let (l1, l2) = SCALE_FACTOR_SIZES[cs.scalefac_compress as usize];
    match cs.block {
        Block::Switched { block_type: 2, mixed: false } => {
            [(l1 as u8, 18), (l2 as u8, 18), (0, 0), (0, 0)]
        }
        Block::Switched { block_type: 2, mixed: true } => {
            [(l1 as u8, 8), (l1 as u8, 9), (l2 as u8, 18), (0, 0)]
        }
        _ => {
            // long windows, incl. start/end switched blocks
            let mut out = [(0u8, 0u16); 4];
            let counts = [6u16, 5, 5, 5];
            for i in 0..4 {
                let len = if i < 2 { l1 } else { l2 } as u8;
                let tx = granule == 0 || !scfsi[i];
                out[i] = (if tx { len } else { 0 }, counts[i]);
            }
            out
        }
    }
}

/// Region boundaries (r1_start, r2_start) in coefficient index for the
/// big_values tables, or None if the block's counts are out of range.
pub fn region_bounds(cs: &ChSide, rate_idx: usize) -> Option<(usize, usize)> {
    let sfb = &SCALE_FACTOR_BAND_INDICES[rate_idx].0;
    match cs.block {
        Block::Switched { block_type, .. } => match block_type {
            2 => Some((36, 576)),
            1 | 3 => Some((usize::try_from(sfb[8]).ok()?, usize::try_from(sfb[22]).ok()?)),
            _ => None,
        },
        Block::Long { region0, region1 } => {
            let a = usize::from(region0) + 1;
            let b = a + usize::from(region1) + 1;
            Some((usize::try_from(*sfb.get(a)?).ok()?, usize::try_from(*sfb.get(b)?).ok()?))
        }
    }
}

pub struct ParsedGrCh {
    pub scalefacs: Vec<u8>, // transmitted values, bitstream order
    pub ix: [i16; 576],
    pub count1_quads: u16,
    // bit accounting inside part2_3
    pub sf_bits: u32,
    pub code_bits: u32,   // big-value tree codes
    pub lin_bits: u32,
    pub sign_bits: u32,   // all sign bits (pairs + quads)
    pub quad_code_bits: u32,
}

/// Decode one granule channel at `r`, consuming exactly `p23` bits.
/// `scfsi` is this channel's scfsi bands; `granule` is 0 or 1.
pub fn decode_grch(
    r: &mut BitReader,
    cs: &ChSide,
    scfsi: &[bool; 4],
    granule: usize,
    rate_idx: usize,
) -> Option<ParsedGrCh> {
    let p23 = u32::from(cs.part2_3);
    let (r1, r2) = region_bounds(cs, rate_idx)?;
    let is_len = usize::from(cs.big_values) * 2;
    if is_len > 576 {
        return None;
    }
    let layout = scalefac_layout(cs, scfsi, granule);
    let mut d = ParsedGrCh {
        scalefacs: Vec::new(),
        ix: [0; 576],
        count1_quads: 0,
        sf_bits: 0,
        code_bits: 0,
        lin_bits: 0,
        sign_bits: 0,
        quad_code_bits: 0,
    };
    for &(len, count) in &layout {
        for _ in 0..count {
            let v = r.read(u32::from(len)) as u8;
            d.sf_bits += u32::from(len);
            d.scalefacs.push(v);
        }
    }
    if d.sf_bits > p23 {
        return None;
    }
    let huff_len = p23 - d.sf_bits;
    let mut used = 0u32;
    let mut pos = 0usize;
    while pos < is_len {
        let table = if pos < r1 { cs.tabs[0] } else if pos < r2 { cs.tabs[1] } else { cs.tabs[2] };
        let (sym, c) = decode_symbol(r, usize::from(table))?;
        used += c;
        d.code_bits += c;
        let lin = crate::mp3::tables::HUFFMAN_TABLES[usize::from(table)].linbits as u32;
        let (mut x, mut y) = (i64::from(sym >> 4), i64::from(sym & 15));
        if lin > 0 && x == 15 {
            x += r.read(lin) as i64;
            used += lin;
            d.lin_bits += lin;
        }
        if x > 0 {
            if r.read(1) == 1 {
                x = -x;
            }
            used += 1;
            d.sign_bits += 1;
        }
        if lin > 0 && y == 15 {
            y += r.read(lin) as i64;
            used += lin;
            d.lin_bits += lin;
        }
        if y > 0 {
            if r.read(1) == 1 {
                y = -y;
            }
            used += 1;
            d.sign_bits += 1;
        }
        d.ix[pos] = x as i16;
        d.ix[pos + 1] = y as i16;
        pos += 2;
    }
    // count1 quadruples while bits remain and index < 576
    let quad_table = if cs.count1 { 33 } else { 32 };
    while pos < 576 && used < huff_len {
        let (sym, c) = decode_symbol(r, quad_table)?;
        used += c;
        d.quad_code_bits += c;
        d.count1_quads += 1;
        for (i, v) in [sym >> 3 & 1, sym >> 2 & 1, sym >> 1 & 1, sym & 1]
            .into_iter()
            .enumerate()
        {
            let mut q = i64::from(v);
            if q > 0 {
                if r.read(1) == 1 {
                    q = -q;
                }
                used += 1;
                d.sign_bits += 1;
            }
            if pos + i < 576 {
                d.ix[pos + i] = q as i16;
            }
        }
        pos += 4;
    }
    if pos > 576 || used != huff_len || r.overrun {
        return None;
    }
    Some(d)
}

/// Re-encode a parsed granule channel. Output bit count must equal
/// `cs.part2_3` (checked by the caller/tests).
pub fn encode_grch(
    bw: &mut BitWriter,
    cs: &ChSide,
    scfsi: &[bool; 4],
    granule: usize,
    rate_idx: usize,
    d: &ParsedGrCh,
) {
    let layout = scalefac_layout(cs, scfsi, granule);
    let mut it = d.scalefacs.iter();
    for &(len, count) in &layout {
        for _ in 0..count {
            bw.write(u64::from(*it.next().unwrap_or(&0)), u32::from(len));
        }
    }
    let (r1, r2) = region_bounds(cs, rate_idx).unwrap_or((36, 576));
    let is_len = usize::from(cs.big_values) * 2;
    let mut pos = 0usize;
    while pos < is_len {
        let table = usize::from(if pos < r1 { cs.tabs[0] } else if pos < r2 { cs.tabs[1] } else { cs.tabs[2] });
        let lin = crate::mp3::tables::HUFFMAN_TABLES[table].linbits as u32;
        let (x, y) = (i64::from(d.ix[pos]), i64::from(d.ix[pos + 1]));
        let (xs, ys) = (x.unsigned_abs().min(15) as usize, y.unsigned_abs().min(15) as usize);
        encode_symbol(bw, table, (xs << 4) | ys);
        if lin > 0 && xs == 15 {
            bw.write(x.unsigned_abs() - 15, lin);
        }
        if x != 0 {
            bw.write(u64::from(x < 0), 1);
        }
        if lin > 0 && ys == 15 {
            bw.write(y.unsigned_abs() - 15, lin);
        }
        if y != 0 {
            bw.write(u64::from(y < 0), 1);
        }
        pos += 2;
    }
    let quad_table = if cs.count1 { 33 } else { 32 };
    for q in 0..usize::from(d.count1_quads) {
        let i = is_len + q * 4;
        let v: Vec<i64> = (0..4).map(|k| i64::from(d.ix.get(i + k).copied().unwrap_or(0))).collect();
        let sym = (usize::from(v[0] != 0) << 3)
            | (usize::from(v[1] != 0) << 2)
            | (usize::from(v[2] != 0) << 1)
            | usize::from(v[3] != 0);
        encode_symbol(bw, quad_table, sym);
        for &q in &v {
            if q != 0 {
                bw.write(u64::from(q < 0), 1);
            }
        }
    }
}

// `encode_grch` output must equal part2_3 bits for a parsed gr/ch — verified
// by re-encoding in `decode_grch_at` (mp3/mod.rs).
