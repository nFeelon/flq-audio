//! Bit-level IO (MSB-first), partitioned Rice coding with escapes, CRC32.

use crate::rc::{ProbBin, RcEnc};

pub const MAX_PART_ORDER: u8 = 8;

#[inline]
fn mask(n: u32) -> u64 {
    if n >= 64 { u64::MAX } else { (1u64 << n) - 1 }
}

#[inline]
pub fn bit_len(v: u64) -> u32 {
    64 - v.leading_zeros()
}

#[inline]
pub fn zigzag(e: i64) -> u64 {
    ((e << 1) ^ (e >> 63)) as u64
}

#[inline]
pub fn unzigzag(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

// ---------------------------------------------------------------- BitWriter

#[derive(Default)]
pub struct BitWriter {
    buf: Vec<u8>,
    acc: u64, // bits are staged MSB-first in the low `nb` bits of acc
    nb: u32,
    pub bits: u64,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Write the low `n` bits of `v`, MSB first. `n` in 0..=64.
    pub fn write(&mut self, v: u64, n: u32) {
        debug_assert!(n <= 64);
        if n == 0 {
            return;
        }
        let v = v & mask(n);
        for i in (0..n).rev() {
            self.acc = (self.acc << 1) | ((v >> i) & 1);
            self.nb += 1;
            if self.nb == 8 {
                self.buf.push(self.acc as u8);
                self.acc = 0;
                self.nb = 0;
            }
        }
        self.bits += u64::from(n);
    }

    /// Two's-complement `v` in `n` bits.
    pub fn write_signed(&mut self, v: i64, n: u32) {
        self.write((v as u64) & mask(n), n);
    }

    /// `q` zero bits then a one bit.
    pub fn write_unary(&mut self, q: u64) {
        let mut rem = q;
        while rem > 0 {
            let c = rem.min(64);
            self.write(0, c as u32);
            rem -= c;
        }
        self.write(1, 1);
    }

    pub fn into_bytes(mut self) -> Vec<u8> {
        if self.nb > 0 {
            self.buf.push((self.acc << (8 - self.nb)) as u8);
        }
        self.buf
    }
}

// ---------------------------------------------------------------- BitReader

pub struct BitReader<'a> {
    buf: &'a [u8],
    pos: u64, // bit position
    pub overrun: bool,
}

impl<'a> BitReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            overrun: false,
        }
    }

    /// Read `n` bits MSB-first. Bits past the end read as 0 and set `overrun`.
    pub fn read(&mut self, n: u32) -> u64 {
        debug_assert!(n <= 64);
        let mut v = 0u64;
        for _ in 0..n {
            let byte = (self.pos / 8) as usize;
            let bit = if byte < self.buf.len() {
                (self.buf[byte] >> (7 - (self.pos % 8) as u32)) & 1
            } else {
                self.overrun = true;
                0
            };
            v = (v << 1) | u64::from(bit);
            self.pos += 1;
        }
        v
    }

    /// Bits left before reads start to overrun.
    pub fn remaining_bits(&self) -> u64 {
        (self.buf.len() as u64 * 8).saturating_sub(self.pos)
    }

    pub fn read_signed(&mut self, n: u32) -> i64 {
        let u = self.read(n);
        if n == 0 {
            return 0;
        }
        if n >= 64 {
            return u as i64;
        }
        if (u >> (n - 1)) & 1 == 1 {
            (u | !mask(n)) as i64
        } else {
            u as i64
        }
    }

    /// Count zero bits until a one. Err on overrun or a run over 2^32.
    pub fn read_unary(&mut self) -> Result<u32, String> {
        let mut q: u64 = 0;
        loop {
            if self.overrun {
                return Err("truncated payload".into());
            }
            let b = self.read(1);
            if self.overrun {
                return Err("truncated payload".into());
            }
            if b == 1 {
                return Ok(q as u32);
            }
            q += 1;
            if q > u64::from(u32::MAX) {
                return Err("unary run too long".into());
            }
        }
    }
}

// ------------------------------------------------------------ Rice residual

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum PartParam {
    Rice(u8),   // k
    Escape(u8), // raw width in bits, 0 = all zeros
    /// Quotient coded with an rANS adaptive-binary model; the k low bits
    /// of each zigzag value still go in raw.
    Rans(u8),
}

pub struct ResidualPlan {
    pub p: u8,
    pub params: Vec<PartParam>,
    /// Cell blobs for `params[i] == Rans` (parallel index); an empty slot
    /// means `write_residual` encodes that partition itself.
    pub rans_blobs: Vec<Vec<u16>>,
    pub cost: u64, // exact bits the encoder will emit
}

fn rice_cost(sum_q: u64, n_p: u64, k: u64) -> u64 {
    // payload: unary (u>>k zeros + stop) + k low bits per sample
    sum_q
        .saturating_add(n_p.saturating_mul(k + 1))
        .saturating_add(6)
}

fn escape_cost(n_p: u64, width: u64) -> u64 {
    if width > 63 {
        return u64::MAX / 4; // width field only holds 0..=63
    }
    n_p.saturating_mul(width).saturating_add(12) // 6 param + 6 width
}

/// rANS partition encoding: quotient bits as adaptive-unary (context per
/// run position, capped at 63), remainders raw. Returns the u16 cell blob
/// (payload); callers add 6 param + 16 length + k·n bits overhead.
fn rans_encode_part(s: &[i64], k: u8) -> Option<Vec<u16>> {
    rans_encode_cap(s, 1, k, 1, 1, u64::MAX)
}

/// Pass 1 — forward model walk: adapt the 64 contexts and record each
/// bit event as `(prob << 1) | bit` for the reverse encode. Returns None
/// on pathological quotients or when the cost model's floor alone can
/// no longer land under `cap` (cells only ever grow, so a capped encode
/// could never have finished cheaper).
fn rans_pass1(s: &[i64], stride: usize, k: u8, num: u64, den: u64, cap: u64) -> Option<Vec<u16>> {
    use crate::codec::prof;
    prof::bump(0, 1);
    let n = s.len().div_ceil(stride) as u64;
    // free check: the smallest possible blob is 3 cells (final state)
    if 22u64
        .saturating_add(48u64.saturating_mul(num) / den)
        .saturating_add(u64::from(k) * n)
        >= cap
    {
        prof::bump(1, 1);
        return None;
    }
    let t1 = prof::tick();
    let mut ctxs = [ProbBin::new(); 64];
    // packed stream: (prob << 1) | bit — one store per bit event;
    // bit-events run ~1.4x sample count — reserve 2x, skip the realloc.
    // Written through a raw pointer: per-sample capacity check once,
    // then plain stores (Vec::push re-checks + re-writes len per bit).
    let mut pb: Vec<u16> = Vec::with_capacity(2 * n as usize);
    let mut len = 0usize;
    let mut ptr = pb.as_mut_ptr();
    let mut vi = 0usize;
    while vi < s.len() {
        let q = zigzag(s[vi]) >> k;
        vi += stride;
        if q > 1 << 26 {
            return None; // pathological quotient — escape territory
        }
        // ctx index is min(j, 63): j < 64 each hit their own context,
        // the rest of the run all lands on ctx 63.
        let head = (q as usize).min(64);
        if len + head + 1 > pb.capacity() {
            // SAFETY: len only counts stores already made through ptr
            unsafe { pb.set_len(len) };
            pb.reserve(head + 64);
            ptr = pb.as_mut_ptr();
        }
        for j in 0..head {
            // SAFETY: j < 64 = ctxs.len(); len + head <= capacity checked
            unsafe {
                *ptr.add(len) = ctxs.get_unchecked_mut(j).step0() << 1;
                len += 1;
            }
        }
        if q > 64 {
            // rare path — sync len and let push handle growth
            unsafe { pb.set_len(len) };
            // SAFETY: 63 < ctxs.len()
            let cx = unsafe { ctxs.get_unchecked_mut(63) };
            for _ in 64..q {
                pb.push(cx.step0() << 1);
            }
            len = pb.len();
            // the tail pushes may have filled capacity exactly — the
            // initial len+head+1 guard does NOT cover this stop write
            if len + 1 > pb.capacity() {
                pb.reserve(64);
            }
            ptr = pb.as_mut_ptr();
        }
        let ci = (q as usize).min(63);
        // SAFETY: ci <= 63 = ctxs.len()-1; index len < capacity by the
        // head guard (q <= 64) or the re-check above (q > 64)
        unsafe {
            *ptr.add(len) = (ctxs.get_unchecked_mut(ci).step1() << 1) | 1;
            len += 1;
        }
    }
    // SAFETY: len counts exactly the stores made through ptr
    unsafe { pb.set_len(len) };
    prof::bump(2, pb.len() as u64);
    prof::add(12, t1);
    Some(pb)
}

/// Pass 2 — replay `pb` in reverse through the rANS state machine,
/// checking every 256 bits that the running cell count can still land
/// under `cap` (the cost model is `6 + 16 + 16·cells·num/den + k·n`).
/// `S` = `Vec<u16>` records the blob; `S` = `u64` just counts cells.
/// Returns the emitted cell count, or None when aborted.
fn rans_pass2<S: crate::rc::Cells>(
    pb: &[u16],
    enc: &mut RcEnc<S>,
    k: u8,
    n: u64,
    num: u64,
    den: u64,
    cap: u64,
) -> Option<u64> {
    use crate::codec::prof;
    let t2 = prof::tick();
    let mut i = pb.len();
    while i > 0 {
        let hi = i.saturating_sub(256);
        while i > hi {
            i -= 1;
            let v = pb[i];
            enc.put_prob(u32::from(v >> 1), (v & 1) as u8);
        }
        // running blob = emitted cells + the 3-cell final flush
        let cells = (enc.len() + 3) as u64;
        if cells > u64::from(u16::MAX)
            || 22u64
                .saturating_add(16u64.saturating_mul(cells).saturating_mul(num) / den)
                .saturating_add(u64::from(k) * n)
                >= cap
        {
            prof::add(13, t2);
            prof::bump(1, 1);
            return None;
        }
    }
    prof::add(13, t2);
    Some(enc.len() as u64 + 3)
}

/// Probe encode: like `rans_encode_cap` but only the cell count is
/// returned — the blob is never materialized (pass 2 counts emits).
fn rans_probe(s: &[i64], stride: usize, k: u8, num: u64, den: u64, cap: u64) -> Option<u64> {
    crate::codec::prof::time(5, || {
        let pb = rans_pass1(s, stride, k, num, den, cap)?;
        let mut enc = RcEnc::<u64>::counting();
        rans_pass2(&pb, &mut enc, k, s.len().div_ceil(stride) as u64, num, den, cap)
    })
}

/// `cap` aborts the encode (returns None) as soon as the emitted cell
/// count proves the cost model can no longer land under `cap`. Also
/// returns None on the wire's 65535-cell limit and on pathological
/// quotients, as before. `stride` subsamples `s` (1 = exact encode).
fn rans_encode_cap(s: &[i64], stride: usize, k: u8, num: u64, den: u64, cap: u64) -> Option<Vec<u16>> {
    crate::codec::prof::time(5, || {
        let pb = rans_pass1(s, stride, k, num, den, cap)?;
        let mut enc = RcEnc::with_capacity(512);
        let cells = rans_pass2(&pb, &mut enc, k, s.len().div_ceil(stride) as u64, num, den, cap)?;
        let blob = enc.finish();
        debug_assert_eq!(cells, blob.len() as u64);
        if blob.len() > u16::MAX as usize {
            return None;
        }
        Some(blob)
    })
}

/// AVX2 body of the plan_residual scan: for each fp_len chunk, computes
/// zigzag magnitudes, the chunk max, and per-k prefix accumulations in
/// u64 lanes (4 samples per ymm). Lane sums can't wrap (callers gate on
/// max_u <= 2^51), so wrapping lane adds equal the scalar saturating adds.
/// Tail samples inside a chunk take the scalar path.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn scan_chunks_avx2(
    e: &[i64],
    fp_len: usize,
    nk: usize,
    k_lo: u64,
    acc: &mut [u64; 9],
    fine_max: &mut [u64],
    cs: &mut [u64],
) {
    use std::arch::x86_64::*;
    unsafe {
        let mut pos = 0usize;
        for (fi, fm_out) in fine_max.iter_mut().enumerate() {
            let chunk = &e[pos..pos + fp_len];
            let mut vmax = _mm256_setzero_si256();
            let mut vacc = [_mm256_setzero_si256(); 9];
            let mut i = 0;
            while i + 4 <= fp_len {
                // SAFETY: i+4 <= fp_len = chunk.len()
                let v = _mm256_loadu_si256(chunk.as_ptr().add(i) as *const __m256i);
                // zigzag = (v << 1) ^ broadcast(sign): cmpgt(0, v) is the
                // all-ones sign mask (no srai_epi64 in AVX2)
                let sgn = _mm256_cmpgt_epi64(_mm256_setzero_si256(), v);
                let u = _mm256_xor_si256(_mm256_slli_epi64(v, 1), sgn);
                // u < 2^51 stays positive — signed cmpgt orders it fine
                vmax =
                    _mm256_blendv_epi8(vmax, u, _mm256_cmpgt_epi64(u, vmax));
                for (a, k) in vacc.iter_mut().take(nk).zip(k_lo..) {
                    *a = _mm256_add_epi64(*a, _mm256_srl_epi64(u, _mm_cvtsi64_si128(k as i64)));
                }
                i += 4;
            }
            // scalar tail (fp_len % 4 samples) — same ops as the scalar path
            let mut fm = 0u64;
            let mut tail = [0u64; 9];
            while i < fp_len {
                let u = zigzag(chunk[i]);
                fm = fm.max(u);
                let mut b = u >> k_lo;
                for t in tail.iter_mut().take(nk) {
                    *t = t.saturating_add(b);
                    b >>= 1;
                }
                i += 1;
            }
            let mut tmp = [0u64; 4];
            _mm256_storeu_si256(tmp.as_mut_ptr() as *mut __m256i, vmax);
            *fm_out = tmp.iter().fold(fm, |m, &x| m.max(x));
            for (ki, a) in vacc.iter().take(nk).enumerate() {
                _mm256_storeu_si256(tmp.as_mut_ptr() as *mut __m256i, *a);
                acc[ki] = acc
                    [ki]
                    .saturating_add(tmp[0] + tmp[1] + tmp[2] + tmp[3] + tail[ki]);
            }
            pos += fp_len;
            let base = (fi + 1) * nk;
            cs[base..base + nk].copy_from_slice(&acc[..nk]);
        }
    }
}

/// Plan a partitioned Rice encoding of `e` (raw signed residuals).
/// Cost accounting must equal `write_residual` output bit-for-bit.
pub fn plan_residual(e: &[i64]) -> ResidualPlan {
    plan_residual_cap(e, u64::MAX)
}

/// `cap` is the caller's incumbent cost: partition work that provably
/// cannot bring the plan under `cap` is skipped. A plan reported below
/// `cap` is identical to what `plan_residual` produces — the early-exit
/// bounds are rigorous, so a report >= `cap` may be returned after less
/// probing but still proves the true plan can't beat `cap` either.
pub fn plan_residual_cap(e: &[i64], cap: u64) -> ResidualPlan {
    crate::codec::prof::time(4, || plan_residual_inner(e, cap))
}

fn plan_residual_inner(e: &[i64], cap: u64) -> ResidualPlan {
    use std::sync::atomic::Ordering::Relaxed;
    crate::codec::prof::CNT[3].fetch_add(1, Relaxed);
    let n = e.len();
    if n == 0 {
        return ResidualPlan {
            p: 0,
            params: Vec::new(),
            rans_blobs: Vec::new(),
            cost: 0,
        };
    }
    // pass 1: mean of zigzag magnitudes picks the k window; the max also
    // gates the SIMD scan (lane sums must stay far from u64 wrap)
    let (total, max_u) = e.iter().fold((0u128, 0u64), |(t, m), &v| {
        let u = zigzag(v);
        (t + u128::from(u), m.max(u))
    });
    let mean = total / n as u128;
    let mean1 = (mean + 1).min(u64::MAX as u128) as u64;
    // per-partition k never strayed past +-4 of the global center on the
    // 47-file corpus (measured 0 size delta) — keep the narrow window
    let center = bit_len(mean1).saturating_sub(1);
    let k_lo = center.saturating_sub(4);
    let k_hi = (center + 4).min(30);
    let ks: Vec<u64> = (u64::from(k_lo)..=u64::from(k_hi)).collect();
    let nk = ks.len();

    // valid partition orders need n % 2^p == 0; finest = min(8, v2(n))
    let pf = (n.trailing_zeros() as u8).min(MAX_PART_ORDER);
    // per-finest-partition max u, merged upward for coarser p
    let nf = 1usize << pf;
    let fp_len = n >> pf;

    // Prefix sums of u>>k at chunk granularity only: every partition is a
    // run of whole fp_len chunks, so sum(s..t) = cs[t/fp_len]-cs[s/fp_len].
    // Layout: cs[chunk*nk + ki] — the 9 k-rows of one chunk sit in one
    // cache line pair, so a partition's column reads stay contiguous.
    // Scan runs as chunk-outer/sample-inner: no per-sample boundary test.
    let mut cs = vec![0u64; nk * (nf + 1)];
    let mut acc = [0u64; 9];
    let mut fine_max = vec![0u64; nf];
    // ks is a consecutive run (k_lo..=k_hi): u>>k for ascending k is a
    // halving chain — one variable shift + nk-1 fixed shift/adds.
    let k_lo64 = u64::from(k_lo);
    // AVX2 path needs lane sums that can never wrap u64: max_u <= 2^51
    // and <= 4096 lanes-worth per chunk puts the worst lane < 2^63, so
    // wrapping adds equal the scalar saturating adds bit-for-bit.
    #[cfg(target_arch = "x86_64")]
    let simd_scan =
        is_x86_feature_detected!("avx2") && max_u <= (1u64 << 51) && fp_len >= 8;
    #[cfg(not(target_arch = "x86_64"))]
    let simd_scan = false;
    crate::codec::prof::time(14, || {
        #[cfg(target_arch = "x86_64")]
        if simd_scan {
            // SAFETY: gated above; loads bounded by i+4 <= fp_len slices
            unsafe {
                scan_chunks_avx2(e, fp_len, nk, k_lo64, &mut acc, &mut fine_max, &mut cs);
            }
            return;
        }
        let mut pos = 0usize;
        for (fi, fmax) in fine_max.iter_mut().enumerate() {
            let mut fm = 0u64;
            for &v in &e[pos..pos + fp_len] {
                let u = zigzag(v);
                fm = fm.max(u);
                let mut b = u >> k_lo64;
                for a in acc.iter_mut().take(nk) {
                    *a = a.saturating_add(b);
                    b >>= 1;
                }
            }
            pos += fp_len;
            *fmax = fm;
            let base = (fi + 1) * nk;
            cs[base..base + nk].copy_from_slice(&acc[..nk]);
        }
    });

    // Per-level partition maxes in one flat buffer: level pf is fine_max;
    // each coarser level is the pairwise merge of the level below — one
    // lookup per partition. `lo[p]` = offset of level p's row.
    let mut lvl_max = vec![0u64; nf * 2];
    let mut lo = [0usize; MAX_PART_ORDER as usize + 1]; // lo[pf] = 0
    lvl_max[..nf].copy_from_slice(&fine_max);
    let mut off = nf;
    for p in (0..pf).rev() {
        lo[p as usize] = off;
        let cl = lo[p as usize + 1];
        for j in 0..(1usize << p) {
            lvl_max[off + j] = lvl_max[cl + 2 * j].max(lvl_max[cl + 2 * j + 1]);
        }
        off += 1 << p;
    }

    let mut best = ResidualPlan {
        p: 0,
        params: Vec::new(),
        rans_blobs: Vec::new(),
        cost: u64::MAX,
    };
    crate::codec::prof::time(15, || {
        for p in 0..=pf {
        let parts = 1usize << p;
        let n_p = (n >> p) as u64;
        let mut params = Vec::with_capacity(parts);
        let mut total = 4u64;
        // Level totals only grow per partition — once the running sum
        // can't beat the internal best, the rest of the level is dead.
        // NOT capped by the caller bound: a level above it could still
        // be selected and pulled under by the rANS pass. A truncated
        // level is discard-only (partial params would corrupt the plan).
        let bound = best.cost;
        let mut truncated = false;
        for j in 0..parts {
            if total >= bound {
                truncated = true;
                break;
            }
            let (s, t) = (j * (n_p as usize), (j + 1) * (n_p as usize));
            let max_u = lvl_max[lo[p as usize] + j];
            let width = u64::from(bit_len(max_u));
            let mut part = (escape_cost(n_p, width), PartParam::Escape(width as u8));
            let (sc, tc) = (s / fp_len * nk, t / fp_len * nk);
            for (ki, &k) in ks.iter().enumerate() {
                let sum_q = cs[tc + ki].saturating_sub(cs[sc + ki]);
                let c = rice_cost(sum_q, n_p, k);
                if c < part.0 {
                    part = (c, PartParam::Rice(k as u8));
                }
            }
            total = total.saturating_add(part.0);
            params.push(part.1);
        }
        if !truncated && total < best.cost {
            best = ResidualPlan {
                p,
                params,
                rans_blobs: vec![Vec::new(); parts],
                cost: total,
            };
        }
        }
    });

    // rANS pass on the winning level only: adaptive-unary quotient coding.
    // A strided probe decides, then one exact encode fixes the real cost.
    // rANS cells ride in a u16-length field, so blobs over 65535 cells are
    // not representable — those partitions keep Rice.
    let n_p = (n >> best.p) as u64;
    if best.cost != u64::MAX && n_p >= 64 {
        let n_pu = n_p as usize;
        let parts = best.params.len();
        // Per-partition rice cost `old` plus a rigorous lower bound on its
        // rANS cost (6+16 header, >= 3 cells, k_min·n remainder bits):
        // the suffix sum bounds how far the plan could still drop — once
        // it can't reach `cap`, every later probe is provably dead work.
        let mut old_v = vec![0u64; parts];
        let mut pot = vec![0u64; parts + 1];
        for j in 0..parts {
            if let PartParam::Rice(k0) = best.params[j] {
                let ki = (u64::from(k0) - u64::from(k_lo)) as usize;
                let sum_q = cs[(j + 1) * n_pu / fp_len * nk + ki]
                    .saturating_sub(cs[j * n_pu / fp_len * nk + ki]);
                old_v[j] = rice_cost(sum_q, n_p, u64::from(k0));
                let floor = 70u64.saturating_add(u64::from(k0.saturating_sub(1)) * n_p);
                pot[j] = old_v[j].saturating_sub(floor);
            }
        }
        for j in (0..parts).rev() {
            pot[j] = pot[j].saturating_add(pot[j + 1]);
        }
        for j in 0..parts {
            // remaining partitions can improve by at most pot[j] — if that
            // still leaves the plan at >= cap, the result is provably
            // discarded work: stop, the reported cost already proves it
            if best.cost.saturating_sub(pot[j]) >= cap {
                break;
            }
            let PartParam::Rice(k0) = best.params[j] else {
                continue;
            };
            let (s, t) = (j * n_pu, (j + 1) * n_pu);
            let old = old_v[j];
            let n_probe = n_pu / 2; // n_p is even (>= 64)
            // rANS optimal k can differ from the Rice pick — probe ±1;
            // the cap aborts probes that can no longer beat `old`
            let mut k_best = (k0, u64::MAX);
            for kc in [k0, k0.saturating_sub(1), (k0 + 1).min(30)] {
                // cap at the running best too — a probe that can no
                // longer win aborts mid-encode (decisions unchanged)
                let cap = old.min(k_best.1);
                let Some(cells) = rans_probe(&e[s..t], 2, kc, n_p, n_probe as u64, cap)
                else {
                    continue;
                };
                let est = 6u64
                    .saturating_add(16)
                    .saturating_add(16 * cells * n_p / n_probe as u64)
                    .saturating_add(u64::from(kc) * n_p);
                if est < k_best.1 {
                    k_best = (kc, est);
                }
            }
            let (k_sel, est) = k_best;
            if est == u64::MAX || est >= old {
                continue;
            }
            let Some(blob) = rans_encode_cap(&e[s..t], 1, k_sel, 1, 1, old) else {
                continue;
            };
            let exact = 6u64
                .saturating_add(16)
                .saturating_add(16 * blob.len() as u64)
                .saturating_add(u64::from(k_sel) * n_p);
            if exact < old {
                best.params[j] = PartParam::Rans(k_sel);
                best.rans_blobs[j] = blob;
                best.cost = best.cost.saturating_sub(old).saturating_add(exact);
            }
        }
    }
    best
}

/// Write `e` per `plan`; emits exactly `plan.cost` bits.
pub fn write_residual(w: &mut BitWriter, e: &[i64], plan: &ResidualPlan) {
    let n = e.len();
    if n == 0 {
        return;
    }
    w.write(u64::from(plan.p), 4);
    let n_p = n >> plan.p;
    for (j, &param) in plan.params.iter().enumerate() {
        let s = &e[j * n_p..(j + 1) * n_p];
        match param {
            PartParam::Rice(k) => {
                w.write(u64::from(k), 6);
                for &v in s {
                    let u = zigzag(v);
                    w.write_unary(u >> k);
                    w.write(u & mask(u32::from(k)), u32::from(k));
                }
            }
            PartParam::Escape(width) => {
                w.write(62, 6);
                w.write(u64::from(width), 6);
                for &v in s {
                    w.write(zigzag(v) & mask(u32::from(width)), u32::from(width));
                }
            }
            PartParam::Rans(k) => {
                // [param u6][blob_len u16][cells][n_p remainders of k bits]
                let owned;
                let blob: &[u16] = match plan.rans_blobs.get(j) {
                    Some(b) if !b.is_empty() => b,
                    _ => {
                        owned = rans_encode_part(s, k).expect("planned rANS encodes");
                        &owned
                    }
                };
                w.write(31 + u64::from(k), 6);
                w.write(blob.len() as u64, 16);
                for &c in blob {
                    w.write(u64::from(c), 16);
                }
                for &v in s {
                    w.write(zigzag(v) & mask(u32::from(k)), u32::from(k));
                }
            }
        }
    }
}

/// Read `n` zigzag residual values.
pub fn read_residual(r: &mut BitReader<'_>, n: usize) -> Result<Vec<i64>, String> {
    if n == 0 {
        return Ok(Vec::new());
    }
    let p = r.read(4) as u8;
    if p > MAX_PART_ORDER {
        return Err("partition order out of range".into());
    }
    let parts = 1usize << p;
    if n % parts != 0 {
        return Err("partition order does not divide residual length".into());
    }
    let n_p = n >> p;
    let mut out = vec![0i64; n];
    for j in 0..parts {
        let s = &mut out[j * n_p..(j + 1) * n_p];
        let param = r.read(6);
        match param {
            0..=30 => {
                let k = param as u32;
                for v in s.iter_mut() {
                    let q = r.read_unary()?;
                    let u = (u64::from(q) << k) | r.read(k);
                    *v = unzigzag(u);
                }
            }
            62 => {
                let width = r.read(6) as u32;
                if width > 63 {
                    return Err("escape width out of range".into());
                }
                for v in s.iter_mut() {
                    *v = unzigzag(r.read(width));
                }
            }
            31..=61 => {
                let k = (param - 31) as u32;
                let ncells = r.read(16) as usize;
                let mut cells = Vec::with_capacity(ncells);
                for _ in 0..ncells {
                    cells.push(r.read(16) as u16);
                }
                if r.overrun {
                    return Err("truncated payload".into());
                }
                let mut ctxs = Vec::new();
                ctxs.resize(64, crate::rc::ProbBin::new());
                let mut dec = crate::rc::RcDec::new(&cells)?;
                // A 0-bit costs >= -log2(1 - 4/4096) blob bits, so a legit
                // stream yields < ~11372 zeros per cell; 16384/cell is the
                // hard budget (exhausted cells would otherwise loop ~2^26
                // per sample on a 3-cell blob).
                let mut budget = (cells.len() as u64) << 14;
                for v in s.iter_mut() {
                    // unary quotient: 0-bits until the stop bit, context per
                    // run position capped at 63
                    let mut q: u64 = 0;
                    loop {
                        let bit = dec.get(&mut ctxs[(q as usize).min(63)]);
                        if bit == 1 {
                            break;
                        }
                        q += 1;
                        if budget == 0 {
                            return Err("rANS unary budget exceeded".into());
                        }
                        budget -= 1;
                        // encoder-side cap in rans_encode_part is 1<<26
                        if q > 1 << 26 {
                            return Err("unary run too long".into());
                        }
                    }
                    *v = unzigzag(q << k | r.read(k));
                }
            }
            _ => return Err("partition param out of range".into()),
        }
    }
    if r.overrun {
        return Err("truncated payload".into());
    }
    Ok(out)
}

// -------------------------------------------------------------------- CRC32

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC_TABLE: [u32; 256] = crc_table();

pub fn crc32(data: &[u8]) -> u32 {
    let mut c = Crc32::new();
    c.update(data);
    c.finish()
}

/// Streaming CRC32 — `finish` equals `crc32` over the concatenated updates.
pub struct Crc32(u32);

impl Crc32 {
    pub fn new() -> Self {
        Self(0xFFFF_FFFF)
    }
    pub fn update(&mut self, data: &[u8]) {
        for &b in data {
            self.0 = CRC_TABLE[((self.0 ^ u32::from(b)) & 0xFF) as usize] ^ (self.0 >> 8);
        }
    }
    pub fn finish(self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
    }

    fn roundtrip(e: &[i64]) -> (Vec<i64>, u64) {
        let plan = plan_residual(e);
        let mut w = BitWriter::new();
        write_residual(&mut w, e, &plan);
        assert_eq!(w.bits, plan.cost, "planner cost must equal written bits");
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let back = read_residual(&mut r, e.len()).unwrap();
        (back, plan.cost)
    }

    #[test]
    fn bit_io_roundtrip() {
        let mut w = BitWriter::new();
        w.write(0, 0);
        w.write(1, 1);
        w.write(0b1011, 4);
        w.write(u64::MAX, 64);
        w.write_signed(-1, 4);
        w.write_signed(-8, 4);
        w.write_signed(7, 4);
        w.write_signed(-2, 64);
        w.write_unary(0);
        w.write_unary(5);
        w.write(0xAB, 8);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read(1), 1);
        assert_eq!(r.read(4), 0b1011);
        assert_eq!(r.read(64), u64::MAX);
        assert_eq!(r.read_signed(4), -1);
        assert_eq!(r.read_signed(4), -8);
        assert_eq!(r.read_signed(4), 7);
        assert_eq!(r.read_signed(64), -2);
        assert_eq!(r.read_unary().unwrap(), 0);
        assert_eq!(r.read_unary().unwrap(), 5);
        assert_eq!(r.read(8), 0xAB);
        assert!(!r.overrun);
        let _ = r.read(4);
        assert!(r.overrun, "reading past end sets overrun");
    }

    #[test]
    fn rice_all_zero() {
        let e = vec![0i64; 4096];
        let (back, cost) = roundtrip(&e);
        assert_eq!(back, e);
        assert!(
            cost <= 64,
            "all-zero residual cost {cost} must be <= 64 bits"
        );
    }

    #[test]
    fn rice_laplacian() {
        let mut rng = Lcg(0x9E3779B97F4A7C15);
        // Laplacian-like: mostly small, occasional large
        let e: Vec<i64> = (0..4096)
            .map(|_| {
                let v = rng.next();
                let mag = (v % 50) as i64 - 25;
                if v % 97 == 0 { mag * 4000 } else { mag }
            })
            .collect();
        let (back, _) = roundtrip(&e);
        assert_eq!(back, e);
    }

    #[test]
    fn rice_extremes() {
        let mut rng = Lcg(42);
        let e: Vec<i64> = (0..512)
            .map(|_| {
                let s = if rng.next() & 1 == 0 { 1 } else { -1 };
                s * (1i64 << 40) * (1 + (rng.next() % 100) as i64)
            })
            .collect();
        let (back, _) = roundtrip(&e);
        assert_eq!(back, e);
    }

    #[test]
    fn rice_small_n() {
        for n in [1usize, 1000] {
            let mut rng = Lcg(n as u64);
            let e: Vec<i64> = (0..n).map(|_| (rng.next() % 200) as i64 - 100).collect();
            let (back, _) = roundtrip(&e);
            assert_eq!(back, e);
        }
    }

    #[test]
    fn rice_truncated_err() {
        let e = vec![7i64; 64];
        let plan = plan_residual(&e);
        let mut w = BitWriter::new();
        write_residual(&mut w, &e, &plan);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes[..bytes.len() - 1]);
        assert!(read_residual(&mut r, 64).is_err());
    }

    #[test]
    fn crc32_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }
}
