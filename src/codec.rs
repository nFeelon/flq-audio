//! Per-track block coding: float split, stereo decorrelation, LPC + Rice.

use crate::bits::{
    BitReader, BitWriter, ResidualPlan, bit_len, plan_residual, read_residual, write_residual,
};
use crate::joint::{RefChoice, choose_ref, ref_pred, seg_index};

pub const BLOCK: usize = 4096;
pub const MAX_ORDER: usize = 32;
pub const ORDERS: [usize; 9] = [0, 1, 2, 4, 8, 12, 16, 24, 32];
pub const LPC_PREC: u32 = 15;
pub const Y_LIMIT: i64 = 1 << 40;

// ---------------------------------------------------------------- float

/// Per-sample tail data for float: low mantissa bits, or an escape.
#[derive(Clone, Debug)]
pub enum DBit {
    Bits(u64, u32), // low `s` mantissa bits
    Zero,           // +0.0: single 0 bit
    Word(u64),      // escape: 1 bit + raw stream word (32/64 bits)
}

pub struct FloatData {
    pub e: i16,
    pub streams: Vec<Vec<i64>>, // significand-coded stream per channel
    pub d: Vec<Vec<DBit>>,
}

fn float_params(mw: u8) -> (u32, i64, u64, u64) {
    let expw: u32 = if mw == 52 { 11 } else { 8 };
    let bias: i64 = if mw == 52 { 1023 } else { 127 };
    (expw, bias, (1u64 << expw) - 1, (1u64 << mw) - 1)
}

/// Split float words into the LPC-coded stream `I` and the raw tail bits.
pub fn split_float(words: &[Vec<u64>], mw: u8) -> FloatData {
    let (expw, bias, emask, mmask) = float_params(mw);
    let mut e_max = i64::MIN;
    for ch in words {
        for &w in ch {
            let ef = ((w >> mw) & emask) as i64;
            if ef != 0 && ef != emask as i64 {
                e_max = e_max.max(ef - bias);
            }
        }
    }
    let e_max = if e_max == i64::MIN { 0 } else { e_max };
    let mut streams = Vec::with_capacity(words.len());
    let mut dd = Vec::with_capacity(words.len());
    for ch in words {
        let mut si = Vec::with_capacity(ch.len());
        let mut sd = Vec::with_capacity(ch.len());
        for &w in ch {
            let ef = ((w >> mw) & emask) as i64;
            let s = (e_max - (ef - bias)) + (i64::from(mw) - 23);
            if ef != 0 && ef != emask as i64 && s <= i64::from(mw) {
                let m = (w & mmask) | (1u64 << mw);
                let mag = (m >> s) as i64;
                si.push(if (w >> (u32::from(mw) + expw)) & 1 == 1 { -mag } else { mag });
                sd.push(DBit::Bits(m & ((1u64 << s) - 1), s as u32));
            } else {
                si.push(0);
                sd.push(if w == 0 { DBit::Zero } else { DBit::Word(w) });
            }
        }
        streams.push(si);
        dd.push(sd);
    }
    FloatData { e: e_max as i16, streams, d: dd }
}

/// Rebuild a float stream word from |I|, its sign, and the `s` low mantissa bits.
pub fn recombine_float(i: i64, dbits: u64, mw: u8, e_max: i16) -> Result<u64, String> {
    let (expw, bias, emask, mmask) = float_params(mw);
    let bl = bit_len(i.unsigned_abs());
    if bl > u32::from(mw) + 1 {
        return Err("mantissa magnitude out of range".into());
    }
    let s = i64::from(mw) - (i64::from(bl) - 1);
    let e = i64::from(e_max) - (s - (i64::from(mw) - 23));
    let ef = e + bias;
    if ef <= 0 || ef >= emask as i64 {
        return Err("float exponent out of range".into());
    }
    let m = ((i.unsigned_abs() << s) | dbits) & mmask;
    Ok(((u64::from(i < 0)) << (u32::from(mw) + expw)) | ((ef as u64) << mw) | m)
}

// ---------------------------------------------------------------- LPC

fn tukey(n: usize) -> Vec<f64> {
    if n < 3 {
        return vec![1.0; n];
    }
    let t = 0.5 * (n as f64 - 1.0) / 2.0;
    if t <= 0.0 {
        return vec![1.0; n];
    }
    let last = (n - 1) as f64;
    (0..n)
        .map(|i| {
            let i = i as f64;
            if i < t {
                0.5 * (1.0 - (std::f64::consts::PI * i / t).cos())
            } else if i > last - t {
                0.5 * (1.0 - (std::f64::consts::PI * (last - i) / t).cos())
            } else {
                1.0
            }
        })
        .collect()
}

fn autocorr(sig: &[i64], win: &[f64]) -> [f64; MAX_ORDER + 1] {
    let n = sig.len();
    let xw: Vec<f64> = sig.iter().zip(win).map(|(&x, &w)| x as f64 * w).collect();
    let mut r = [0f64; MAX_ORDER + 1];
    for (lag, slot) in r.iter_mut().enumerate() {
        let mut s = 0.0;
        for i in lag..n {
            s += xw[i] * xw[i - lag];
        }
        *slot = s;
    }
    r
}

/// Levinson–Durbin; `sol[p]` = coefficients for order p (empty when unavailable).
fn levinson(r: &[f64; MAX_ORDER + 1]) -> Vec<Vec<f64>> {
    let mut sol: Vec<Vec<f64>> = vec![Vec::new(); MAX_ORDER + 1];
    let mut a = vec![0f64; MAX_ORDER + 1];
    let mut err = r[0];
    for p in 1..=MAX_ORDER {
        let mut acc = r[p];
        for k in 1..p {
            acc -= a[k] * r[p - k];
        }
        let kp = acc / err;
        if !kp.is_finite() {
            break;
        }
        let mut na = a.clone();
        na[p] = kp;
        for k in 1..p {
            na[k] = a[k] - kp * a[p - k];
        }
        a = na;
        err *= 1.0 - kp * kp;
        if err <= 0.0 || !err.is_finite() {
            break;
        }
        sol[p] = a[1..=p].to_vec();
    }
    sol
}

fn quantize(a: &[f64]) -> Option<(u8, Vec<i16>)> {
    let cmax = a.iter().fold(0.0f64, |m, &v| m.max(v.abs()));
    if !cmax.is_finite() || cmax == 0.0 {
        return None;
    }
    let ex = cmax.log2().floor() as i32 + 1;
    let qshift = (14 - ex).clamp(0, 31) as u8;
    let scale = 2f64.powi(i32::from(qshift));
    let q: Vec<i16> = a
        .iter()
        .map(|&v| (v * scale).round().clamp(-16384.0, 16383.0) as i16)
        .collect();
    Some((qshift, q))
}

pub struct SignalPlan {
    pub order: u8,
    pub qshift: u8,
    pub q: Vec<i16>,
    pub resid: Vec<i64>,
    pub rplan: ResidualPlan,
}

impl SignalPlan {
    pub fn cost(&self) -> u64 {
        self.rplan.cost
            + 6
            + if self.order > 0 {
                4 + 5 + u64::from(self.order) * u64::from(LPC_PREC)
            } else {
                0
            }
    }
}

fn predict_at(w: &[i64], i: usize, q: &[i16], qshift: u8) -> i64 {
    let mut s = 0i64;
    for (k, &c) in q.iter().enumerate() {
        s = s.wrapping_add(i64::from(c) * w[i - k - 1]);
    }
    s >> qshift
}

/// Best LPC coding for one coded signal: `w` = window [s-32, e), last `n` are the block.
fn plan_signal(w: &[i64], n: usize) -> SignalPlan {
    let blk = &w[MAX_ORDER..];
    let mut best = {
        let r0: Vec<i64> = blk.to_vec();
        let rp = plan_residual(&r0);
        SignalPlan { order: 0, qshift: 0, q: Vec::new(), resid: r0, rplan: rp }
    };
    let win = tukey(n);
    let r = autocorr(blk, &win);
    if r[0] == 0.0 {
        return best;
    }
    let sol = levinson(&r);
    for &o in ORDERS.iter().skip(1) {
        let a = &sol[o];
        if a.is_empty() {
            continue;
        }
        let Some((qshift, q)) = quantize(a) else { continue };
        let resid: Vec<i64> = (0..n)
            .map(|i| w[MAX_ORDER + i].wrapping_sub(predict_at(w, MAX_ORDER + i, &q, qshift)))
            .collect();
        let rp = plan_residual(&resid);
        let cand = SignalPlan { order: o as u8, qshift, q, resid, rplan: rp };
        if cand.cost() < best.cost() {
            best = cand;
        }
    }
    best
}

// ---------------------------------------------------------------- helpers

fn window(x: &[i64], s: usize, e: usize) -> Vec<i64> {
    let mut v = Vec::with_capacity(MAX_ORDER + (e - s));
    let base = s as i64 - MAX_ORDER as i64;
    for i in 0..MAX_ORDER as i64 {
        let gp = base + i;
        v.push(if gp >= 0 { x[gp as usize] } else { 0 });
    }
    v.extend_from_slice(&x[s..e]);
    v
}

fn sub(a: &[i64], b: &[i64]) -> Vec<i64> {
    a.iter().zip(b).map(|(&x, &y)| x.wrapping_sub(y)).collect()
}

fn mid(a: &[i64], b: &[i64]) -> Vec<i64> {
    a.iter().zip(b).map(|(&x, &y)| x.wrapping_add(y) >> 1).collect()
}

/// Coded signal pair for a stereo mode over any window: 0 LR, 1 LS, 2 RS, 3 MS.
fn stereo_forward(mode: u8, y0: &[i64], y1: &[i64]) -> (Vec<i64>, Vec<i64>) {
    match mode {
        1 => (y0.to_vec(), sub(y0, y1)),
        2 => (y1.to_vec(), sub(y0, y1)),
        3 => (mid(y0, y1), sub(y0, y1)),
        _ => (y0.to_vec(), y1.to_vec()),
    }
}

/// Inverse of `stereo_forward` for one sample.
fn stereo_inverse(mode: u8, a: i64, b: i64) -> (i64, i64) {
    match mode {
        1 => (a, a.wrapping_sub(b)),
        2 => (b.wrapping_add(a), a),
        3 => {
            let mid = a.wrapping_mul(2) | (b & 1);
            (mid.wrapping_add(b) >> 1, mid.wrapping_sub(b) >> 1)
        }
        _ => (a, b),
    }
}

fn mode_of(mode: u8, l: usize, r: usize, m: usize, s: usize) -> (usize, usize) {
    match mode {
        1 => (l, s),
        2 => (r, s),
        3 => (m, s),
        _ => (l, r),
    }
}

fn d_cost(d: Option<&[Vec<DBit>]>, word_bits: u32, s: usize, e: usize) -> u64 {
    d.map(|d| {
        d.iter()
            .flat_map(|ch| ch[s..e].iter())
            .map(|b| match b {
                DBit::Bits(_, n) => u64::from(*n),
                DBit::Zero => 1,
                DBit::Word(_) => 1 + u64::from(word_bits),
            })
            .sum()
    })
    .unwrap_or(0)
}

// ---------------------------------------------------------------- planning

/// A reference source in the bitstream pool: the track's own earlier
/// material, or an earlier track (index into the meta refs list / the
/// `cross` slice on decode).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pool {
    SelfRef,
    Cross(u8),
}

/// One candidate reference source for planning (self or an earlier track).
pub struct RefSource<'a> {
    pub self_ref: bool,
    /// Cross-track source index (meta order); ignored for self sources.
    pub track: u8,
    pub x: &'a [Vec<i64>],
    pub peaks: &'a [Vec<i32>],
    pub refine: i32,
}

/// Bits for the per-ref source index: ceil_log2(pool_len) (0 for pools of
/// size 0 or 1).
pub fn src_bits(pool_len: usize) -> u32 {
    usize::BITS - pool_len.max(1).saturating_sub(1).leading_zeros()
}

pub struct TrackCtx<'a> {
    pub x: &'a [Vec<i64>],            // code streams (int samples, or I for float)
    pub d: Option<&'a [Vec<DBit>]>,   // float tail data
    pub float_word_bits: u32,         // stream word width for escapes
    pub sources: &'a [RefSource<'a>], // reference pool (empty = no ref bits)
}

pub struct AltBlock {
    pub refs: Vec<Option<RefChoice>>,
    pub mode: u8,
    pub signals: Vec<SignalPlan>,
}

pub struct BlockPlan {
    pub mode: u8,
    pub signals: Vec<SignalPlan>, // variant A
    pub alt: Option<AltBlock>,    // variant B, when strictly cheaper
}

pub struct TrackPlan {
    pub blocks: Vec<BlockPlan>,
    pub independent: u64,
    pub stereo: u64,
    pub joint_bits: u64,
    pub float_bits: u64,
    pub joint_on: bool,
    /// Blocks where each pool source supplied >= 1 channel reference.
    pub source_blocks: Vec<u64>,
}

pub fn plan_track(ctx: &TrackCtx<'_>) -> TrackPlan {
    let ch = ctx.x.len();
    let frames = ctx.x.first().map_or(0, |x| x.len());
    let stereo = ch == 2;
    let nb = frames.div_ceil(BLOCK);
    let nsrc = ctx.sources.len();
    let analysis = nsrc > 0;
    let sbits = u64::from(src_bits(nsrc));
    let mut prev_lag: Vec<Option<(usize, i32)>> = vec![None; ch];
    let mut plans = Vec::with_capacity(nb);
    let (mut ind_t, mut st_t, mut jt_t, mut d_t) = (0u64, 0u64, 0u64, 0u64);
    let mut source_blocks = vec![0u64; nsrc];

    for b in 0..nb {
        let s = b * BLOCK;
        let e = (s + BLOCK).min(frames);
        let n = e - s;
        let w: Vec<Vec<i64>> = ctx.x.iter().map(|x| window(x, s, e)).collect();
        let d_b = d_cost(ctx.d, ctx.float_word_bits, s, e);
        d_t += d_b;

        // variant A ----------------------------------------------------
        let raw: Vec<SignalPlan> = w.iter().map(|wc| plan_signal(wc, n)).collect();
        let ind_b: u64 = raw.iter().map(|p| p.cost()).sum::<u64>() + d_b;
        let (st_b, mode_a, sigs_a) = if stereo {
            let sig_s = plan_signal(&sub(&w[0], &w[1]), n);
            let sig_m = plan_signal(&mid(&w[0], &w[1]), n);
            let (c0, c1, cs, cm) = (raw[0].cost(), raw[1].cost(), sig_s.cost(), sig_m.cost());
            let modes = [c0 + c1, c0 + cs, c1 + cs, cm + cs];
            let m = modes
                .iter()
                .enumerate()
                .min_by_key(|&(_, &c)| c)
                .map(|(i, _)| i)
                .unwrap();
            let mut it = raw.into_iter();
            let (l, r) = (it.next().unwrap(), it.next().unwrap());
            let sigs = match mode_of(m as u8, 0, 1, 2, 3) {
                (0, 1) => vec![l, r],
                (0, 3) => vec![l, sig_s],
                (1, 3) => vec![r, sig_s],
                _ => vec![sig_m, sig_s],
            };
            (2 + modes[m] + d_b, m as u8, sigs)
        } else {
            (ind_b, 0u8, raw)
        };
        ind_t += ind_b;
        st_t += st_b;

        // variant B ----------------------------------------------------
        let mut alt: Option<AltBlock> = None;
        let mut b_cost: Option<u64> = None;
        if analysis {
            let mut refs: Vec<Option<RefChoice>> = Vec::with_capacity(ch);
            let mut lags: Vec<Option<(usize, i32)>> = vec![None; ch];
            let mut any = false;
            for c in 0..ch {
                let mut best: Option<(RefChoice, f64)> = None;
                for (si, src) in ctx.sources.iter().enumerate() {
                    let mut cands: Vec<i32> = src
                        .peaks
                        .get(seg_index(s, src.peaks.len().max(1)))
                        .cloned()
                        .unwrap_or_default();
                    if let Some((psi, l)) = prev_lag[c]
                        && psi == si
                    {
                        cands.push(l);
                    }
                    // self refs may only use already-decoded material: every
                    // predicted position gp+lag, gp in [s-MAX_ORDER, e), must
                    // be < s, i.e. lag <= -(e - s)
                    let lag_ok =
                        |lag: i32| !src.self_ref || i64::from(lag) <= s as i64 - e as i64;
                    if let Some((mut g, score)) = choose_ref(
                        &ctx.x[c][s..e],
                        src.x,
                        s,
                        &cands,
                        src.refine,
                        lag_ok,
                    ) {
                        g.src = si as u8;
                        if best.is_none_or(|(_, bs)| score > bs) {
                            best = Some((g, score));
                        }
                    }
                }
                lags[c] = best.map(|(g, _)| (g.src as usize, g.lag));
                if best.is_some() {
                    any = true;
                }
                refs.push(best.map(|(g, _)| g));
            }
            if any {
                // y = x - P over the whole window
                let mut y = w.clone();
                let mut overflow = false;
                for c in 0..ch {
                    if let Some(g) = &refs[c] {
                        let rc = &ctx.sources[g.src as usize].x[g.ref_ch as usize];
                        for (i, yv) in y[c].iter_mut().enumerate() {
                            let gp = s as i64 - MAX_ORDER as i64 + i as i64;
                            *yv = yv.wrapping_sub(ref_pred(g, rc, gp + i64::from(g.lag)));
                            if yv.saturating_abs() > Y_LIMIT {
                                overflow = true;
                            }
                        }
                    }
                }
                if !overflow {
                    let nrefs = refs.iter().flatten().count() as u64;
                    let raw_b: Vec<SignalPlan> = y.iter().map(|yc| plan_signal(yc, n)).collect();
                    let (cost, mode_b, sigs_b) = if stereo {
                        let ss = plan_signal(&sub(&y[0], &y[1]), n);
                        let sm = plan_signal(&mid(&y[0], &y[1]), n);
                        let (c0, c1, cs, cm) =
                            (raw_b[0].cost(), raw_b[1].cost(), ss.cost(), sm.cost());
                        let modes = [c0 + c1, c0 + cs, c1 + cs, cm + cs];
                        let m = modes
                            .iter()
                            .enumerate()
                            .min_by_key(|&(_, &c)| c)
                            .map(|(i, _)| i)
                            .unwrap();
                        let mut it = raw_b.into_iter();
                        let (l, r) = (it.next().unwrap(), it.next().unwrap());
                        let sigs = match mode_of(m as u8, 0, 1, 2, 3) {
                            (0, 1) => vec![l, r],
                            (0, 3) => vec![l, ss],
                            (1, 3) => vec![r, ss],
                            _ => vec![sm, ss],
                        };
                        (2 + modes[m] + (66 + sbits) * nrefs + d_b, m as u8, sigs)
                    } else {
                        (
                            raw_b.iter().map(|p| p.cost()).sum::<u64>()
                                + (66 + sbits) * nrefs
                                + d_b,
                            0,
                            raw_b,
                        )
                    };
                    b_cost = Some(cost);
                    if cost < st_b {
                        alt = Some(AltBlock { refs: refs.clone(), mode: mode_b, signals: sigs_b });
                        // count the block once per source used (any channel)
                        for (si, sb) in source_blocks.iter_mut().enumerate() {
                            if refs.iter().flatten().any(|g| g.src as usize == si) {
                                *sb += 1;
                            }
                        }
                    }
                }
            }
            for c in 0..ch {
                prev_lag[c] = if alt.is_some() { lags[c] } else { None };
            }
        }

        let jt_b = if analysis {
            ch as u64 + b_cost.map_or(st_b, |c| c.min(st_b))
        } else {
            st_b
        };
        jt_t += jt_b;
        plans.push(BlockPlan { mode: mode_a, signals: sigs_a, alt });
    }

    // joint coding adds (1 self_ref + 1 nref + nref) refspec bytes to the
    // track meta — count them against the joint total (nref before pruning:
    // a conservative superset of what will actually be written)
    let nref = ctx.sources.iter().filter(|s| !s.self_ref).count() as u64;
    let joint_on = analysis && jt_t + (2 + nref) * 8 < st_t;
    if !joint_on {
        source_blocks.iter_mut().for_each(|v| *v = 0);
    }
    TrackPlan {
        blocks: plans,
        independent: ind_t,
        stereo: st_t,
        joint_bits: jt_t,
        float_bits: d_t,
        joint_on,
        source_blocks,
    }
}

// ---------------------------------------------------------------- writing

pub fn write_track(bw: &mut BitWriter, ctx: &TrackCtx<'_>, plan: &TrackPlan, sbits: u32) {
    let ch = ctx.x.len();
    let frames = ctx.x.first().map_or(0, |x| x.len());
    let stereo = ch == 2;
    for (b, bp) in plan.blocks.iter().enumerate() {
        let s = b * BLOCK;
        let e = (s + BLOCK).min(frames);
        if plan.joint_on {
            for c in 0..ch {
                match bp.alt.as_ref().and_then(|a| a.refs[c].as_ref()) {
                    Some(g) => {
                        bw.write(1, 1);
                        bw.write(u64::from(g.src), sbits);
                        bw.write(u64::from(g.ref_ch), 4);
                        bw.write_signed(i64::from(g.lag), 32);
                        bw.write_signed(i64::from(g.gm), 24);
                        bw.write(u64::from(g.gshift), 6);
                    }
                    None => bw.write(0, 1),
                }
            }
        }
        let (mode, sigs) = match (&bp.alt, plan.joint_on) {
            (Some(a), true) => (a.mode, &a.signals),
            _ => (bp.mode, &bp.signals),
        };
        if stereo {
            bw.write(u64::from(mode), 2);
        }
        for sig in sigs {
            bw.write(u64::from(sig.order), 6);
            if sig.order > 0 {
                bw.write(u64::from(LPC_PREC) - 1, 4);
                bw.write(u64::from(sig.qshift), 5);
                for &q in &sig.q {
                    bw.write_signed(i64::from(q), LPC_PREC);
                }
            }
            write_residual(bw, &sig.resid, &sig.rplan);
        }
        if let Some(d) = ctx.d {
            for dc in d.iter() {
                for db in &dc[s..e] {
                    match db {
                        DBit::Bits(v, n) => bw.write(*v, *n),
                        DBit::Zero => bw.write(0, 1),
                        DBit::Word(wd) => {
                            bw.write(1, 1);
                            bw.write(*wd, ctx.float_word_bits);
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------- reading

pub struct FloatMeta {
    pub mw: u8,          // 23 or 52
    pub e: i16,
    pub stream_bits: u32, // 32 or 64
    pub pcm_bits: u16,    // 32 or 64 (mode 2: stream 32, pcm 64)
}

pub struct TrackMeta {
    pub channels: usize,
    pub frames: usize,
    /// Source pool (empty = no ref bits): SelfRef entries predict from this
    /// track's own earlier samples, Cross(j) from `cross[j]`.
    pub pool: Vec<Pool>,
    pub float: Option<FloatMeta>,
}

pub struct DecodedCore {
    pub x: Vec<Vec<i64>>,               // code streams (I for float)
    pub words: Option<Vec<Vec<u64>>>,   // PCM-domain float words
}

/// Source stream for a pool entry: `cross[j]` for Cross(j), the track's own
/// partially decoded `x` for SelfRef (positions are guaranteed < the current
/// block by the causality check below).
fn pool_slice<'a>(
    pool: &Pool,
    ch_ref: usize,
    x: &'a [Vec<i64>],
    cross: &'a [&'a [Vec<i64>]],
) -> &'a [i64] {
    match pool {
        Pool::SelfRef => &x[ch_ref],
        Pool::Cross(j) => &cross[*j as usize][ch_ref],
    }
}

pub fn read_track(
    r: &mut BitReader<'_>,
    meta: &TrackMeta,
    cross: &[&[Vec<i64>]],
) -> Result<DecodedCore, String> {
    let ch = meta.channels;
    let stereo = ch == 2;
    let nb = meta.frames.div_ceil(BLOCK);
    // bound the declared stream before allocating from it: hard cap on
    // channel-samples (~2^30, see AGENTS.md known limits), then feasibility —
    // each block of each coded signal costs at least ~16 payload bits
    // (6 order + >=9 residual + 1 unary stop), so the declared frame count
    // cannot exceed what the remaining bitstream can hold
    if meta.frames as u64 * ch as u64 > 1 << 30 {
        return Err("declared frames exceed decode cap".into());
    }
    let nsig = if stereo { 2 } else { ch };
    let min_bits = nb as u64 * nsig as u64 * 16;
    if min_bits > r.remaining_bits() {
        return Err("declared frames exceed payload".into());
    }
    let pool_len = meta.pool.len();
    let sbits = src_bits(pool_len);
    let mut x = vec![vec![0i64; meta.frames]; ch];
    let mut words = meta.float.as_ref().map(|_| vec![vec![0u64; meta.frames]; ch]);

    for b in 0..nb {
        let s = b * BLOCK;
        let e = (s + BLOCK).min(meta.frames);
        let n = e - s;

        // 1. reference flags
        let mut refs: Vec<Option<RefChoice>> = vec![None; ch];
        if pool_len > 0 {
            for rc in refs.iter_mut() {
                if r.read(1) == 1 {
                    let si = r.read(sbits) as usize;
                    if si >= pool_len {
                        return Err("ref source out of range".into());
                    }
                    let ref_ch = r.read(4) as usize;
                    let g = RefChoice {
                        src: si as u8,
                        ref_ch: ref_ch as u8,
                        lag: r.read_signed(32) as i32,
                        gm: r.read_signed(24) as i32,
                        gshift: r.read(6) as u8,
                    };
                    match meta.pool[si] {
                        Pool::SelfRef => {
                            if ref_ch >= ch {
                                return Err("self ref_ch out of range".into());
                            }
                            // causality: gp+lag must be < s for all gp < e
                            if i64::from(g.lag) > s as i64 - e as i64 {
                                return Err("self ref lag reaches undecoded samples".into());
                            }
                        }
                        Pool::Cross(j) => {
                            if j as usize >= cross.len() {
                                return Err("cross ref index out of range".into());
                            }
                            if ref_ch >= cross[j as usize].len() {
                                return Err("ref_ch out of range".into());
                            }
                        }
                    }
                    *rc = Some(g);
                }
            }
        }
        let mode = if stereo { r.read(2) as u8 } else { 0 };
        if r.overrun {
            return Err("truncated payload".into());
        }

        let p_term = |c: usize, gp: i64, x: &[Vec<i64>]| -> i64 {
            let Some(g) = &refs[c] else { return 0 };
            let src = pool_slice(&meta.pool[g.src as usize], g.ref_ch as usize, x, cross);
            ref_pred(g, src, gp + i64::from(g.lag))
        };

        // history of the coded signals = transform of y-history over W
        let mut y_hist: Vec<Vec<i64>> = vec![vec![0; MAX_ORDER]; ch];
        for c in 0..ch {
            for (i, yv) in y_hist[c].iter_mut().enumerate() {
                let gp = s as i64 - MAX_ORDER as i64 + i as i64;
                let xv = if gp >= 0 { x[c][gp as usize] } else { 0 };
                *yv = xv.wrapping_sub(p_term(c, gp, &x));
            }
        }
        let coded_hist: Vec<Vec<i64>> = if stereo {
            let (a, b) = stereo_forward(mode, &y_hist[0], &y_hist[1]);
            vec![a, b]
        } else {
            y_hist.clone()
        };

        // 2. coded signals
        let nsig = if stereo { 2 } else { ch };
        let mut coded: Vec<Vec<i64>> = Vec::with_capacity(nsig);
        for hist in coded_hist.iter().take(nsig) {
            let order = r.read(6) as usize;
            if order > MAX_ORDER {
                return Err("LPC order out of range".into());
            }
            let (qshift, q) = if order > 0 {
                let prec = r.read(4) + 1;
                if prec > 32 {
                    return Err("LPC precision out of range".into());
                }
                let qshift = r.read(5) as u8;
                let mut q = Vec::with_capacity(order);
                for _ in 0..order {
                    q.push(r.read_signed(prec as u32) as i16);
                }
                (qshift, q)
            } else {
                (0u8, Vec::new())
            };
            let resid = read_residual(r, n)?;
            let mut aw: Vec<i64> = hist.clone();
            for &e in &resid {
                let mut p = 0i64;
                let l = aw.len();
                for (k, &c) in q.iter().enumerate() {
                    p = p.wrapping_add(i64::from(c).wrapping_mul(aw[l - k - 1]));
                }
                aw.push(e.wrapping_add(p >> qshift));
            }
            coded.push(aw[MAX_ORDER..].to_vec());
        }

        // 3. inverse transform, then add references back
        if stereo {
            for i in 0..n {
                let (y0, y1) = stereo_inverse(mode, coded[0][i], coded[1][i]);
                let p0 = p_term(0, (s + i) as i64, &x);
                let p1 = p_term(1, (s + i) as i64, &x);
                x[0][s + i] = y0.wrapping_add(p0);
                x[1][s + i] = y1.wrapping_add(p1);
            }
        } else {
            for c in 0..ch {
                for i in 0..n {
                    let p = p_term(c, (s + i) as i64, &x);
                    x[c][s + i] = coded[c][i].wrapping_add(p);
                }
            }
        }

        // 4. float tail bits
        if let (Some(fm), Some(wds)) = (&meta.float, words.as_mut()) {
            for c in 0..ch {
                for i in s..e {
                    let iv = x[c][i];
                    let word = if iv == 0 {
                        if r.read(1) == 0 {
                            0
                        } else {
                            r.read(fm.stream_bits)
                        }
                    } else {
                        let s2 = i64::from(fm.mw) - (i64::from(bit_len(iv.unsigned_abs())) - 1);
                        if s2 < 0 {
                            return Err("mantissa magnitude out of range".into());
                        }
                        let db = r.read(s2 as u32);
                        recombine_float(iv, db, fm.mw, fm.e)?
                    };
                    wds[c][i] = if fm.stream_bits == 32 && fm.pcm_bits == 64 {
                        (f32::from_bits(word as u32) as f64).to_bits()
                    } else {
                        word
                    };
                }
            }
        }
        if r.overrun {
            return Err("truncated payload".into());
        }
    }
    Ok(DecodedCore { x, words })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::{BitReader as BR, BitWriter as BW, bit_len};

    fn float_roundtrip(words: &[u64], mw: u8, pcm_bits: u16) -> Vec<u64> {
        let fd = split_float(&[words.to_vec()], mw);
        // rebuild: simulate decode of D data (not the bitstream, just values)
        words
            .iter()
            .zip(fd.d[0].iter())
            .zip(fd.streams[0].iter())
            .map(|((&_w, d), &i)| {
                let word = match d {
                    DBit::Zero => 0,
                    DBit::Word(raw) => *raw,
                    DBit::Bits(v, s) => {
                        assert_ne!(i, 0);
                        let bl = bit_len(i.unsigned_abs());
                        assert_eq!(u32::from(mw) + 1 - bl, *s, "recomputed s must match");
                        recombine_float(i, *v, mw, fd.e).unwrap()
                    }
                };
                if mw == 23 && pcm_bits == 64 {
                    (f32::from_bits(word as u32) as f64).to_bits()
                } else {
                    word
                }
            })
            .collect()
    }

    #[test]
    fn float_edge_f32() {
        let vals: Vec<f32> = vec![
            0.0,
            -0.0,
            f32::from_bits(1),          // min denormal
            f32::from_bits(0x007F_FFFF), // max denormal
            f32::from_bits(0x0080_0000), // min normal
            -1.0,
            1.0,
            f32::from_bits(0x3EFF_FFFF), // 0.5 - ulp
            0.5,
            f32::from_bits(0x3F00_0001), // 0.5 + ulp
            f32::MAX,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::from_bits(0x7FC0_0001), // quiet NaN payload
            f32::from_bits(0x7F80_0001), // signalling NaN payload
            f32::from_bits(0x2E80_0000), // ~2^-30
        ];
        let words: Vec<u64> = vals.iter().map(|v| u64::from(v.to_bits())).collect();
        let back = float_roundtrip(&words, 23, 32);
        assert_eq!(words, back, "f32 edge words must be bit-exact");
    }

    #[test]
    fn float_edge_f64() {
        let vals: Vec<f64> = vec![
            0.0,
            -0.0,
            f64::from_bits(1),
            f64::from_bits(0x000F_FFFF_FFFF_FFFF),
            f64::from_bits(0x0010_0000_0000_0000),
            -1.0,
            1.0,
            f64::from_bits(0x3FDF_FFFF_FFFF_FFFF),
            0.5,
            f64::from_bits(0x3FE0_0000_0000_0001),
            f64::MAX,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::from_bits(0x7FF8_0000_0000_0001),
            f64::from_bits(0x7FF0_0000_0000_0001),
            0.1, // not f32-exact -> forces mode 3 path (mw=52)
            f64::from_bits(0x3C10_0000_0000_0000), // ~2^-30
        ];
        let words: Vec<u64> = vals.iter().map(|v| v.to_bits()).collect();
        let back = float_roundtrip(&words, 52, 64);
        assert_eq!(words, back, "f64 edge words must be bit-exact");
    }

    #[test]
    fn float_f64_as_f32() {
        // values exactly representable in f32
        let vals = [0.5f64, -2.0, 1.25, 1024.0, 0.0, -0.0];
        let all_exact = vals
            .iter()
            .all(|&v| ((v as f32) as f64).to_bits() == v.to_bits());
        assert!(all_exact);
        let w32: Vec<u64> = vals.iter().map(|&v| u64::from((v as f32).to_bits())).collect();
        let back = float_roundtrip(&w32, 23, 64);
        let orig: Vec<u64> = vals.iter().map(|v| v.to_bits()).collect();
        assert_eq!(back, orig);
    }

    #[test]
    fn stereo_modes_roundtrip() {
        // full-scale i32 stereo noise: S needs 33 bits
        let mut st = 0x12345u64;
        let mut lcg = move || {
            st = st.wrapping_mul(6364136223846793005).wrapping_add(1);
            (st >> 33) as u32
        };
        for &(l, r) in &[
            (i64::from(i32::MAX), i64::from(i32::MIN)),
            (i64::from(i32::MIN), i64::from(i32::MAX)),
            (5i64, 4),
        ] {
            for mode in 0..4u8 {
                let (a, b) = stereo_forward(mode, &[l], &[r]);
                let (l2, r2) = stereo_inverse(mode, a[0], b[0]);
                assert_eq!((l, r), (l2, r2), "mode {mode} ({l},{r})");
            }
        }
        // random sweep
        for _ in 0..10_000 {
            let l = (lcg() as i32) as i64 - (1i64 << 31);
            let r = (lcg() as i32) as i64 - (1i64 << 31);
            for mode in 0..4u8 {
                let (a, b) = stereo_forward(mode, &[l], &[r]);
                let (l2, r2) = stereo_inverse(mode, a[0], b[0]);
                assert_eq!((l, r), (l2, r2));
            }
        }
    }

    #[test]
    fn plan_write_read_block() {
        // pseudo-audio: sine-ish + noise in i24 range
        let n = 300usize;
        let x: Vec<i64> = (0..n)
            .map(|i| ((i as f64 * 0.05).sin() * 300_000.0 + ((i * 37) % 97) as f64 * 80.0) as i64)
            .collect();
        let d: Option<&[Vec<DBit>]> = None;
        let xs = vec![x.clone()];
        let ctx = TrackCtx {
            x: &xs,
            d,
            float_word_bits: 0,
            sources: &[],
        };
        let plan = plan_track(&ctx);
        let mut bw = BW::new();
        write_track(&mut bw, &ctx, &plan, 0);
        let bytes = bw.into_bytes();
        let mut r = BR::new(&bytes);
        let meta = TrackMeta { channels: 1, frames: n, pool: Vec::new(), float: None };
        let core = read_track(&mut r, &meta, &[]).unwrap();
        assert_eq!(core.x[0], x);
        assert!(!r.overrun);
    }
}
