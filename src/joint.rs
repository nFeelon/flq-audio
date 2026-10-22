//! Joint analysis: per-segment NCC against a reference track via FFT,
//! per-block ref choice.

use rustfft::FftPlanner;
use rustfft::num_complex::Complex64;
use std::borrow::Cow;

pub const SEG: usize = 65536;
pub const NCC_MIN: f64 = 0.2;
pub const PEAK_EXCL: i64 = 64;
pub const MIN_OVERLAP: usize = 4096;
pub const REFINE: i32 = 8;
pub const ENERGY_GATE: f64 = 0.04;
/// Reference-pool screening: fully analyze at most this many candidates.
pub const SCREEN_K: usize = 4;
/// Coarse screening decimation factor (mean of this many samples).
pub const SCREEN_DEC: usize = 64;
/// Analysis size bound: the reference is decimated to at most this many
/// samples before the FFT (keeps a bounded working set on long tracks).
pub const MAX_ANALYSIS: usize = 1 << 21;

#[derive(Clone, Copy, Debug)]
pub struct RefChoice {
    /// Index into the track's source pool (0 = self when present).
    pub src: u8,
    pub ref_ch: u8,
    pub lag: i32,
    pub gm: i32,
    pub gshift: u8,
}

pub struct JointAnalysis {
    pub max_ncc: f64,
    pub seg_peaks: Vec<Vec<i32>>, // candidate lags per SEG segment of track k
    /// Lag refinement radius the planner must use with these peaks
    /// (`max(REFINE, decimation factor)`).
    pub refine: i32,
}

/// Block starting at `s` maps to its analysis segment; a merged tail belongs
/// to the last segment.
pub fn seg_index(s: usize, nseg: usize) -> usize {
    (s / SEG).min(nseg - 1)
}

/// Reference prediction `P = (gm * r(j) + round) >> gshift`, round-to-nearest.
/// `r(j) = 0` outside `[0, r.len())`. Shared by encoder planning and decoder.
pub fn ref_pred(g: &RefChoice, r: &[i64], j: i64) -> i64 {
    if j < 0 || j as usize >= r.len() {
        return 0;
    }
    let round = if g.gshift > 0 { 1i64 << (g.gshift - 1) } else { 0 };
    i64::from(g.gm).wrapping_mul(r[j as usize]).wrapping_add(round) >> g.gshift
}

fn prefix_sq(v: &[f64]) -> Vec<f64> {
    let mut ps = vec![0.0; v.len() + 1];
    for (i, &x) in v.iter().enumerate() {
        ps[i + 1] = ps[i] + x * x;
    }
    ps
}

fn prefix(v: &[f64]) -> Vec<f64> {
    let mut ps = vec![0.0; v.len() + 1];
    for (i, &x) in v.iter().enumerate() {
        ps[i + 1] = ps[i] + x;
    }
    ps
}

/// Cross-correlation scan of every SEG segment of `mk` against the whole of
/// `m0`. Both inputs are mono mixes of the code streams; means are removed
/// here. Only lags `d - seg_start <= lag_max` are reported (e.g.
/// `-(BLOCK as i64)` restricts self-analysis to earlier material; pass
/// `i64::MAX` for cross-track analysis). The reference is decimated by the
/// smallest power of two that bounds it to `MAX_ANALYSIS` samples, keeping
/// FFT memory bounded on long tracks (D=1 for short tracks — identical
/// results); returned lags are scaled back to full-rate samples.
pub fn analyze(m0: &[f64], mk: &[f64], lag_max: i64) -> JointAnalysis {
    let lenk_orig = mk.len();
    let mut dec = 1usize;
    while m0.len().div_ceil(dec) > MAX_ANALYSIS {
        dec <<= 1;
    }
    let decimate = |v: &[f64]| -> Vec<f64> {
        v.chunks(dec)
            .map(|c| c.iter().sum::<f64>() / c.len() as f64)
            .collect()
    };
    let (m0, mk): (Cow<'_, [f64]>, Cow<'_, [f64]>) = if dec == 1 {
        (Cow::Borrowed(m0), Cow::Borrowed(mk))
    } else {
        (Cow::Owned(decimate(m0)), Cow::Owned(decimate(mk)))
    };
    // work in decimated units; segment boundaries are floored to it and lags
    // are scaled back by `dec` before being stored
    let len0 = m0.len();
    let lenk = mk.len();
    // tail shorter than SEG/2 merges into the previous segment (original units)
    let nseg = ((lenk_orig + SEG / 2) / SEG).max(1);
    let longest = (0..nseg)
        .map(|si| {
            let s2 = if si + 1 == nseg { lenk } else { ((si + 1) * SEG) / dec };
            s2.saturating_sub((si * SEG) / dec)
        })
        .max()
        .unwrap_or(0);
    let n = (len0 + longest).next_power_of_two().max(2);
    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(n);
    let ift = planner.plan_fft_inverse(n);

    let mean0 = m0.iter().sum::<f64>() / len0.max(1) as f64;
    let m0c: Vec<f64> = m0.iter().map(|&v| v - mean0).collect();
    let ps0 = prefix_sq(&m0c);
    let ps0r = prefix_sq(&m0); // raw, for the constant-window guard
    let pm0 = prefix(&m0);
    let mut f0 = vec![Complex64::ZERO; n];
    for (i, &v) in m0c.iter().enumerate() {
        f0[i] = Complex64::new(v, 0.0);
    }
    fft.process(&mut f0);
    let inv_n = 1.0 / n as f64;

    let dec64 = dec as i64;
    // decimated-domain lag bound (floor division keeps legal lags legal)
    let lag_lim = lag_max.div_euclid(dec64);
    let mut max_ncc = 0f64;
    let mut seg_peaks = Vec::with_capacity(nseg);
    for si in 0..nseg {
        let s1 = (si * SEG) / dec;
        let s2 = if si + 1 == nseg {
            lenk
        } else {
            ((si + 1) * SEG) / dec
        };
        if s1 >= s2 || s1 >= lenk {
            seg_peaks.push(Vec::new());
            continue;
        }
        let seg = &mk[s1..s2.min(lenk)];
        let seglen = seg.len();
        let overlap_min = (MIN_OVERLAP / dec).max(1).max(seglen.min(len0) / 2) as i64;
        let mean = seg.iter().sum::<f64>() / seglen as f64;
        let segc: Vec<f64> = seg.iter().map(|&v| v - mean).collect();
        let pss = prefix_sq(&segc);
        let pssr = prefix_sq(seg); // raw, for the constant-window guard
        let pms = prefix(seg);
        let mut fs = vec![Complex64::ZERO; n];
        for (i, &v) in segc.iter().enumerate() {
            fs[i] = Complex64::new(v, 0.0);
        }
        fft.process(&mut fs);
        let mut c: Vec<Complex64> = f0
            .iter()
            .zip(fs.iter())
            .map(|(a, b)| a * b.conj())
            .collect();
        ift.process(&mut c);

        let d_lo = -(seglen as i64) + 1;
        let d_hi = len0 as i64 - 1;
        let ncc_at = |d: i64| -> Option<f64> {
            let lo = 0i64.max(-d);
            let hi = (seglen as i64).min(len0 as i64 - d);
            if hi - lo < overlap_min {
                return None;
            }
            // a constant overlap window (e.g. digital silence) is a nonzero
            // vector after global mean removal and correlates ±1 with anything
            let nov = (hi - lo) as f64;
            let (h0u, l0u) = ((hi + d) as usize, (lo + d) as usize);
            let rs = pssr[hi as usize] - pssr[lo as usize];
            let r0 = ps0r[h0u] - ps0r[l0u];
            let vs = rs - (pms[hi as usize] - pms[lo as usize]).powi(2) / nov;
            let v0 = r0 - (pm0[h0u] - pm0[l0u]).powi(2) / nov;
            if vs <= rs * 1e-9 || v0 <= r0 * 1e-9 {
                return None;
            }
            let es = pss[hi as usize] - pss[lo as usize];
            let e0 = ps0[(hi + d) as usize] - ps0[(lo + d) as usize];
            if es <= 0.0 || e0 <= 0.0 {
                return None;
            }
            let idx = if d >= 0 { d as usize } else { (n as i64 + d) as usize };
            Some(c[idx].re * inv_n / (es * e0).sqrt())
        };
        let mut p1: Option<(i64, f64)> = None;
        for d in d_lo..=d_hi {
            if d - s1 as i64 > lag_lim {
                continue;
            }
            if let Some(v) = ncc_at(d) {
                max_ncc = max_ncc.max(v.abs());
                if p1.is_none_or(|(_, pv)| v.abs() > pv.abs()) {
                    p1 = Some((d, v));
                }
            }
        }
        let mut peaks: Vec<i32> = Vec::new();
        if let Some((d1, v1)) = p1
            && v1.abs() >= NCC_MIN
        {
            peaks.push(((d1 - s1 as i64) * dec64) as i32);
            let mut p2: Option<(i64, f64)> = None;
            for d in d_lo..=d_hi {
                if (d - d1).abs() <= PEAK_EXCL || d - s1 as i64 > lag_lim {
                    continue;
                }
                if let Some(v) = ncc_at(d)
                    && p2.is_none_or(|(_, pv)| v.abs() > pv.abs())
                {
                    p2 = Some((d, v));
                }
            }
            if let Some((d2, v2)) = p2
                && v2.abs() >= NCC_MIN
            {
                peaks.push(((d2 - s1 as i64) * dec64) as i32);
            }
        }
        seg_peaks.push(peaks);
    }
    JointAnalysis { max_ncc, seg_peaks, refine: REFINE.max(dec as i32) }
}

/// Pick the best reference (channel, lag, gain) for one channel of one block.
/// `x` = block samples `[s, e)`; `cands` = candidate lags refined by
/// +/- `refine`; `lag_ok` vetoes lags (self references only allow material
/// that is already decodable); ref streams indexed absolutely
/// (`r(i+lag)` = 0 outside `[0, r.len())`). Returns the choice (`src` left 0
/// for the caller to fill) and its score for cross-source comparison.
pub fn choose_ref(
    x: &[i64],
    refs: &[Vec<i64>],
    s: usize,
    cands: &[i32],
    refine: i32,
    lag_ok: impl Fn(i32) -> bool,
) -> Option<(RefChoice, f64)> {
    let xx: f64 = x.iter().map(|&v| (v as f64) * (v as f64)).sum();
    if xx <= 0.0 {
        return None;
    }
    let mut best: Option<(f64, usize, i32, f64, f64)> = None; // score, rc, lag, xr, rr
    for &lag0 in cands {
        for (rci, r) in refs.iter().enumerate() {
            for d in -refine..=refine {
                let lag = lag0.wrapping_add(d);
                if !lag_ok(lag) {
                    continue;
                }
                let (mut xr, mut rr) = (0f64, 0f64);
                for (i, &xv) in x.iter().enumerate() {
                    let j = s as i64 + i as i64 + i64::from(lag);
                    let rv = if j >= 0 && (j as usize) < r.len() {
                        r[j as usize] as f64
                    } else {
                        0.0
                    };
                    xr += xv as f64 * rv;
                    rr += rv * rv;
                }
                if rr <= 0.0 {
                    continue;
                }
                let score = xr * xr / rr;
                if best.is_none_or(|(bs, _, _, _, _)| score > bs) {
                    best = Some((score, rci, lag, xr, rr));
                }
            }
        }
    }
    let (score, rci, lag, xr, rr) = best?;
    if score < ENERGY_GATE * xx {
        return None;
    }
    let g = xr / rr;
    if !g.is_finite() || g == 0.0 {
        return None;
    }
    let ex = g.abs().log2().floor() as i32 + 1;
    let gshift = (22 - ex).clamp(0, 63) as u8;
    let gm = (g * 2f64.powi(i32::from(gshift)))
        .round()
        .clamp(-8_388_607.0, 8_388_607.0) as i32;
    if gm == 0 {
        return None;
    }
    Some((RefChoice { src: 0, ref_ch: rci as u8, lag, gm, gshift }, score))
}
