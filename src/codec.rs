//! Per-track block coding: float split, stereo decorrelation, LPC + Rice.

use crate::bits::{
    BitReader, BitWriter, ResidualPlan, bit_len, plan_residual, read_residual, write_residual,
};

pub const BLOCK: usize = 4096;
pub const MAX_ORDER: usize = 63;
pub const ORDERS: [usize; 11] = [0, 1, 2, 4, 8, 12, 16, 24, 32, 48, 63];
pub const LPC_PREC: u32 = 15;

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
                si.push(if (w >> (u32::from(mw) + expw)) & 1 == 1 {
                    -mag
                } else {
                    mag
                });
                sd.push(DBit::Bits(m & ((1u64 << s) - 1), s as u32));
            } else {
                si.push(0);
                sd.push(if w == 0 { DBit::Zero } else { DBit::Word(w) });
            }
        }
        streams.push(si);
        dd.push(sd);
    }
    FloatData {
        e: e_max as i16,
        streams,
        d: dd,
    }
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
                0.5 * (1.0 - dcos(std::f64::consts::PI * i / t))
            } else if i > last - t {
                0.5 * (1.0 - dcos(std::f64::consts::PI * (last - i) / t))
            } else {
                1.0
            }
        })
        .collect()
}

// Bit-portable f64 math for encoder decisions: platform libm (UCRT vs
// glibc vs musl vs wasm) differs by ULPs, and encoder output must be
// byte-identical everywhere — conformance vectors are re-encoded on CI.
// IEEE basic ops are correctly rounded, so a fixed-order evaluation is
// bit-identical on every target. These are NOT a libm replacement —
// they cover only the domains used below.

/// sin(x) for x in [0, π/4]; Taylor, residual < (π/4)^17/17! ≈ 5e-17.
fn dsin(x: f64) -> f64 {
    let z = x * x;
    x * (1.0
        + z * (-1.0 / 6.0
            + z * (1.0 / 120.0
                + z * (-1.0 / 5040.0
                    + z * (1.0 / 362880.0
                        + z * (-1.0 / 39916800.0
                            + z * (1.0 / 6227020800.0 + z * (-1.0 / 1307674368000.0))))))))
}

/// cos(x) for x in [0, π]; residual < (π/4)^18/18! ≈ 2e-18.
fn dcos(x: f64) -> f64 {
    let x = x.clamp(0.0, std::f64::consts::PI);
    if x > std::f64::consts::FRAC_PI_2 {
        return -dcos(std::f64::consts::PI - x);
    }
    if x > std::f64::consts::FRAC_PI_4 {
        return dsin(std::f64::consts::FRAC_PI_2 - x);
    }
    let z = x * x;
    1.0 + z
        * (-0.5
            + z * (1.0 / 24.0
                + z * (-1.0 / 720.0
                    + z * (1.0 / 40320.0
                        + z * (-1.0 / 3628800.0
                            + z * (1.0 / 479001600.0
                                + z * (-1.0 / 87178291200.0 + z * (1.0 / 20922789888000.0))))))))
}

/// floor(log2(x)) for finite x > 0, via the exponent field.
fn floor_log2(x: f64) -> i32 {
    debug_assert!(x > 0.0 && x.is_finite());
    let mut v = x;
    let mut adj = 0i32;
    if v.to_bits() >> 52 == 0 {
        v *= 4503599627370496.0; // subnormal -> normal: x * 2^52
        adj = 52;
    }
    ((v.to_bits() >> 52) & 0x7ff) as i32 - 1023 - adj
}

/// log2(x) for finite x >= 1: e + log2(mantissa in [1,2)) via the
/// atanh series 2·Σ s^(2k+1)/(2k+1), s = (m-1)/(m+1) in [0, 1/3).
fn dlog2(x: f64) -> f64 {
    debug_assert!(x >= 1.0 && x.is_finite());
    let b = x.to_bits();
    let e = ((b >> 52) & 0x7ff) as i32 - 1023;
    let m = f64::from_bits((b & !(0x7ffu64 << 52)) | (1023u64 << 52));
    let s = (m - 1.0) / (m + 1.0);
    let z = s * s;
    let mut sum = 0.0;
    for k in (0..24u32).rev() {
        sum = sum * z + 1.0 / (2 * k + 1) as f64;
    }
    e as f64 + 2.0 * s * sum * std::f64::consts::LOG2_E
}

fn autocorr(sig: &[i64], win: &[f64]) -> [f64; MAX_ORDER + 1] {
    let n = sig.len();
    let xw: Vec<f64> = sig.iter().zip(win).map(|(&x, &w)| x as f64 * w).collect();
    let mut r = [0f64; MAX_ORDER + 1];
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx2") {
        // lane-per-lag keeps each dot product's summation order identical
        // to the scalar loop — the result is bit-exact
        unsafe { autocorr_avx2(&xw, &mut r) };
        return r;
    }
    for (lag, slot) in r.iter_mut().enumerate() {
        let mut s = 0.0;
        for i in lag..n {
            s += xw[i] * xw[i - lag];
        }
        *slot = s;
    }
    r
}

/// r[lag] = Σ_i xw[i]·xw[i−lag] for lag 0..MAX_ORDER.
/// AVX2: lane j of each group accumulates lag (hi−j), so the 4-term
/// product vector is loaded contiguously and every lane keeps the
/// scalar summation order (bit-identical to the scalar fallback).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn autocorr_avx2(xw: &[f64], r: &mut [f64; MAX_ORDER + 1]) {
    use std::arch::x86_64::*;
    let n = xw.len();
    for g in 0..(MAX_ORDER + 1) / 4 {
        let hi = 4 * g + 3; // largest lag in the group
        // prologue: terms i in [lag, min(hi, n)) the vector loop misses
        for lag in (4 * g)..=hi {
            let mut s = 0.0;
            for i in lag..hi.min(n) {
                s += xw[i] * xw[i - lag];
            }
            r[lag] = s;
        }
        if hi >= n {
            continue;
        }
        let mut acc = _mm256_setzero_pd();
        let mut tmp = [0f64; 4];
        unsafe {
            for i in hi..n {
                let m = _mm256_loadu_pd(xw.as_ptr().add(i - hi));
                acc = _mm256_add_pd(acc, _mm256_mul_pd(_mm256_set1_pd(xw[i]), m));
            }
            _mm256_storeu_pd(tmp.as_mut_ptr(), acc);
        }
        for (j, &t) in tmp.iter().enumerate() {
            r[hi - j] += t;
        }
    }
}

/// Levinson–Durbin; `sol[p]` = coefficients for order p (empty when
/// unavailable).
fn levinson(r: &[f64; MAX_ORDER + 1]) -> Vec<Vec<f64>> {
    let mut sol: Vec<Vec<f64>> = vec![Vec::new(); MAX_ORDER + 1];
    let mut a = vec![0f64; MAX_ORDER + 1];
    let mut na = vec![0f64; MAX_ORDER + 1];
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
        na.copy_from_slice(&a);
        na[p] = kp;
        for k in 1..p {
            na[k] = a[k] - kp * a[p - k];
        }
        std::mem::swap(&mut a, &mut na);
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
    let ex = floor_log2(cmax) + 1;
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
    /// Warm-up field presence: Some(wbits) when the signal sits in a
    /// stream's first block with order > 0 — Some(0) = field present but no
    /// warm-up values (signed width > 63). None = ordinary block.
    pub warm_wbits: Option<u8>,
    /// Verbatim warm-up values (len = `warm_wbits` count, i.e. `order`).
    pub warm: Vec<i64>,
    /// Long-term prediction applied to `resid` before Rice coding:
    /// Some((lag τ, quantized gain on the 1/128 grid)); the stored `resid`
    /// is then the post-LTP residual and `rplan` covers it.
    pub ltp: Option<(u16, i8)>,
    /// Optional adaptive pass applied last: Some(index into LMS_STEPS).
    /// The stored `resid` is then the post-LMS stream.
    pub lms: Option<u8>,
    pub resid: Vec<i64>,
    pub rplan: ResidualPlan,
}

impl SignalPlan {
    /// Exact bit count `write_track` emits for this signal.
    pub fn cost(&self) -> u64 {
        self.rplan.cost
            + 6
            + 1
            + if self.order > 0 {
                4 + 5 + u64::from(self.order) * u64::from(LPC_PREC)
            } else {
                0
            }
            + self
                .warm_wbits
                .map_or(0, |w| 6 + u64::from(self.order) * u64::from(w))
            + self.ltp.map_or(0, |_| 20)
            + 1
            + self.lms.map_or(0, |_| 2)
    }
}

fn predict_at(w: &[i64], i: usize, q: &[i16], qshift: u8) -> i64 {
    let mut s = 0i64;
    for (k, &c) in q.iter().enumerate() {
        s = s.wrapping_add(i64::from(c) * w[i - k - 1]);
    }
    s >> qshift
}

/// Minimal signed bit width holding every value (1 for empty/all-zero).
fn signed_bits(vals: &[i64]) -> u8 {
    vals.iter()
        .map(|&v| {
            if v >= 0 {
                64 - v.leading_zeros() + 1
            } else {
                64 - (!v).leading_zeros() + 1
            }
        })
        .max()
        .unwrap_or(1) as u8
}

/// Fully evaluate one LPC order given its quantized coefficients:
/// residual + residual plan. In a stream's first block (`warm` = the
/// order), the first `warm` positions are carried as explicit verbatim
/// warm-up fields (residual placeholder 0) rather than predictions —
/// unless their signed width exceeds the 6-bit `wbits` field's 63-bit
/// maximum, when `warm_wbits` = Some(0) marks "decode normally".
fn eval_order(w: &[i64], n: usize, o: usize, qshift: u8, q: Vec<i16>, warm: usize) -> SignalPlan {
    let warm = warm.min(n);
    let warm_vals: Vec<i64> = (0..warm).map(|i| w[MAX_ORDER + i]).collect();
    let wbits = signed_bits(&warm_vals);
    let (warm_wbits, warm_vals, nw) = if warm > 0 && wbits <= 63 {
        (Some(wbits), warm_vals.clone(), warm)
    } else if warm > 0 {
        (Some(0), Vec::new(), 0)
    } else {
        (None, Vec::new(), 0)
    };
    let resid: Vec<i64> = (0..n)
        .map(|i| {
            if i < nw {
                0
            } else {
                w[MAX_ORDER + i].wrapping_sub(predict_at(w, MAX_ORDER + i, &q, qshift))
            }
        })
        .collect();
    let rp = plan_residual(&resid);
    SignalPlan {
        order: o as u8,
        qshift,
        q,
        warm_wbits,
        warm: warm_vals,
        ltp: None,
        lms: None,
        resid,
        rplan: rp,
    }
}

/// Best LPC coding for one coded signal: `w` = window [s-32, e), last `n` are
/// the block. `preset`: 8 = evaluate every candidate order (exact); 5 =
/// rank candidates by a quantized-residual proxy (stride 2) and fully
/// evaluate only the top 2; 0 = same with stride 4 and only the top 1.
/// Order 0 is always evaluated. `warm` marks a stream's first block: the
/// first `order` positions of every evaluated order are coded verbatim
/// (prediction 0) instead of against a zero history. `win` is the shared
/// per-block Tukey window.
fn plan_signal_win(w: &[i64], n: usize, preset: u8, win: &[f64], warm: bool) -> SignalPlan {
    let blk = &w[MAX_ORDER..];
    let mut best = {
        let r0: Vec<i64> = blk.to_vec();
        let rp = plan_residual(&r0);
        SignalPlan {
            order: 0,
            qshift: 0,
            q: Vec::new(),
            warm_wbits: None,
            warm: Vec::new(),
            ltp: None,
            lms: None,
            resid: r0,
            rplan: rp,
        }
    };
    let r = autocorr(blk, win);
    if r[0] == 0.0 {
        return best;
    }
    let sol = levinson(&r);
    let cands: Vec<usize> = ORDERS
        .iter()
        .skip(1)
        .copied()
        .filter(|&o| !sol[o].is_empty())
        .collect();
    if preset >= 8 {
        for &o in &cands {
            let Some((qshift, q)) = quantize(&sol[o]) else {
                continue;
            };
            let cand = eval_order(w, n, o, qshift, q, if warm { o } else { 0 });
            if cand.cost() < best.cost() {
                best = cand;
            }
        }
    } else {
        // Rank candidates by a quantized-residual proxy: predict a strided
        // subset of block positions with the real (15-bit) coefficients and
        // estimate total residual energy from their mean |r|. This sees the
        // actual quantization damage a Levinson-error estimate cannot.
        let stride = if preset == 0 { 4 } else { 2 };
        let keep = if preset == 0 { 1 } else { 2 };
        let mut ranked: Vec<(f64, usize, u8, Vec<i16>)> = Vec::with_capacity(cands.len());
        for &o in &cands {
            let Some((qshift, q)) = quantize(&sol[o]) else {
                continue;
            };
            // warm-up positions are carried verbatim, not as residuals: skip
            // them in the mean-|r| estimate and charge `order * wbits` bits
            let warm_n = (o.min(n)).min(if warm { o } else { 0 });
            let wbits_est = u64::from(signed_bits(
                &(0..warm_n).map(|i| w[MAX_ORDER + i]).collect::<Vec<_>>(),
            ));
            let warm_n = if wbits_est <= 63 { warm_n } else { 0 };
            let (mut sum_abs, mut m) = (0u128, 0u64);
            let mut i = 0;
            while i < n {
                if i >= warm_n {
                    let r_i =
                        w[MAX_ORDER + i].wrapping_sub(predict_at(w, MAX_ORDER + i, &q, qshift));
                    sum_abs += u128::from(r_i.unsigned_abs());
                    m += 1;
                }
                i += stride;
            }
            let mean = if m > 0 {
                sum_abs as f64 / m as f64
            } else {
                0.0
            };
            let proxy = n as f64 * dlog2(mean + 1.0)
                + o as f64 * f64::from(LPC_PREC)
                + o as f64 * wbits_est as f64;
            ranked.push((proxy, o, qshift, q));
        }
        ranked.sort_by(|a, b| {
            a.0.partial_cmp(&b.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.1.cmp(&b.1)) // ties prefer the lower order
        });
        let mut top: Vec<(usize, u8, Vec<i16>)> = ranked
            .into_iter()
            .take(keep)
            .map(|(_, o, qshift, q)| (o, qshift, q))
            .collect();
        top.sort_unstable_by_key(|&(o, _, _)| o);
        for (o, qshift, q) in top {
            let cand = eval_order(w, n, o, qshift, q, if warm { o } else { 0 });
            if cand.cost() < best.cost() {
                best = cand;
            }
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
    a.iter()
        .zip(b)
        .map(|(&x, &y)| x.wrapping_add(y) >> 1)
        .collect()
}

/// Apply a quantized stereo gain: round(g·v / 128) with wrapping
/// arithmetic — identical on encode and decode, so wrap-around cancels.
#[inline]
fn apply_gain(g: i8, v: i64) -> i64 {
    (i64::from(g).wrapping_mul(v).wrapping_add(64)) >> 7
}

/// Quantized gain minimizing Σ|b − g·a|² for `a` kept / side `b − g·a`:
/// g = Σab/Σa² on a 1/128 grid, clamped to an i8. Returns 0 when `a` is
/// silent (the gain mode then degrades to a fixed mode on equal cost).
fn gain_coef(a: &[i64], b: &[i64]) -> i8 {
    let (mut num, mut den) = (0i128, 0i128);
    for (&x, &y) in a.iter().zip(b) {
        num += x as i128 * y as i128;
        den += x as i128 * x as i128;
    }
    if den == 0 {
        return 0;
    }
    ((num * 128) / den).clamp(-128, 127) as i8
}

// ------------------------------------------------------------------ LMS
//
// Optional last-mile decorrelator on the wire residual (WavPack-style
// sign-sign adaptive weights): d[i] = s[i] − round((w1·s[i−1] +
// w2·s[i−2]) / 2^10), weights move ±step on sign agreement. Pure integer
// and replayed identically by the decoder. `step` indexes LMS_STEPS.

pub const LMS_STEPS: [i64; 4] = [2, 4, 8, 16];
const LMS_SHIFT: u32 = 10;
const LMS_WMAX: i64 = 1 << 12;

fn lms_forward(s: &[i64], step: i64) -> Vec<i64> {
    let (mut w1, mut w2) = (0i64, 0i64);
    let mut e = Vec::with_capacity(s.len());
    for (i, &v) in s.iter().enumerate() {
        let s1 = if i >= 1 { s[i - 1] } else { 0 };
        let s2 = if i >= 2 { s[i - 2] } else { 0 };
        let p = (w1
            .wrapping_mul(s1)
            .wrapping_add(w2.wrapping_mul(s2))
            .wrapping_add(512))
            >> LMS_SHIFT;
        let d = v.wrapping_sub(p);
        e.push(d);
        if d != 0 {
            let ds = if d > 0 { step } else { -step };
            if s1 != 0 {
                w1 = (w1 + if s1 > 0 { ds } else { -ds }).clamp(-LMS_WMAX, LMS_WMAX);
            }
            if s2 != 0 {
                w2 = (w2 + if s2 > 0 { ds } else { -ds }).clamp(-LMS_WMAX, LMS_WMAX);
            }
        }
    }
    e
}

fn lms_inverse(e: &[i64], step: i64) -> Vec<i64> {
    let (mut w1, mut w2) = (0i64, 0i64);
    let mut s = Vec::with_capacity(e.len());
    for (i, &d) in e.iter().enumerate() {
        let s1 = if i >= 1 { s[i - 1] } else { 0 };
        let s2 = if i >= 2 { s[i - 2] } else { 0 };
        let p = (w1
            .wrapping_mul(s1)
            .wrapping_add(w2.wrapping_mul(s2))
            .wrapping_add(512))
            >> LMS_SHIFT;
        s.push(d.wrapping_add(p));
        if d != 0 {
            let ds = if d > 0 { step } else { -step };
            if s1 != 0 {
                w1 = (w1 + if s1 > 0 { ds } else { -ds }).clamp(-LMS_WMAX, LMS_WMAX);
            }
            if s2 != 0 {
                w2 = (w2 + if s2 > 0 { ds } else { -ds }).clamp(-LMS_WMAX, LMS_WMAX);
            }
        }
    }
    s
}

/// `other` minus the gained `kept`: side channel for modes 4/5.
fn gain_side(kept: &[i64], other: &[i64], g: i8) -> Vec<i64> {
    other
        .iter()
        .zip(kept)
        .map(|(&o, &k)| o.wrapping_sub(apply_gain(g, k)))
        .collect()
}

/// Coded signal pair for a stereo mode over any window: 0 LR, 1 LS, 2 RS,
/// 3 MS, 4 L + R−g·L, 5 R + L−g·R (`g` only read for 4/5).
fn stereo_forward(mode: u8, g: i8, y0: &[i64], y1: &[i64]) -> (Vec<i64>, Vec<i64>) {
    match mode {
        1 => (y0.to_vec(), sub(y0, y1)),
        2 => (y1.to_vec(), sub(y0, y1)),
        3 => (mid(y0, y1), sub(y0, y1)),
        4 => (y0.to_vec(), gain_side(y0, y1, g)),
        5 => (y1.to_vec(), gain_side(y1, y0, g)),
        _ => (y0.to_vec(), y1.to_vec()),
    }
}

/// Inverse of `stereo_forward` for one sample.
fn stereo_inverse(mode: u8, g: i8, a: i64, b: i64) -> (i64, i64) {
    match mode {
        1 => (a, a.wrapping_sub(b)),
        2 => (b.wrapping_add(a), a),
        3 => {
            let mid = a.wrapping_mul(2) | (b & 1);
            (mid.wrapping_add(b) >> 1, mid.wrapping_sub(b) >> 1)
        }
        4 => (a, b.wrapping_add(apply_gain(g, a))),
        5 => (b.wrapping_add(apply_gain(g, a)), a),
        _ => (a, b),
    }
}

/// Index pair into the per-block plan pool
/// [L=0, R=1, side=2, mid=3, gain-side-R=4, gain-side-L=5].
fn mode_of(mode: u8) -> (usize, usize) {
    match mode {
        1 => (0, 2),
        2 => (1, 2),
        3 => (3, 2),
        4 => (0, 4),
        5 => (1, 5),
        _ => (0, 1),
    }
}

// ---------------------------------------------------------------- planning

pub struct TrackCtx<'a> {
    pub x: &'a [Vec<i64>],          // code streams (int samples, or I for float)
    pub d: Option<&'a [Vec<DBit>]>, // float tail data
    pub float_word_bits: u32,       // stream word width for escapes
}

pub struct BlockPlan {
    pub mode: u8,
    /// Quantized stereo gain for modes 4/5 (unused otherwise).
    pub gain: i8,
    pub signals: Vec<SignalPlan>,
}

pub struct TrackPlan {
    pub blocks: Vec<BlockPlan>,
}

/// Result for one block: the plan pieces. The sample window is rebuilt in
/// the sequential loop (cheap copy) rather than kept.
struct VarA {
    mode_a: u8,
    gain_a: i8,
    sigs_a: Vec<SignalPlan>,
}

/// One block's plan; pure function of the input streams — safe to compute
/// in parallel across blocks.
fn variant_a(
    ctx: &TrackCtx<'_>,
    stereo: bool,
    s: usize,
    e: usize,
    preset: u8,
    win: &[f64],
) -> VarA {
    let n = e - s;
    let w: Vec<Vec<i64>> = ctx.x.iter().map(|x| window(x, s, e)).collect();
    let warm = s == 0;
    let ps = |wc: &[i64]| plan_signal_win(wc, n, preset, win, warm);
    let raw: Vec<SignalPlan> = w.iter().map(|wc| ps(wc)).collect();
    let (mode_a, gain_a, sigs_a) = if stereo {
        let sig_s = ps(&sub(&w[0], &w[1]));
        let sig_m = ps(&mid(&w[0], &w[1]));
        // adaptive-gain sides: s = other - round(g·kept), g on a 1/128
        // grid. A near-zero gain degenerates to a plain side already
        // covered by modes 1-3 — skip the two extra signal plans then.
        let g4 = gain_coef(&w[0], &w[1]); // keep L, side = R - g·L
        let g5 = gain_coef(&w[1], &w[0]); // keep R, side = L - g·R
        let sig_gr = (g4.abs() >= 4).then(|| ps(&gain_side(&w[0], &w[1], g4)));
        let sig_gl = (g5.abs() >= 4).then(|| ps(&gain_side(&w[1], &w[0], g5)));
        let c = [
            raw[0].cost(),
            raw[1].cost(),
            sig_s.cost(),
            sig_m.cost(),
            sig_gr.as_ref().map_or(u64::MAX, SignalPlan::cost),
            sig_gl.as_ref().map_or(u64::MAX, SignalPlan::cost),
        ];
        // +8 on modes 4/5 = the gain field those modes carry on the wire
        let modes = [
            c[0] + c[1],
            c[0] + c[2],
            c[1] + c[2],
            c[3] + c[2],
            c[0].saturating_add(c[4]).saturating_add(8),
            c[1].saturating_add(c[5]).saturating_add(8),
        ];
        let m = modes
            .iter()
            .enumerate()
            .min_by_key(|&(_, &c)| c)
            .map(|(i, _)| i)
            .unwrap();
        let mut pool: Vec<Option<SignalPlan>> = raw
            .into_iter()
            .map(Some)
            .chain([Some(sig_s), Some(sig_m), sig_gr, sig_gl])
            .collect();
        let (i, j) = mode_of(m as u8);
        let sigs = vec![pool[i].take().unwrap(), pool[j].take().unwrap()];
        let gain = match m {
            4 => g4,
            5 => g5,
            _ => 0,
        };
        (m as u8, gain, sigs)
    } else {
        (0u8, 0, raw)
    };
    VarA {
        mode_a,
        gain_a,
        sigs_a,
    }
}

// Kept as the exact preset-8/single-thread entry point (tests);
// encode paths go through `plan_track_with`.
#[allow(dead_code)]
pub fn plan_track(ctx: &TrackCtx<'_>) -> TrackPlan {
    plan_track_with(ctx, 8, 1)
}

/// Plan a track. `preset` selects the LPC order search depth (0/5/8; see
/// `plan_signal`). `threads` splits the per-block variant-A work across
/// `std::thread::scope` chunks; the output is identical for any thread count.
pub fn plan_track_with(ctx: &TrackCtx<'_>, preset: u8, threads: usize) -> TrackPlan {
    let ch = ctx.x.len();
    let frames = ctx.x.first().map_or(0, |x| x.len());
    let stereo = ch == 2;
    let nb = frames.div_ceil(BLOCK);
    let mut plans = Vec::with_capacity(nb);

    // variant A for all blocks — independent, parallelized over chunks.
    // The Tukey window depends only on block length: hoist it (full +
    // tail) instead of recomputing ~2000 cos() per block.
    let win_full = tukey(BLOCK.min(frames.max(1)));
    let tail = frames % BLOCK;
    let win_tail = if tail > 0 { tukey(tail) } else { Vec::new() };
    let va_at = |b: usize| {
        let s = b * BLOCK;
        let e = (s + BLOCK).min(frames);
        let win = if e - s == win_full.len() {
            &win_full
        } else {
            &win_tail
        };
        variant_a(ctx, stereo, s, e, preset, win)
    };
    let vas: Vec<VarA> = if threads > 1 && nb > 1 {
        let chunk = nb.div_ceil(threads.min(nb));
        let mut slots: Vec<Option<VarA>> = (0..nb).map(|_| None).collect();
        std::thread::scope(|sc| {
            for (ci, slice) in slots.chunks_mut(chunk).enumerate() {
                let base = ci * chunk;
                sc.spawn(move || {
                    for (i, slot) in slice.iter_mut().enumerate() {
                        *slot = Some(va_at(base + i));
                    }
                });
            }
        });
        slots.into_iter().map(|s| s.unwrap()).collect()
    } else {
        (0..nb).map(va_at).collect()
    };

    for va in vas {
        plans.push(BlockPlan {
            mode: va.mode_a,
            gain: va.gain_a,
            signals: va.sigs_a,
        });
    }

    // Long-term prediction post-pass: for each coded signal in each block,
    // pitch-predict the LPC residual (e[n] -= round(g·e[n−τ])) when it pays
    // for its 21-bit header. History = the reconstructed residual stream
    // (pre-LTP values), capped at the last LTP_MAX_LAG samples — identical
    // on encode and decode.
    // Long-term prediction post-pass: for each coded signal in each block,
    // pitch-predict the LPC residual (e[n] -= round(g·e[n−τ])) when it pays
    // for its 21-bit header. `hist[s]` carries the reconstructed residual
    // stream (pre-LTP values — the decoder sees exactly these), trimmed to
    // the deepest usable lag.
    let nsig = plans.first().map_or(0, |b| b.signals.len());
    let mut hist: Vec<Vec<i64>> = vec![Vec::new(); nsig];
    for bp in plans.iter_mut() {
        for (si, sig) in bp.signals.iter_mut().enumerate() {
            let base = hist[si].len();
            hist[si].extend_from_slice(&sig.resid);
            if let Some((tau, g, e2, rp2)) = ltp_try(&hist[si], base, sig.rplan.cost) {
                sig.ltp = Some((tau, g));
                sig.resid = e2;
                sig.rplan = rp2;
            }
            let hl = hist[si].len();
            if hl > LTP_HIST {
                hist[si].drain(..hl - LTP_HIST);
            }
            // adaptive last-mile pass on the (possibly post-LTP) residual;
            // the u2 step field must be repaid. Step choice probes a
            // strided subsample — exact cost only for the winning step.
            if sig.rplan.cost > 4096 {
                let probe: Vec<i64> = sig.resid.iter().step_by(4).copied().collect();
                let mut si_best = (0u8, u64::MAX);
                for (idx, &step) in LMS_STEPS.iter().enumerate() {
                    let ep = lms_forward(&probe, step);
                    let rp = plan_residual(&ep);
                    if rp.cost < si_best.1 {
                        si_best = (idx as u8, rp.cost);
                    }
                }
                let e3 = lms_forward(&sig.resid, LMS_STEPS[si_best.0 as usize]);
                let rp3 = plan_residual(&e3);
                if rp3.cost + 2 < sig.rplan.cost {
                    sig.lms = Some(si_best.0);
                    sig.resid = e3;
                    sig.rplan = rp3;
                }
            }
        }
    }

    TrackPlan { blocks: plans }
}

/// LTP history kept for taps; the τ field is 12 bits, lags are also
/// capped well below that for cost/benefit.
const LTP_HIST: usize = 4096;
const LTP_MAX_LAG: usize = 2048;

/// Try long-term prediction of the block at `stream[base..]` — `stream`
/// is the reconstructed residual stream (blocks before `base` are
/// history). Taps read `stream[j − τ]`; position j ∈ [base, end).
///
/// Returns `(τ, g, resid2, rplan2)` when the re-planned post-LTP residual
/// beats `old_cost` by more than the 20-bit LTP header.
///
/// Search: normalized correlation over τ ∈ [32, min(2048, base)] on a
/// decimated grid (τ step 8, sample stride 8), then an exact
/// gain/residual-plan evaluation at τ0−1, τ0, τ0+1.
fn ltp_try(
    stream: &[i64],
    base: usize,
    old_cost: u64,
) -> Option<(u16, i8, Vec<i64>, ResidualPlan)> {
    let n = stream.len() - base;
    let tau_max = LTP_MAX_LAG.min(base);
    if tau_max < 32 || n < 64 {
        return None;
    }
    // coarse scan: normalized |correlation| on a strided grid
    let mut best: Option<(f64, usize)> = None;
    let mut tau = 32usize;
    while tau <= tau_max {
        let (mut num, mut d1, mut d2) = (0f64, 0f64, 0f64);
        let mut i = base;
        while i < stream.len() {
            let a = stream[i] as f64;
            let b = stream[i - tau] as f64;
            num += a * b;
            d1 += a * a;
            d2 += b * b;
            i += 8;
        }
        let score = if d1 > 0.0 && d2 > 0.0 {
            num.abs() / (d1.sqrt() * d2.sqrt())
        } else {
            0.0
        };
        if best.is_none_or(|(bs, _)| score > bs) {
            best = Some((score, tau));
        }
        tau += 8;
    }
    let (score, tau0) = best?;
    if score < 0.25 {
        return None; // no real periodicity — skip the exact pass
    }
    let mut out: Option<(u16, i8, Vec<i64>, ResidualPlan, u64)> = None;
    for tau_c in [tau0.saturating_sub(1), tau0, (tau0 + 1).min(tau_max)] {
        if tau_c == 0 || tau_c > tau_max {
            continue;
        }
        let (mut num, mut den) = (0i128, 0i128);
        for i in base..stream.len() {
            let b = stream[i - tau_c];
            num += stream[i] as i128 * b as i128;
            den += b as i128 * b as i128;
        }
        if den == 0 {
            continue;
        }
        let g = ((num * 128) / den).clamp(-128, 127) as i8;
        if g == 0 {
            continue;
        }
        let e2: Vec<i64> = (0..n)
            .map(|i| stream[base + i].wrapping_sub(apply_gain(g, stream[base + i - tau_c])))
            .collect();
        let rp2 = plan_residual(&e2);
        let c2 = rp2.cost;
        if out.as_ref().is_none_or(|o| c2 < o.4) {
            out = Some((tau_c as u16, g, e2, rp2, c2));
        }
    }
    let (tau, g, e2, rp2, total) = out?;
    // the 20-bit LTP header (τ u12 + g i8) must be more than repaid
    if total + 20 < old_cost {
        Some((tau, g, e2, rp2))
    } else {
        None
    }
}

// ---------------------------------------------------------------- writing

pub fn write_track(bw: &mut BitWriter, ctx: &TrackCtx<'_>, plan: &TrackPlan) {
    let ch = ctx.x.len();
    let frames = ctx.x.first().map_or(0, |x| x.len());
    let stereo = ch == 2;
    for (b, bp) in plan.blocks.iter().enumerate() {
        let s = b * BLOCK;
        let e = (s + BLOCK).min(frames);
        if stereo {
            bw.write(u64::from(bp.mode), 3);
            if bp.mode >= 4 {
                bw.write_signed(i64::from(bp.gain), 8);
            }
        }
        for sig in &bp.signals {
            bw.write(u64::from(sig.order), 6);
            if sig.order > 0 {
                bw.write(u64::from(LPC_PREC) - 1, 4);
                bw.write(u64::from(sig.qshift), 5);
                for &q in &sig.q {
                    bw.write_signed(i64::from(q), LPC_PREC);
                }
                if let Some(wbits) = sig.warm_wbits {
                    bw.write(u64::from(wbits), 6);
                    for &v in &sig.warm {
                        bw.write_signed(v, u32::from(wbits));
                    }
                }
            }
            // LTP flag + fields (τ u12, g i8) — the stored residual is
            // then the post-LTP stream
            bw.write(sig.ltp.is_some() as u64, 1);
            if let Some((tau, g)) = sig.ltp {
                bw.write(u64::from(tau), 12);
                bw.write_signed(i64::from(g), 8);
            }
            // LMS flag + u2 step index; the stored residual is post-LMS
            bw.write(sig.lms.is_some() as u64, 1);
            if let Some(si) = sig.lms {
                bw.write(u64::from(si), 2);
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
    pub mw: u8, // 23 or 52
    pub e: i16,
    pub stream_bits: u32, // 32 or 64
    pub pcm_bits: u16,    // 32 or 64 (mode 2: stream 32, pcm 64)
}

pub struct TrackMeta {
    pub channels: usize,
    pub frames: usize,
    pub float: Option<FloatMeta>,
}

pub struct DecodedCore {
    pub x: Vec<Vec<i64>>,             // code streams (I for float)
    pub words: Option<Vec<Vec<u64>>>, // PCM-domain float words
}

pub fn read_track(r: &mut BitReader<'_>, meta: &TrackMeta) -> Result<DecodedCore, String> {
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
    let mut x = vec![vec![0i64; meta.frames]; ch];
    let mut words = meta
        .float
        .as_ref()
        .map(|_| vec![vec![0u64; meta.frames]; ch]);
    // reconstructed-residual history per coded signal, for LTP taps
    let mut resid_hist: Vec<Vec<i64>> = vec![Vec::new(); nsig];

    for b in 0..nb {
        let s = b * BLOCK;
        let e = (s + BLOCK).min(meta.frames);
        let n = e - s;

        // 1. stereo mode (u3; modes 4/5 carry an 8-bit quantized gain)
        let mode = if stereo { r.read(3) as u8 } else { 0 };
        if r.overrun {
            return Err("truncated payload".into());
        }
        if mode > 5 {
            return Err(format!("stereo mode {mode} out of range"));
        }
        let gain = if mode >= 4 {
            let g = r.read_signed(8) as i8;
            if r.overrun {
                return Err("truncated payload".into());
            }
            g
        } else {
            0
        };

        // history of the coded signals = transform of x-history over W
        let mut y_hist: Vec<Vec<i64>> = vec![vec![0; MAX_ORDER]; ch];
        for c in 0..ch {
            for (i, yv) in y_hist[c].iter_mut().enumerate() {
                let gp = s as i64 - MAX_ORDER as i64 + i as i64;
                *yv = if gp >= 0 { x[c][gp as usize] } else { 0 };
            }
        }
        let coded_hist: Vec<Vec<i64>> = if stereo {
            let (a, b) = stereo_forward(mode, gain, &y_hist[0], &y_hist[1]);
            vec![a, b]
        } else {
            y_hist.clone()
        };

        // 2. coded signals
        let nsig = if stereo { 2 } else { ch };
        let mut coded: Vec<Vec<i64>> = Vec::with_capacity(nsig);
        for (si, hist) in coded_hist.iter().take(nsig).enumerate() {
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
            // warm-up block: `order` verbatim values follow the coefficients
            // (wbits == 0 marks none — signed width was unrepresentable)
            let mut warm_vals: Vec<i64> = Vec::new();
            if b == 0 && order > 0 {
                let wbits = r.read(6) as u32;
                if wbits > 0 {
                    for _ in 0..order {
                        warm_vals.push(r.read_signed(wbits));
                    }
                }
                if r.overrun {
                    return Err("truncated payload".into());
                }
            }
            // LTP: flag, then τ (u12) + gain (i8). The residual on the wire
            // is the post-LTP stream; taps read the reconstructed residual
            // history (positions before the stream read as 0).
            let ltp = if r.read(1) == 1 {
                let tau = r.read(12) as usize;
                let g = r.read_signed(8) as i8;
                if r.overrun {
                    return Err("truncated payload".into());
                }
                if tau == 0 {
                    return Err("LTP lag out of range".into());
                }
                Some((tau, g))
            } else {
                if r.overrun {
                    return Err("truncated payload".into());
                }
                None
            };
            // LMS: flag + u2 step index (LMS_STEPS). The wire residual is
            // post-LMS — invert it before the LTP reconstruction below.
            let lms = if r.read(1) == 1 {
                let si = r.read(2) as usize;
                if r.overrun {
                    return Err("truncated payload".into());
                }
                Some(LMS_STEPS[si])
            } else {
                if r.overrun {
                    return Err("truncated payload".into());
                }
                None
            };
            let mut resid = read_residual(r, n)?;
            if let Some(step) = lms {
                resid = lms_inverse(&resid, step);
            }
            if let Some((tau, g)) = ltp {
                let rh = &resid_hist[si];
                for i in 0..n {
                    let j = rh.len() as i64 + i as i64 - tau as i64;
                    let tap = if j < 0 {
                        0
                    } else if (j as usize) < rh.len() {
                        rh[j as usize]
                    } else {
                        resid[i - tau]
                    };
                    resid[i] = resid[i].wrapping_add(apply_gain(g, tap));
                }
            }
            resid_hist[si].extend_from_slice(&resid);
            let hl = resid_hist[si].len();
            if hl > LTP_HIST {
                resid_hist[si].drain(..hl - LTP_HIST);
            }
            let mut aw: Vec<i64> = hist.clone();
            for (i, &e) in resid.iter().enumerate() {
                if i < warm_vals.len() {
                    // warm-up position: the residual is a 0 placeholder and
                    // the verbatim value is the sample
                    if e != 0 {
                        return Err("warm-up residual must be 0".into());
                    }
                    aw.push(warm_vals[i]);
                    continue;
                }
                let mut p = 0i64;
                let l = aw.len();
                for (k, &c) in q.iter().enumerate() {
                    p = p.wrapping_add(i64::from(c).wrapping_mul(aw[l - k - 1]));
                }
                aw.push(e.wrapping_add(p >> qshift));
            }
            coded.push(aw[MAX_ORDER..].to_vec());
        }

        // 3. inverse stereo transform
        if stereo {
            for i in 0..n {
                let (y0, y1) = stereo_inverse(mode, gain, coded[0][i], coded[1][i]);
                x[0][s + i] = y0;
                x[1][s + i] = y1;
            }
        } else {
            for c in 0..ch {
                x[c][s..e].copy_from_slice(&coded[c]);
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

    #[test]
    fn portable_math_close_to_libm() {
        for i in 0..=1000 {
            let x = std::f64::consts::PI * i as f64 / 1000.0;
            assert!((dcos(x) - x.cos()).abs() < 1e-15, "dcos({x})");
        }
        for i in 1..=1000 {
            let x = i as f64 / 7.3;
            assert!((dlog2(x) - x.log2()).abs() < 1e-12, "dlog2({x})");
        }
        for (x, want) in [
            (f64::MIN_POSITIVE, -1022),
            (f64::from_bits(1), -1074),
            (0.5, -1),
            (1.0, 0),
            (7.999, 2),
            (8.0, 3),
            (1024.0, 10),
        ] {
            assert_eq!(floor_log2(x), want, "floor_log2({x})");
        }
    }

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
            f32::from_bits(1),           // min denormal
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
            0.1,                                   // not f32-exact -> forces mode 3 path (mw=52)
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
        let w32: Vec<u64> = vals
            .iter()
            .map(|&v| u64::from((v as f32).to_bits()))
            .collect();
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
            for mode in 0..6u8 {
                for g in [0i8, 96, -64, 127, -128] {
                    let (a, b) = stereo_forward(mode, g, &[l], &[r]);
                    let (l2, r2) = stereo_inverse(mode, g, a[0], b[0]);
                    assert_eq!((l, r), (l2, r2), "mode {mode} g{g} ({l},{r})");
                }
            }
        }
        // random sweep
        for _ in 0..10_000 {
            let l = (lcg() as i32) as i64 - (1i64 << 31);
            let r = (lcg() as i32) as i64 - (1i64 << 31);
            let g = (lcg() % 256) as u8 as i8;
            for mode in 0..6u8 {
                let (a, b) = stereo_forward(mode, g, &[l], &[r]);
                let (l2, r2) = stereo_inverse(mode, g, a[0], b[0]);
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
        };
        let plan = plan_track(&ctx);
        let mut bw = BW::new();
        write_track(&mut bw, &ctx, &plan);
        let bytes = bw.into_bytes();
        let mut r = BR::new(&bytes);
        let meta = TrackMeta {
            channels: 1,
            frames: n,
            float: None,
        };
        let core = read_track(&mut r, &meta).unwrap();
        assert_eq!(core.x[0], x);
        assert!(!r.overrun);
    }

    #[test]
    fn warmup_cost_and_roundtrip() {
        // mono high-amplitude narrowband sine: block 0 picks a real LPC
        // order and emits warm-up fields; SignalPlan::cost must equal the
        // emitted bits exactly
        let n = 2 * BLOCK + 500;
        // 6 incommensurate sinusoids + noise floor: needs order >= 8
        let x: Vec<i64> = (0..n)
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
            .collect();
        let xs = vec![x.clone()];
        let ctx = TrackCtx {
            x: &xs,
            d: None,
            float_word_bits: 0,
        };
        let plan = plan_track_with(&ctx, 8, 1);
        let s0 = &plan.blocks[0].signals[0];
        assert!(s0.order >= 8, "sine block 0 should pick a real order");
        assert!(s0.warm_wbits.unwrap_or(0) >= 1);
        assert_eq!(s0.warm.len(), s0.order as usize);
        assert!(plan.blocks[1].signals[0].warm_wbits.is_none());
        let mut bw = BW::new();
        write_track(&mut bw, &ctx, &plan);
        let emitted: u64 = plan
            .blocks
            .iter()
            .flat_map(|b| b.signals.iter())
            .map(|s| s.cost())
            .sum();
        assert_eq!(bw.bits, emitted, "cost() must equal emitted bits");
        let bytes = bw.into_bytes();
        let mut r = BR::new(&bytes);
        let meta = TrackMeta {
            channels: 1,
            frames: n,
            float: None,
        };
        let core = read_track(&mut r, &meta).unwrap();
        assert_eq!(core.x[0], x);
        assert!(!r.overrun);
    }

    #[test]
    fn plan_track_with_parallel_is_deterministic() {
        // multi-block stereo int signal: parallel variant-A planning must
        // produce the exact same bits as the single-thread exact planner
        let n = 10_000usize; // > 2 blocks
        let mk = |seed: u64| -> Vec<i64> {
            let mut st = seed;
            (0..n)
                .map(|i| {
                    st = st.wrapping_mul(6364136223846793005).wrapping_add(1);
                    ((i as f64 * 0.03).sin() * 100_000.0 + ((st >> 40) % 700) as f64) as i64
                })
                .collect()
        };
        let xs = vec![mk(1), mk(7)];
        let d: Option<&[Vec<DBit>]> = None;
        let ctx = TrackCtx {
            x: &xs,
            d,
            float_word_bits: 0,
        };
        let a = plan_track(&ctx);
        let b = plan_track_with(&ctx, 8, 4);
        let wr = |p: &TrackPlan| {
            let mut bw = BW::new();
            write_track(&mut bw, &ctx, p);
            bw.into_bytes()
        };
        assert_eq!(wr(&a), wr(&b), "threads must not change the bitstream");
    }

    #[test]
    fn lms_roundtrip() {
        let mut st = 0x9E3779B97F4A7C15u64;
        let mut s = Vec::with_capacity(1000);
        for _ in 0..1000 {
            st = st
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            s.push(((st >> 33) % 2000) as i64 - 1000);
        }
        for &step in &LMS_STEPS {
            assert_eq!(lms_inverse(&lms_forward(&s, step), step), s, "step {step}");
        }
        let z = vec![0i64; 256];
        assert_eq!(lms_inverse(&lms_forward(&z, 8), 8), z);
    }
}
