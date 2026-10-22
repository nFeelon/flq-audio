//! Lossless recompression of a parsed Mp3File (Stage 2, container mode 1).
//! One range coder for everything: headers, side info, scalefactors,
//! coefficient magnitudes (context-modeled unary + linbits escapes), signs,
//! gaps and raw items. `decompress` mirrors `compress` exactly.

use super::frame::{
    Block, ChSide, ParsedGrCh, SideInfo, encode_grch, header, scalefac_layout, write_side,
};
use super::rc::{Bit, Decoder as Dec, Encoder as Enc, LIMIT};
use super::{Chunk, Frame, Item, Mp3File, PackedBits, Run};
use crate::bits::BitWriter;
use std::sync::OnceLock;

/// 2b: integer logistic mixing on the coefficient unary decisions.
/// High bit of the payload's first byte marks a mixer payload.
const USE_MIXER: bool = true;
const MIX_FLAG: u8 = 0x80;

/// Cost categories (stats only).
pub mod cat {
    pub const HDR: usize = 0;
    pub const SIDE: usize = 1;
    pub const SF: usize = 2;
    pub const MAG0: usize = 3;
    pub const MAGU: usize = 4;
    pub const MAGE: usize = 5;
    pub const SIGN: usize = 6;
    pub const GAP: usize = 7;
    pub const RAW: usize = 8;
    pub const MISC: usize = 9;
}
pub const NCAT: usize = 10;

// ---------------------------------------------------------------- contexts

/// Exp-Golomb set: zero flag, sign, unary exponent, mantissa.
#[derive(Clone)]
struct Eg {
    z: Bit,
    s: Bit,
    un: [Bit; 10],
    mant: [[Bit; 10]; 10],
}

impl Eg {
    fn new() -> Self {
        Eg { z: Bit::new(), s: Bit::new(), un: [Bit::new(); 10], mant: [[Bit::new(); 10]; 10] }
    }
}

/// All adaptive contexts. Bit models persist across the whole file;
/// `Prev` semantic state resets at every run start. Cloned for the
/// container's mode-2 shared-model chain (encode tries a carried copy).
#[derive(Clone)]
pub(crate) struct M {
    pub(crate) hdr: [[Bit; 2]; 32],
    crc_flag: Bit,
    mdb_flag: Bit,
    mdb_tree: Vec<Bit>,       // 2^9
    private: Vec<Bit>,        // prev(32) * 32 nodes
    scfsi: Vec<Bit>,          // prev(16) * 16 nodes
    eligible: [Bit; 2],
    parsed: [Bit; 8],         // slot(4) * prev(2)
    p23: Vec<Bit>,            // 2^12
    gain: Eg,
    bval: Eg,
    quads: Eg,
    fields: Vec<Vec<Bit>>,    // per field id: prev_dim * (1<<bits) nodes
    sf: Vec<Bit>,             // (slot 40 * len 5 * prev 16) * 16 nodes
    mag: Vec<Bit>,            // (bucket 19 * cl 15 * class 11 * ch 2 * step 15)
    esc_un: Vec<Bit>,         // (cl 15 * Lm1 17 * pos 18)
    esc_top: Vec<Bit>,        // (bl 18 * pos 2 * cl3 5)
    esc_low: Vec<Bit>,        // (bl 18 * pos 15)
    sign: Vec<Bit>,           // (s_prev 3 * s_ch0 3 * ms 2)
    gap: Bit,
    // 2b mixer inputs (only used when payload has MIX_FLAG)
    mixer: bool,
    lim: u8,
    mag1: Vec<Bit>,           // (bucket 19 * q1 15 * class 11 * step 15)
    mag2: Vec<Bit>,           // (bucket 19 * q2 15 * ch 2 * step 15)
    mag3: Vec<Bit>,           // (step 15 * q3 15 * class 11 * ms 2)
    mixw: Vec<[i32; 4]>,      // (min(step,3) 4 * class 11) weight sets
}

/// Side-info field bit-widths, in coding order.
const FIELD_BITS: [u32; 15] = [4, 1, 2, 1, 5, 5, 5, 3, 3, 3, 4, 3, 1, 1, 1];
const F_SFC: usize = 0;
const F_WS: usize = 1;
const F_BT: usize = 2;
const F_MIX: usize = 3;
const F_TAB0: usize = 4;
const F_SUB0: usize = 7;
const F_R0: usize = 10;
const F_R1: usize = 11;
const F_PRE: usize = 12;
const F_SFS: usize = 13;
const F_C1T: usize = 14;

pub(crate) fn new_m(mixer: bool, lim: u8) -> M {
    M {
        hdr: [[Bit::new(); 2]; 32],
        crc_flag: Bit::new(),
        mdb_flag: Bit::new(),
        mdb_tree: vec![Bit::new(); 1 << 9],
        private: vec![Bit::new(); 32 * 32],
        scfsi: vec![Bit::new(); 16 * 16],
        eligible: [Bit::new(); 2],
        parsed: [Bit::new(); 8],
        p23: vec![Bit::new(); 1 << 12],
        gain: Eg::new(),
        bval: Eg::new(),
        quads: Eg::new(),
        fields: FIELD_BITS
            .iter()
            .map(|&b| vec![Bit::new(); (1usize << b) * (1usize << b)])
            .collect(),
        sf: vec![Bit::new(); 40 * 5 * 16 * 16],
        mag: vec![Bit::new(); 19 * 15 * 11 * 2 * 15],
        esc_un: vec![Bit::new(); 15 * 17 * 18],
        esc_top: vec![Bit::new(); 18 * 2 * 5],
        esc_low: vec![Bit::new(); 18 * 15],
        sign: vec![Bit::new(); 3 * 3 * 2],
        gap: Bit::new(),
        mixer,
        lim,
        mag1: vec![Bit::new(); 19 * 15 * 11 * 15],
        mag2: vec![Bit::new(); 19 * 15 * 2 * 15],
        mag3: vec![Bit::new(); 15 * 15 * 11 * 2],
        mixw: vec![[16384; 4]; 4 * 11],
    }
}

fn field_ctx(m: &mut M, id: usize, prev: u32) -> &mut [Bit] {
    let bits = FIELD_BITS[id] as usize;
    let dim = 1usize << bits;
    let off = (prev as usize & (dim - 1)) * dim;
    &mut m.fields[id][off..off + dim]
}

// ---------------------------------------------------------------- helpers

/// MPEG CRC-16 (poly 0x8005, init 0xFFFF, MSB-first) over header bytes 2..4
/// and the serialized side info.
pub(crate) fn mp3_crc16(hdr: u32, side: &[u8]) -> u16 {
    let mut crc = 0xFFFFu16;
    for &b in [(hdr >> 8) as u8, hdr as u8].iter().chain(side.iter()) {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x8005 } else { crc << 1 };
        }
    }
    crc
}

fn side_bytes(s: &SideInfo, nch: usize) -> Vec<u8> {
    let mut bw = BitWriter::new();
    write_side(&mut bw, s, nch);
    bw.into_bytes()
}

/// table index -> (class 0..10, vmax). class 0 = uncoded (all zero);
/// 7/8 = linbits groups A/B; 9/10 = count1 A/B.
fn class_of(t: usize) -> (usize, i64) {
    match t {
        0 | 4 | 14 => (0, 0),
        1 => (1, 1),
        2 | 3 => (2, 2),
        5 | 6 => (3, 3),
        7..=9 => (4, 5),
        10..=12 => (5, 7),
        13 | 15 => (6, 15),
        16..=23 => (7, 15),
        24..=31 => (8, 15),
        32 => (9, 1),
        _ => (10, 1),
    }
}

const CLEV: [i64; 15] = [0, 1, 2, 3, 4, 6, 9, 14, 20, 30, 45, 70, 110, 180, i64::MAX];

fn clevel(e: i64) -> usize {
    CLEV.iter().position(|&b| e <= b).unwrap_or(14)
}

fn bucket(i: usize) -> usize {
    let v = i + 1;
    let b = 63 - (v as u64).leading_zeros() as usize;
    let second = if b >= 1 { usize::from((v >> (b - 1)) & 1 == 1) } else { 0 };
    (2 * b + second).min(18)
}

fn mag_idx(bk: usize, cl: usize, class: usize, ch: usize, step: usize) -> usize {
    (((bk * 15 + cl) * 11 + class) * 2 + ch) * 15 + step
}

fn eg_put(e: &mut Enc, m: &mut Eg, d: i64, cat: usize) {
    e.bit(u32::from(d == 0), &mut m.z, cat);
    if d == 0 {
        return;
    }
    e.bit(u32::from(d < 0), &mut m.s, cat);
    let v = d.unsigned_abs();
    let bl = 64 - v.leading_zeros();
    let ex = (bl - 1) as usize; // 0..=9 for our deltas
    for k in 0..ex.min(9) {
        e.bit(1, &mut m.un[k], cat);
    }
    if ex < 10 {
        e.bit(0, &mut m.un[ex.min(9)], cat);
    }
    for j in (0..ex.min(10)).rev() {
        let b = ((v >> j) & 1) as u32;
        e.bit(b, &mut m.mant[ex.min(9)][j.min(9)], cat);
    }
}

fn eg_get(d: &mut Dec, m: &mut Eg) -> i64 {
    if d.bit(&mut m.z) != 0 {
        return 0;
    }
    let neg = d.bit(&mut m.s) != 0;
    let mut ex = 0usize;
    while ex < 10 && d.bit(&mut m.un[ex]) != 0 {
        ex += 1;
    }
    if ex >= 10 {
        return 0; // out of modeled range
    }
    let mut v = 1u64 << ex;
    for j in (0..ex).rev() {
        v |= u64::from(d.bit(&mut m.mant[ex][j])) << j;
    }
    if neg { -(v as i64) } else { v as i64 }
}

// ---------------------------------------------------------------- per-run state

struct Prev {
    hdr: u32,
    private: u32,
    scfsi: [u32; 2],
    fields: [[u32; 15]; 2],
    gain: [i64; 2],
    bval: [i64; 2],
    quads: [i64; 2],
    parsed: [u32; 4],
    eligible: u32,
    parsed_end: i64, // bits
    eff: [[u8; 40]; 2],
    ix: [[i16; 576]; 2],
    ch0_ix: [i16; 576],
    ch0_parsed: bool,
    ch0_quads: i64,
}

impl Prev {
    fn new() -> Self {
        Prev {
            hdr: 0,
            private: 0,
            scfsi: [0; 2],
            fields: [[0; 15]; 2],
            gain: [0; 2],
            bval: [0; 2],
            quads: [0; 2],
            parsed: [0; 4],
            eligible: 0,
            parsed_end: 0,
            eff: [[0; 40]; 2],
            ix: [[0; 576]; 2],
            ch0_ix: [0; 576],
            ch0_parsed: false,
            ch0_quads: 0,
        }
    }
}

static ZERO_IX: [i16; 576] = [0; 576];

fn ceil8(x: i64) -> i64 {
    (x + 7) / 8
}

fn put_field(e: &mut Enc, m: &mut M, pv: &mut Prev, ch: usize, id: usize, v: u32) {
    let prev = pv.fields[ch][id];
    let ctx = field_ctx(m, id, prev);
    e.tree(v, FIELD_BITS[id], ctx, cat::SIDE);
    pv.fields[ch][id] = v;
}

fn get_field(d: &mut Dec, m: &mut M, pv: &mut Prev, ch: usize, id: usize) -> u32 {
    let prev = pv.fields[ch][id];
    let ctx = field_ctx(m, id, prev);
    let v = d.tree(FIELD_BITS[id], ctx);
    pv.fields[ch][id] = v;
    v
}

// ---------------------------------------------------------------- side info

fn put_grch_side(e: &mut Enc, m: &mut M, pv: &mut Prev, c: usize, cs: &ChSide, nch: usize) {
    let refc = if c == 1 && nch == 2 { 0 } else { c };
    eg_put(e, &mut m.bval, i64::from(cs.big_values) - pv.bval[refc], cat::SIDE);
    eg_put(e, &mut m.gain, i64::from(cs.global_gain) - pv.gain[refc], cat::SIDE);
    pv.bval[c] = i64::from(cs.big_values);
    pv.gain[c] = i64::from(cs.global_gain);
    put_field(e, m, pv, c, F_SFC, u32::from(cs.scalefac_compress));
    let ws = u32::from(cs.window_switching);
    put_field(e, m, pv, c, F_WS, ws);
    match cs.block {
        Block::Switched { block_type, mixed } => {
            put_field(e, m, pv, c, F_BT, u32::from(block_type));
            put_field(e, m, pv, c, F_MIX, u32::from(mixed));
            put_field(e, m, pv, c, F_TAB0, u32::from(cs.tabs[0]));
            put_field(e, m, pv, c, F_TAB0 + 1, u32::from(cs.tabs[1]));
            for (i, &v) in cs.subblock.iter().enumerate() {
                put_field(e, m, pv, c, F_SUB0 + i, u32::from(v));
            }
        }
        Block::Long { region0, region1 } => {
            for (i, &v) in cs.tabs.iter().enumerate() {
                put_field(e, m, pv, c, F_TAB0 + i, u32::from(v));
            }
            put_field(e, m, pv, c, F_R0, u32::from(region0));
            put_field(e, m, pv, c, F_R1, u32::from(region1));
        }
    }
    put_field(e, m, pv, c, F_PRE, u32::from(cs.preflag));
    put_field(e, m, pv, c, F_SFS, u32::from(cs.scalefac_scale));
    put_field(e, m, pv, c, F_C1T, u32::from(cs.count1));
}

fn get_grch_side(d: &mut Dec, m: &mut M, pv: &mut Prev, c: usize, nch: usize) -> ChSide {
    let refc = if c == 1 && nch == 2 { 0 } else { c };
    let bv = pv.bval[refc] + eg_get(d, &mut m.bval);
    let gg = pv.gain[refc] + eg_get(d, &mut m.gain);
    let mut cs = ChSide {
        big_values: bv as u16,
        global_gain: gg as u8,
        ..Default::default()
    };
    pv.bval[c] = bv;
    pv.gain[c] = gg;
    cs.scalefac_compress = get_field(d, m, pv, c, F_SFC) as u8;
    cs.window_switching = get_field(d, m, pv, c, F_WS) != 0;
    if cs.window_switching {
        let bt = get_field(d, m, pv, c, F_BT) as u8;
        let mixed = get_field(d, m, pv, c, F_MIX) != 0;
        cs.tabs[0] = get_field(d, m, pv, c, F_TAB0) as u8;
        cs.tabs[1] = get_field(d, m, pv, c, F_TAB0 + 1) as u8;
        for i in 0..3 {
            cs.subblock[i] = get_field(d, m, pv, c, F_SUB0 + i) as u8;
        }
        cs.block = Block::Switched { block_type: bt, mixed };
    } else {
        for (i, t) in cs.tabs.iter_mut().enumerate() {
            *t = get_field(d, m, pv, c, F_TAB0 + i) as u8;
        }
        let region0 = get_field(d, m, pv, c, F_R0) as u8;
        let region1 = get_field(d, m, pv, c, F_R1) as u8;
        cs.block = Block::Long { region0, region1 };
    }
    cs.preflag = get_field(d, m, pv, c, F_PRE) != 0;
    cs.scalefac_scale = get_field(d, m, pv, c, F_SFS) != 0;
    cs.count1 = get_field(d, m, pv, c, F_C1T) != 0;
    cs
}

// ---------------------------------------------------------------- scalefacs

#[allow(clippy::too_many_arguments)]
fn put_sf(
    e: &mut Enc,
    m: &mut M,
    pv: &mut Prev,
    cs: &ChSide,
    scfsi: &[bool; 4],
    g: usize,
    c: usize,
    d: &ParsedGrCh,
) {
    let layout = scalefac_layout(cs, scfsi, g);
    let mut slot = 0usize;
    for &(len, count) in &layout {
        for _ in 0..count {
            if len > 0 {
                let v = u32::from(*d.scalefacs.get(slot).unwrap_or(&0));
                let s = slot.min(39);
                let base =
                    ((s * 5 + usize::from(len)) * 16 + usize::from(pv.eff[c][s])) * 16;
                e.tree(v, u32::from(len), &mut m.sf[base..base + 16], cat::SF);
                pv.eff[c][s] = v as u8;
            }
            slot += 1;
        }
    }
}

fn get_sf(
    d: &mut Dec,
    m: &mut M,
    pv: &mut Prev,
    cs: &ChSide,
    scfsi: &[bool; 4],
    g: usize,
    c: usize,
) -> Vec<u8> {
    let layout = scalefac_layout(cs, scfsi, g);
    let mut slot = 0usize;
    let mut out = Vec::new();
    for &(len, count) in &layout {
        for _ in 0..count {
            if len > 0 {
                let s = slot.min(39);
                let base =
                    ((s * 5 + usize::from(len)) * 16 + usize::from(pv.eff[c][s])) * 16;
                let v = d.tree(u32::from(len), &mut m.sf[base..base + 16]) as u8;
                pv.eff[c][s] = v;
                out.push(v);
            } else {
                out.push(0);
            }
            slot += 1;
        }
    }
    out
}

// ---------------------------------------------------------------- coefficients

/// Quantized coefficient context features shared by 2a and the mixer.
struct CFeat {
    bk: usize,
    cl: usize,
    q1: usize, // clevel(2*n1+n2)
    q2: usize, // clevel(2*p0+pm+pp)
    q3: usize, // ch1: clevel(2*o0); ch0: clevel(n1)
}

fn coef_ctx(
    ix: &[i16; 576],
    prev: &[i16; 576],
    ch0: &[i16; 576],
    i: usize,
    ch: usize,
) -> CFeat {
    let n1 = i64::from(if i >= 1 { ix[i - 1].abs() } else { 0 }).min(255);
    let n2 = i64::from(if i >= 2 { ix[i - 2].abs() } else { 0 }).min(255);
    let p0 = i64::from(prev[i].abs()).min(255);
    let pm = i64::from(if i >= 1 { prev[i - 1].abs() } else { 0 }).min(255);
    let pp = i64::from(if i + 1 < 576 { prev[i + 1].abs() } else { 0 }).min(255);
    let o0 = i64::from(if ch == 1 { ch0[i].abs() } else { 0 }).min(255);
    CFeat {
        bk: bucket(i),
        cl: clevel(2 * n1 + n2 + 2 * p0 + pm + pp + 2 * o0),
        q1: clevel(2 * n1 + n2),
        q2: clevel(2 * p0 + pm + pp),
        q3: if ch == 1 { clevel(2 * o0) } else { clevel(n1) },
    }
}

// ---------------------------------------------------------- 2b logistic mix
// Integer-only lpaq1-style squash/stretch (deterministic across platforms).
// squash(d) maps 256-per-nat logit d in [-2047, 2047] to a 12-bit probability.

const SQUASH_TAB: [u16; 33] = [
    1, 2, 3, 6, 10, 16, 27, 45, 73, 120, 194, 310, 488, 747, 1101, 1546, 2047,
    2549, 2994, 3348, 3607, 3785, 3901, 3975, 4024, 4050, 4068, 4079, 4085, 4089,
    4092, 4093, 4094,
];

pub(crate) fn squash(d: i32) -> u32 {
    if d > 2047 {
        return 4095;
    }
    if d < -2047 {
        return 0;
    }
    let w = (d & 127) as u32;
    let i = ((d + 2048) >> 7) as usize;
    (u32::from(SQUASH_TAB[i]) * (128 - w) + u32::from(SQUASH_TAB[i + 1]) * w + 64) >> 7
}

/// stretch(p12) = inverse of squash, built by the integer inversion loop.
fn stretch_tab() -> &'static [i16; 4096] {
    static T: OnceLock<[i16; 4096]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0i16; 4096];
        let mut pi = 0usize;
        for x in -2047i32..=2047 {
            let i = squash(x) as usize;
            for v in t.iter_mut().take(i + 1).skip(pi) {
                *v = x as i16;
            }
            pi = i + 1;
        }
        t[4095] = 2047;
        t
    })
}

pub(crate) fn stretch(p12: usize) -> i32 {
    i32::from(stretch_tab()[p12])
}

/// Mixer input indices (2a ctx, m1, m2, m3) + weight set for (step, class).
fn mag_mix_idx(bk: usize, f: &CFeat, class: usize, ch: usize, ms: bool, step: usize) -> [usize; 5] {
    let i0 = mag_idx(bk, f.cl, class, ch, step);
    let i1 = ((bk * 15 + f.q1) * 11 + class) * 15 + step;
    let i2 = ((bk * 15 + f.q2) * 2 + ch) * 15 + step;
    let i3 = ((step * 15 + f.q3) * 11 + class) * 2 + usize::from(ms);
    [i0, i1, i2, i3, step.min(3) * 11 + class]
}

/// Mixed p16 from the four input models via logistic weights
/// (min(step,3), class). Returns (p16, stretched inputs) for `mix_learn`.
fn mix_pred(m: &M, i0: usize, i1: usize, i2: usize, i3: usize, wi: usize) -> (u32, [i32; 4]) {
    let s = [
        stretch((m.mag[i0].prob() >> 4) as usize),
        stretch((m.mag1[i1].prob() >> 4) as usize),
        stretch((m.mag2[i2].prob() >> 4) as usize),
        stretch((m.mag3[i3].prob() >> 4) as usize),
    ];
    let mut sum = 0i64;
    for (k, &sk) in s.iter().enumerate() {
        sum += i64::from(m.mixw[wi][k]) * i64::from(sk);
    }
    (
        squash((sum >> 16).clamp(-2047, 2047) as i32) << 4,
        s,
    )
}

fn mix_learn(m: &mut M, bit: u32, pm: u32, s: &[i32; 4], idx: [usize; 5]) {
    let err = i64::from(bit << 16) - i64::from(pm);
    {
        let w = &mut m.mixw[idx[4]];
        for (k, &sk) in s.iter().enumerate() {
            w[k] = (i64::from(w[k]) + ((i64::from(sk) * err) >> 18))
                .clamp(-(1 << 23), 1 << 23) as i32;
        }
    }
    let lim = m.lim;
    m.mag[idx[0]].update(bit, lim);
    m.mag1[idx[1]].update(bit, lim);
    m.mag2[idx[2]].update(bit, lim);
    m.mag3[idx[3]].update(bit, lim);
}

#[allow(clippy::too_many_arguments)]
fn put_mag(
    e: &mut Enc,
    m: &mut M,
    magv: i64,
    class: usize,
    vmax: i64,
    lin: u32,
    f: &CFeat,
    ch: usize,
    ms: bool,
) {
    let (bk, cl) = (f.bk, f.cl);
    if m.mixer {
        let mut step = |b: u32, step: usize, cat_: usize| {
            let idx = mag_mix_idx(bk, f, class, ch, ms, step);
            let (pm, s) = mix_pred(m, idx[0], idx[1], idx[2], idx[3], idx[4]);
            e.bit_p(b, pm, cat_);
            mix_learn(m, b, pm, &s, idx);
        };
        step(u32::from(magv == 0), 0, cat::MAG0);
        if magv == 0 {
            return;
        }
        let top = vmax.min(15) as usize;
        for k in 1..top {
            let stop = magv == k as i64;
            step(u32::from(stop), k, cat::MAGU);
            if stop {
                return;
            }
        }
    } else {
        e.bit(u32::from(magv == 0), &mut m.mag[mag_idx(bk, cl, class, ch, 0)], cat::MAG0);
        if magv == 0 {
            return;
        }
        let top = vmax.min(15) as usize;
        for k in 1..top {
            let stop = magv == k as i64;
            e.bit(u32::from(stop), &mut m.mag[mag_idx(bk, cl, class, ch, k)], cat::MAGU);
            if stop {
                return;
            }
        }
    }
    if magv < 15 || class < 7 {
        return;
    }
    // linbits escape: r = m - 15; bl = bit_len(r) in 0..=lin, truncated unary
    let r = (magv - 15) as u64;
    let bl = if r == 0 { 0 } else { 64 - r.leading_zeros() } as usize;
    let lin_u = lin as usize;
    let l_idx = (lin_u.saturating_sub(1)).min(16);
    let mut k = 0usize;
    while k < bl && k < lin_u {
        e.bit(0, &mut m.esc_un[(cl * 17 + l_idx) * 18 + k], cat::MAGE);
        k += 1;
    }
    if bl < lin_u {
        e.bit(1, &mut m.esc_un[(cl * 17 + l_idx) * 18 + bl], cat::MAGE);
    }
    for j in (0..bl.saturating_sub(1)).rev() {
        let bit = ((r >> j) & 1) as u32;
        let from_top = bl - 2 - j;
        if from_top < 2 {
            e.bit(bit, &mut m.esc_top[(bl * 2 + from_top) * 5 + cl / 3], cat::MAGE);
        } else {
            e.bit(bit, &mut m.esc_low[bl * 15 + (from_top - 2).min(14)], cat::MAGE);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn get_mag(
    d: &mut Dec,
    m: &mut M,
    class: usize,
    vmax: i64,
    lin: u32,
    f: &CFeat,
    ch: usize,
    ms: bool,
) -> i64 {
    let (bk, cl) = (f.bk, f.cl);
    let top = vmax.min(15) as usize;
    let mut step = |m_: &mut M, step: usize| -> u32 {
        if m_.mixer {
            let idx = mag_mix_idx(bk, f, class, ch, ms, step);
            let (pm, s) = mix_pred(m_, idx[0], idx[1], idx[2], idx[3], idx[4]);
            let b = d.bit_p(pm);
            mix_learn(m_, b, pm, &s, idx);
            b
        } else {
            d.bit(&mut m_.mag[mag_idx(bk, cl, class, ch, step)])
        }
    };
    if step(m, 0) != 0 {
        return 0;
    }
    let mut magv = top as i64;
    for k in 1..top {
        if step(m, k) != 0 {
            magv = k as i64;
            break;
        }
    }
    if magv < 15 || class < 7 {
        return magv;
    }
    let lin_u = lin as usize;
    let l_idx = lin_u.saturating_sub(1).min(16);
    let mut bl = 0usize;
    while bl < lin_u && bl < 17 {
        if d.bit(&mut m.esc_un[(cl * 17 + l_idx) * 18 + bl]) != 0 {
            break;
        }
        bl += 1;
    }
    let mut r = 0u64;
    if bl > 0 {
        r = 1u64 << (bl - 1);
        for j in (0..bl - 1).rev() {
            let from_top = bl - 2 - j;
            let b = if from_top < 2 {
                d.bit(&mut m.esc_top[(bl * 2 + from_top) * 5 + cl / 3])
            } else {
                d.bit(&mut m.esc_low[bl * 15 + (from_top - 2).min(14)])
            };
            r |= u64::from(b) << j;
        }
    }
    15 + r as i64
}

fn sign_ctx(s_prev: i64, s_ch0: i64, ms: bool) -> usize {
    let sp = (s_prev.signum() + 1) as usize;
    let sc = (s_ch0.signum() + 1) as usize;
    (sp * 3 + sc) * 2 + usize::from(ms)
}

#[allow(clippy::too_many_arguments)]
fn put_coefs(
    e: &mut Enc,
    m: &mut M,
    pv: &Prev,
    cs: &ChSide,
    d: &ParsedGrCh,
    c: usize,
    rate_idx: usize,
    ms: bool,
    ch0: &[i16; 576],
) {
    let bv_end = usize::from(cs.big_values) * 2;
    let (r1, r2) = super::frame::region_bounds(cs, rate_idx).unwrap_or((36, 576));
    let quad_class = if cs.count1 { 10 } else { 9 };
    let lim = (bv_end + usize::from(d.count1_quads) * 4).min(576);
    for i in 0..lim {
        let (class, vmax, lin) = if i < bv_end {
            let p = i & !1;
            let t = usize::from(if p < r1 { cs.tabs[0] } else if p < r2 { cs.tabs[1] } else { cs.tabs[2] });
            let (cl, vm) = class_of(t);
            (cl, vm, super::tables::HUFFMAN_TABLES[t].linbits as u32)
        } else {
            (quad_class, 1, 0)
        };
        if class == 0 {
            continue;
        }
        let f = coef_ctx(&d.ix, &pv.ix[c], ch0, i, c);
        let magv = i64::from(d.ix[i]).abs();
        put_mag(e, m, magv, class, vmax, lin, &f, c, ms);
        if magv != 0 {
            let sp = i64::from(if i >= 1 { d.ix[i - 1] } else { 0 });
            let sc = i64::from(ch0[i]);
            e.bit(u32::from(d.ix[i] < 0), &mut m.sign[sign_ctx(sp, sc, ms)], cat::SIGN);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn get_coefs(
    d: &mut Dec,
    m: &mut M,
    pv: &Prev,
    cs: &ChSide,
    quads: u16,
    c: usize,
    rate_idx: usize,
    ms: bool,
    ch0: &[i16; 576],
    ix: &mut [i16; 576],
) {
    let bv_end = usize::from(cs.big_values) * 2;
    let (r1, r2) = super::frame::region_bounds(cs, rate_idx).unwrap_or((36, 576));
    let quad_class = if cs.count1 { 10 } else { 9 };
    let lim = (bv_end + usize::from(quads) * 4).min(576);
    for i in 0..lim {
        let (class, vmax, lin) = if i < bv_end {
            let p = i & !1;
            let t = usize::from(if p < r1 { cs.tabs[0] } else if p < r2 { cs.tabs[1] } else { cs.tabs[2] });
            let (cl, vm) = class_of(t);
            (cl, vm, super::tables::HUFFMAN_TABLES[t].linbits as u32)
        } else {
            (quad_class, 1, 0)
        };
        if class == 0 {
            ix[i] = 0;
            continue;
        }
        let f = coef_ctx(ix, &pv.ix[c], ch0, i, c);
        let magv = get_mag(d, m, class, vmax, lin, &f, c, ms);
        let mut x = magv;
        if magv != 0 {
            let sp = i64::from(if i >= 1 { ix[i - 1] } else { 0 });
            let sc = i64::from(ch0[i]);
            if d.bit(&mut m.sign[sign_ctx(sp, sc, ms)]) != 0 {
                x = -x;
            }
        }
        ix[i] = x.clamp(-32768, 32767) as i16;
    }
}

// ---------------------------------------------------------------- run codec

/// GrCh chunks sit in (frame, granule, ch) order inside `run.chunks`; the
/// caller queries them in the same order, so a shared cursor keeps each
/// lookup amortized O(1) instead of rescanning the chunk list per granule.
fn parsed_of<'a>(run: &'a Run, cursor: &mut usize, fi: usize, g: usize, c: usize) -> Option<&'a ParsedGrCh> {
    while let Some(ch) = run.chunks.get(*cursor) {
        match ch {
            Chunk::GrCh { frame, granule, ch: cc, .. }
                if (*frame, *granule, *cc) >= (fi, g, c) =>
            {
                break;
            }
            _ => *cursor += 1,
        }
    }
    match run.chunks.get(*cursor) {
        Some(Chunk::GrCh { frame, granule, ch: cc, data })
            if (*frame, *granule, *cc) == (fi, g, c) =>
        {
            Some(&**data)
        }
        _ => None,
    }
}

fn ms_flag(hdr: u32) -> bool {
    (hdr >> 6) & 3 == 1 && (hdr >> 5) & 1 != 0 // joint stereo + MS bit
}

fn put_run(e: &mut Enc, m: &mut M, run: &Run) {
    e.direct(run.frames.len() as u64, 32, cat::MISC);
    let mut pv = Prev::new();
    let mut off = 0i64; // byte offset in M
    let mut ci = 0usize; // GrCh chunk cursor (queries are in chunk order)
    for (fi, fr) in run.frames.iter().enumerate() {
        let h = &fr.hdr_fields;
        for i in 0..32u32 {
            let b = (fr.hdr >> (31 - i)) & 1;
            let prevb = (pv.hdr >> (31 - i)) & 1;
            e.bit(b, &mut m.hdr[i as usize][prevb as usize], cat::HDR);
        }
        pv.hdr = fr.hdr;
        if h.has_crc {
            let sb = side_bytes(&fr.side, h.nch);
            let ok = fr.crc == Some(mp3_crc16(fr.hdr, &sb));
            e.bit(u32::from(ok), &mut m.crc_flag, cat::HDR);
            if !ok {
                e.direct(u64::from(fr.crc.unwrap_or(0)), 16, cat::HDR);
            }
        }
        let pred = (off - ceil8(pv.parsed_end)).clamp(0, 511);
        e.bit(u32::from(i64::from(fr.side.mdb) == pred), &mut m.mdb_flag, cat::SIDE);
        if i64::from(fr.side.mdb) != pred {
            e.tree(u32::from(fr.side.mdb), 9, &mut m.mdb_tree, cat::SIDE);
        }
        let pb = u32::from(fr.side.private);
        let plen = if h.nch == 1 { 5 } else { 3 };
        let base = (pv.private.min(31) as usize) * 32;
        e.tree(pb, plen, &mut m.private[base..base + 32], cat::SIDE);
        pv.private = pb;
        for (c, row) in fr.side.scfsi.iter().take(h.nch).enumerate() {
            let v = row.iter().fold(0u32, |a, &b| (a << 1) | u32::from(b));
            let base = (pv.scfsi[c].min(15) as usize) * 16;
            e.tree(v, 4, &mut m.scfsi[base..base + 16], cat::SIDE);
            pv.scfsi[c] = v;
        }
        let elig = u32::from(fr.parsed);
        e.bit(elig, &mut m.eligible[pv.eligible as usize], cat::SIDE);
        pv.eligible = elig;
        for g in 0..2 {
            pv.ch0_parsed = false;
            for c in 0..h.nch {
                let cs = &fr.side.gr[g][c];
                let d = parsed_of(run, &mut ci, fi, g, c);
                if fr.parsed {
                    let slot = (g * 2 + c) * 2 + pv.parsed[g * 2 + c] as usize;
                    e.bit(u32::from(d.is_some()), &mut m.parsed[slot], cat::SIDE);
                    pv.parsed[g * 2 + c] = u32::from(d.is_some());
                }
                if d.is_none() {
                    e.tree(u32::from(cs.part2_3), 12, &mut m.p23, cat::SIDE);
                }
                put_grch_side(e, m, &mut pv, c, cs, h.nch);
                if let Some(d) = d {
                    let refq = if c == 1 && h.nch == 2 && pv.ch0_parsed {
                        pv.ch0_quads
                    } else {
                        pv.quads[c]
                    };
                    eg_put(e, &mut m.quads, i64::from(d.count1_quads) - refq, cat::SIDE);
                    pv.quads[c] = i64::from(d.count1_quads);
                    if c == 0 {
                        pv.ch0_quads = i64::from(d.count1_quads);
                        pv.ch0_parsed = true;
                        pv.ch0_ix = d.ix;
                    }
                    put_sf(e, m, &mut pv, cs, &fr.side.scfsi[c], g, c, d);
                    let ch0: &[i16; 576] = if c == 1 && pv.ch0_parsed { &pv.ch0_ix } else { &ZERO_IX };
                    put_coefs(e, m, &pv, cs, d, c, h.rate_idx, ms_flag(fr.hdr), ch0);
                    pv.ix[c] = d.ix;
                }
            }
        }
        if fr.parsed {
            let total: i64 = (0..2)
                .map(|g| {
                    (0..h.nch)
                        .map(|c| i64::from(fr.side.gr[g][c].part2_3))
                        .sum::<i64>()
                })
                .sum();
            pv.parsed_end = 8 * (off - i64::from(fr.side.mdb)) + total;
        }
        off += fr.slot as i64;
    }
    // gap bits of the whole run, in chunk order
    for ch in &run.chunks {
        if let Chunk::Gap(gp) = ch {
            let mut r = crate::bits::BitReader::new(&gp.bytes);
            let mut remaining = gp.nbits;
            while remaining > 0 {
                let take = remaining.min(32) as u32;
                let v = r.read(take);
                for k in 0..take {
                    e.bit(((v >> (take - 1 - k)) & 1) as u32, &mut m.gap, cat::GAP);
                }
                remaining -= u64::from(take);
            }
        }
    }
}

struct DecGrCh {
    start: i64,
    end: i64,
    fi: usize,
    g: usize,
    c: usize,
    data: ParsedGrCh,
}

fn get_run(d: &mut Dec, m: &mut M, expected_len: u64) -> Result<Run, String> {
    let nframes = d.direct(32) as usize;
    if nframes as u64 > expected_len / 4 + 1 {
        return Err("frame count exceeds input".into());
    }
    let mut pv = Prev::new();
    let mut off = 0i64;
    let mut frames: Vec<Frame> = Vec::new();
    let mut regions: Vec<DecGrCh> = Vec::new();
    let mut m_bytes = 0u64;
    for fi in 0..nframes {
        if d.overrun {
            return Err("truncated mp3 payload".into());
        }
        let mut hdr = 0u32;
        for i in 0..32u32 {
            let prevb = (pv.hdr >> (31 - i)) & 1;
            hdr = (hdr << 1) | d.bit(&mut m.hdr[i as usize][prevb as usize]);
        }
        pv.hdr = hdr;
        let h = header(&hdr.to_be_bytes(), 0)
            .ok_or_else(|| format!("invalid frame header {hdr:08x} at frame {fi}"))?;
        let slot = h.len - 4 - usize::from(h.has_crc) * 2 - h.side_len;
        m_bytes += slot as u64;
        if m_bytes + 4 * (fi as u64 + 1) > expected_len {
            return Err("frame data exceeds input".into());
        }
        let mut crc: Option<u16> = None;
        let mut crc_computed = false;
        if h.has_crc {
            crc_computed = d.bit(&mut m.crc_flag) != 0;
            if !crc_computed {
                crc = Some(d.direct(16) as u16);
            }
        }
        let pred = (off - ceil8(pv.parsed_end)).clamp(0, 511);
        let mdb = if d.bit(&mut m.mdb_flag) != 0 {
            pred as u16
        } else {
            d.tree(9, &mut m.mdb_tree) as u16
        };
        let plen = if h.nch == 1 { 5 } else { 3 };
        let private = {
            let base = (pv.private.min(31) as usize) * 32;
            d.tree(plen, &mut m.private[base..base + 32])
        };
        pv.private = private;
        let mut scfsi = [[false; 4]; 2];
        for (c, row) in scfsi.iter_mut().take(h.nch).enumerate() {
            let base = (pv.scfsi[c].min(15) as usize) * 16;
            let v = d.tree(4, &mut m.scfsi[base..base + 16]);
            pv.scfsi[c] = v;
            for (i, b) in row.iter_mut().enumerate() {
                *b = (v >> (3 - i)) & 1 != 0;
            }
        }
        let elig = d.bit(&mut m.eligible[pv.eligible as usize]);
        pv.eligible = elig;
        let mut side = SideInfo {
            mdb,
            private: private as u16,
            scfsi,
            gr: [
                [ChSide::default(), ChSide::default()],
                [ChSide::default(), ChSide::default()],
            ],
        };
        let mut parsed_count = 0u8;
        let data_start = 8 * (off - i64::from(mdb));
        let mut cur = data_start;
        let mut total = 0i64;
        for g in 0..2 {
            pv.ch0_parsed = false;
            for c in 0..h.nch {
                let parsed_flag = if elig != 0 {
                    let slot = (g * 2 + c) * 2 + pv.parsed[g * 2 + c] as usize;
                    let b = d.bit(&mut m.parsed[slot]);
                    pv.parsed[g * 2 + c] = b;
                    b != 0
                } else {
                    false
                };
                let mut p23 = 0u32;
                if !parsed_flag {
                    p23 = d.tree(12, &mut m.p23);
                }
                let mut cs = get_grch_side(d, m, &mut pv, c, h.nch);
                if parsed_flag {
                    // parsed granules must satisfy is_len <= 576; bound the
                    // decoded field so corrupt streams cannot index past 576
                    cs.big_values = cs.big_values.min(288);
                    let refq = if c == 1 && h.nch == 2 && pv.ch0_parsed {
                        pv.ch0_quads
                    } else {
                        pv.quads[c]
                    };
                    let quads = (refq + eg_get(d, &mut m.quads)).clamp(0, 576);
                    pv.quads[c] = quads;
                    if c == 0 {
                        pv.ch0_quads = quads;
                        pv.ch0_parsed = true;
                    }
                    let scalefacs = get_sf(d, m, &mut pv, &cs, &side.scfsi[c], g, c);
                    let mut ix = [0i16; 576];
                    let ch0: &[i16; 576] =
                        if c == 1 && pv.ch0_parsed { &pv.ch0_ix } else { &ZERO_IX };
                    get_coefs(d, m, &pv, &cs, quads as u16, c, h.rate_idx, ms_flag(hdr), ch0, &mut ix);
                    let data = ParsedGrCh {
                        scalefacs,
                        ix,
                        count1_quads: quads as u16,
                        sf_bits: 0,
                        code_bits: 0,
                        lin_bits: 0,
                        sign_bits: 0,
                        quad_code_bits: 0,
                    };
                    let mut bw = BitWriter::new();
                    encode_grch(&mut bw, &cs, &side.scfsi[c], g, h.rate_idx, &data);
                    if bw.bits > 4095 {
                        return Err("derived part2_3 too large".into());
                    }
                    p23 = bw.bits as u32;
                    parsed_count += 1;
                    regions.push(DecGrCh { start: cur, end: cur + i64::from(p23), fi, g, c, data });
                    pv.ix[c] = ix;
                    if c == 0 {
                        pv.ch0_ix = ix;
                    }
                }
                cs.part2_3 = p23 as u16;
                side.gr[g][c] = cs;
                cur += i64::from(p23);
                total += i64::from(p23);
            }
        }
        if crc_computed {
            crc = Some(mp3_crc16(hdr, &side_bytes(&side, h.nch)));
        }
        if elig != 0 {
            pv.parsed_end = data_start + total;
        }
        frames.push(Frame {
            hdr,
            crc,
            side,
            slot,
            hdr_fields: h,
            data_start,
            parsed: elig != 0,
            parsed_grch: parsed_count,
        });
        off += slot as i64;
    }
    // validate parsed-region layout before consuming gap bits
    let m_bits = 8 * m_bytes as i64;
    let mut cursor = 0i64;
    for r in &regions {
        if r.start < cursor || r.end > m_bits {
            return Err("granule region out of bounds".into());
        }
        cursor = r.end;
    }
    // assemble chunks, decoding gap bits inline
    let mut chunks = Vec::new();
    let mut cursor = 0i64;
    for r in regions {
        if d.overrun {
            return Err("truncated mp3 payload".into());
        }
        if r.start > cursor {
            chunks.push(Chunk::Gap(gap_decode(d, m, (r.start - cursor) as u64)?));
        }
        chunks.push(Chunk::GrCh {
            frame: r.fi,
            granule: r.g,
            ch: r.c,
            data: Box::new(r.data),
        });
        cursor = r.end;
    }
    if cursor < m_bits {
        chunks.push(Chunk::Gap(gap_decode(d, m, (m_bits - cursor) as u64)?));
    }
    Ok(Run { frames, chunks, m_bytes: m_bytes as usize })
}

fn gap_decode(d: &mut Dec, m: &mut M, n: u64) -> Result<PackedBits, String> {
    if n > 1 << 30 {
        return Err("gap too large".into());
    }
    if d.overrun {
        return Err("truncated mp3 payload".into());
    }
    let mut bw = BitWriter::new();
    for _ in 0..n {
        bw.write(u64::from(d.bit(&mut m.gap)), 1);
    }
    Ok(PackedBits { nbits: n, bytes: bw.into_bytes() })
}

// ---------------------------------------------------------------- top level

/// Returns (payload, per-category ideal coded bits).
/// Used by tests/sweeps; the container encodes via `compress_into`.
#[allow(dead_code)]
pub fn compress(f: &Mp3File) -> (Vec<u8>, [f64; NCAT]) {
    compress_with_limit(f, LIMIT)
}

#[allow(dead_code)]
pub(crate) fn compress_with_limit(f: &Mp3File, limit: u8) -> (Vec<u8>, [f64; NCAT]) {
    compress_opts(f, limit, USE_MIXER)
}

/// `mixer` selects the 2b logistic mixer (only for 2a-vs-2b comparison).
#[allow(dead_code)]
pub(crate) fn compress_opts(f: &Mp3File, limit: u8, mixer: bool) -> (Vec<u8>, [f64; NCAT]) {
    let mut m = new_m(mixer, limit);
    compress_into(f, &mut m)
}

/// Compress into an existing model `m` — fresh, or carried from earlier
/// tracks (container mode 2). The flag byte records m's (limit, mixer);
/// on return `m` holds the post-state a chained decoder reproduces.
pub(crate) fn compress_into(f: &Mp3File, m: &mut M) -> (Vec<u8>, [f64; NCAT]) {
    let mut e = Enc::new(m.lim);
    e.direct(f.items.len() as u64, 32, cat::MISC);
    e.direct(f.invalid_frames, 32, cat::MISC);
    for item in &f.items {
        match item {
            Item::Raw(b) => {
                e.direct(0, 1, cat::RAW);
                e.direct(b.len() as u64, 32, cat::RAW);
                for &byte in b {
                    e.direct(u64::from(byte), 8, cat::RAW);
                }
            }
            Item::Run(run) => {
                e.direct(1, 1, cat::MISC);
                put_run(&mut e, m, run);
            }
        }
    }
    let mut costs = [0.0f64; NCAT];
    costs.copy_from_slice(&e.costs[..NCAT]);
    let mut out = Vec::with_capacity(costs.iter().sum::<f64>() as usize / 8 + 8);
    out.push(m.lim | if m.mixer { MIX_FLAG } else { 0 });
    out.extend(e.finish());
    (out, costs)
}

/// Initial model for the container's mode-2 chain (standard parameters).
pub(crate) fn carried() -> M {
    new_m(USE_MIXER, LIMIT)
}

/// Fresh model for a standalone (mode-1) payload; parameters read from its
/// flag byte. Decode the payload with `decompress_with` to obtain both the
/// file and the post-state the chain carry should advance to.
pub(crate) fn fresh_for(data: &[u8]) -> Result<M, String> {
    let (&flag, _) = data.split_first().ok_or("empty mp3 payload")?;
    Ok(new_m(flag & MIX_FLAG != 0, flag & !MIX_FLAG))
}

pub fn decompress(data: &[u8], expected_len: u64) -> Result<Mp3File, String> {
    let mut m = fresh_for(data)?;
    decompress_with(data, expected_len, &mut m)
}

/// Decompress into an existing model `m` (fresh for mode 1, the running
/// carry for chained mode-2 payloads). The payload flag's (limit, mixer)
/// must match m's parameters — a chain is only valid between payloads
/// built with the same ones.
pub(crate) fn decompress_with(
    data: &[u8],
    expected_len: u64,
    m: &mut M,
) -> Result<Mp3File, String> {
    let (&flag, rest) = data.split_first().ok_or("empty mp3 payload")?;
    let limit = flag & !MIX_FLAG;
    if limit != m.lim || (flag & MIX_FLAG != 0) != m.mixer {
        return Err("mp3 payload flag does not match the carried model".into());
    }
    let mut d = Dec::new(rest, limit);
    let nitems = d.direct(32) as usize;
    // every item costs >= 1 coded bit, so the payload size bounds the count
    if nitems as u64 > expected_len.saturating_add(1) || nitems as u64 > rest.len() as u64 * 8 {
        return Err("item count exceeds input".into());
    }
    let invalid = d.direct(32);
    let mut items = Vec::with_capacity(nitems.min(4096));
    for _ in 0..nitems {
        if d.overrun {
            return Err("truncated mp3 payload".into());
        }
        match d.direct(1) {
            0 => {
                let len = d.direct(32) as usize;
                // each raw byte costs ~8 coded bits: can't exceed the payload
                if len as u64 > expected_len || len as u64 > rest.len() as u64 {
                    return Err("raw item too large".into());
                }
                let mut b = Vec::with_capacity(len);
                for _ in 0..len {
                    b.push(d.direct(8) as u8);
                }
                items.push(Item::Raw(b));
            }
            _ => {
                items.push(Item::Run(get_run(&mut d, m, expected_len)?));
            }
        }
    }
    if d.overrun {
        return Err("truncated mp3 payload".into());
    }
    Ok(Mp3File { items, invalid_frames: invalid })
}




