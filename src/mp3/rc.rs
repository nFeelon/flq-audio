//! LZMA-style binary range coder: carry propagation via `low`/`cache`,
//! 16-bit probabilities, adaptive bit models. Integer-only coding path;
//! -log2(p) per-category costs are float and used for stats only.

pub const LIMIT: u8 = 127; // adaptive rate cap; best of {20,40,127} on real files
const TOP: u32 = 1 << 24;
pub const P_CLAMP_LO: u32 = 64;
pub const P_CLAMP_HI: u32 = 65536 - 64;

// RECIP[n] = round(65536 / (n + 1.5)) = round(131072 / (2n + 3))
const fn build_recip() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut n = 0u64;
    while n < 256 {
        let d = 2 * n + 3;
        t[n as usize] = ((262144 + d) / (2 * d)) as u16;
        n += 1;
    }
    t
}
static RECIP: [u16; 256] = build_recip();

/// Adaptive bit model: p = P(bit = 1) as u16, n = update count (capped).
#[derive(Clone, Copy)]
pub struct Bit {
    p: u16,
    n: u8,
}

impl Bit {
    pub const fn new() -> Self {
        Bit { p: 32768, n: 0 }
    }
    pub fn prob(&self) -> u32 {
        u32::from(self.p).clamp(P_CLAMP_LO, P_CLAMP_HI)
    }
    pub fn update(&mut self, b: u32, limit: u8) {
        let r = u32::from(RECIP[(self.n as usize).min(255)]);
        let d = if b != 0 { 65536i64 } else { 0 } - i64::from(self.p);
        self.p = (i64::from(self.p) + ((d * i64::from(r)) >> 16)).clamp(0, 65535) as u16;
        if self.n < limit {
            self.n += 1;
        }
    }
}

pub struct Encoder {
    low: u64,
    range: u32,
    cache: u8,
    cache_size: u64,
    out: Vec<u8>,
    limit: u8,
    /// -log2(p) ideal cost per category (stats only; index < 16).
    pub costs: [f64; 16],
}

impl Encoder {
    pub fn new(limit: u8) -> Self {
        Encoder {
            low: 0,
            range: 0xFFFF_FFFF,
            cache: 0,
            cache_size: 1,
            out: Vec::new(),
            limit,
            costs: [0.0; 16],
        }
    }

    fn shift_low(&mut self) {
        if self.low < 0xFF00_0000 || self.low > 0xFFFF_FFFF {
            let carry = (self.low >> 32) as u8;
            let mut temp = self.cache;
            loop {
                self.out.push(temp.wrapping_add(carry));
                temp = 0xFF;
                self.cache_size -= 1;
                if self.cache_size == 0 {
                    break;
                }
            }
            self.cache = (self.low >> 24) as u8;
        }
        self.cache_size += 1;
        self.low = (self.low << 8) & 0xFFFF_FFFF;
    }

    /// -log2(p) lookup for stats, indexed by p12 (p16 >> 4). Stats only.
    fn cost_tab() -> &'static [f64; 4096] {
        static T: std::sync::OnceLock<[f64; 4096]> = std::sync::OnceLock::new();
        T.get_or_init(|| {
            let mut t = [0.0f64; 4096];
            for (i, v) in t.iter_mut().enumerate() {
                *v = -(((i as f64) + 0.5) / 4096.0).log2();
            }
            t
        })
    }

    /// Encode bit `b` (0/1) with probability p16 = P(1); p is clamped so
    /// callers can never produce a zero range.
    pub fn bit_p(&mut self, b: u32, p: u32, cat: usize) {
        let p = p.clamp(P_CLAMP_LO, P_CLAMP_HI);
        let bound = (self.range >> 16).wrapping_mul(p);
        if b != 0 {
            self.range = bound;
            self.costs[cat] += Self::cost_tab()[(p >> 4) as usize];
        } else {
            self.low += u64::from(bound);
            self.range = self.range.wrapping_sub(bound);
            self.costs[cat] += Self::cost_tab()[((65536 - p) >> 4) as usize];
        }
        while self.range < TOP {
            self.range <<= 8;
            self.shift_low();
        }
    }

    /// Encode a bit through an adaptive context.
    pub fn bit(&mut self, b: u32, ctx: &mut Bit, cat: usize) {
        let p = ctx.prob();
        self.bit_p(b, p, cat);
        ctx.update(b, self.limit);
    }

    /// `n` raw bits, p = 1/2 each (MSB-first).
    pub fn direct(&mut self, v: u64, n: u32, cat: usize) {
        for i in (0..n).rev() {
            let b = ((v >> i) & 1) as u32;
            let bound = self.range >> 1;
            if b != 0 {
                self.low += u64::from(bound);
                self.range = self.range.wrapping_sub(bound);
            } else {
                self.range = bound;
            }
            self.costs[cat] += 1.0;
            while self.range < TOP {
                self.range <<= 8;
                self.shift_low();
            }
        }
    }

    /// Bit-tree: n MSB-first bits of `v` through `ctx` (len >= 2^n).
    pub fn tree(&mut self, v: u32, n: u32, ctx: &mut [Bit], cat: usize) {
        let mut idx = 1usize;
        for i in (0..n).rev() {
            let b = ((v >> i) & 1) as usize;
            self.bit(b as u32, &mut ctx[idx], cat);
            idx = (idx << 1) | b;
        }
    }

    pub fn finish(mut self) -> Vec<u8> {
        for _ in 0..5 {
            self.shift_low();
        }
        self.out
    }
}

pub struct Decoder<'a> {
    code: u32,
    range: u32,
    data: &'a [u8],
    pos: usize,
    limit: u8,
    pub overrun: bool,
}

impl<'a> Decoder<'a> {
    pub fn new(data: &'a [u8], limit: u8) -> Self {
        let mut d = Decoder {
            code: 0,
            range: 0xFFFF_FFFF,
            data,
            pos: 0,
            limit,
            overrun: false,
        };
        for i in 0..5 {
            let b = d.next_byte();
            if i > 0 {
                d.code = (d.code << 8) | b;
            }
        }
        d
    }

    fn next_byte(&mut self) -> u32 {
        let b = self.data.get(self.pos).copied();
        if b.is_none() {
            self.overrun = true;
        }
        self.pos += 1;
        u32::from(b.unwrap_or(0))
    }

    pub fn bit_p(&mut self, p: u32) -> u32 {
        let p = p.clamp(P_CLAMP_LO, P_CLAMP_HI);
        let bound = (self.range >> 16).wrapping_mul(p);
        let b = if self.code < bound {
            self.range = bound;
            1
        } else {
            self.code = self.code.wrapping_sub(bound);
            self.range = self.range.wrapping_sub(bound);
            0
        };
        while self.range < TOP {
            self.range <<= 8;
            self.code = (self.code << 8) | self.next_byte();
        }
        b
    }

    pub fn bit(&mut self, ctx: &mut Bit) -> u32 {
        let p = ctx.prob();
        let b = self.bit_p(p);
        ctx.update(b, self.limit);
        b
    }

    pub fn direct(&mut self, n: u32) -> u64 {
        let mut v = 0u64;
        for _ in 0..n {
            let bound = self.range >> 1;
            let b: u32 = if self.code < bound {
                self.range = bound;
                0
            } else {
                self.code = self.code.wrapping_sub(bound);
                self.range = self.range.wrapping_sub(bound);
                1
            };
            v = (v << 1) | u64::from(b);
            while self.range < TOP {
                self.range <<= 8;
                self.code = (self.code << 8) | self.next_byte();
            }
        }
        v
    }

    /// Inverse of `Encoder::tree`; returns n bits MSB-first.
    pub fn tree(&mut self, n: u32, ctx: &mut [Bit]) -> u32 {
        let mut idx = 1usize;
        loop {
            let b = self.bit(&mut ctx[idx]) as usize;
            idx = (idx << 1) | b;
            if idx >= (1usize << n) {
                return (idx - (1 << n)) as u32;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(s: &mut u64) -> u64 {
        *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *s >> 33
    }

    fn roundtrip(bits: &[u32], ctxs_same: bool) {
        let mut e = Encoder::new(LIMIT);
        let mut c = vec![Bit::new(); 64];
        for (i, &b) in bits.iter().enumerate() {
            let k = if ctxs_same { i % 64 } else { 0 };
            e.bit(b, &mut c[k], 0);
        }
        let data = e.finish();
        let mut d = Decoder::new(&data, LIMIT);
        let mut c2 = vec![Bit::new(); 64];
        for (i, &b) in bits.iter().enumerate() {
            let k = if ctxs_same { i % 64 } else { 0 };
            assert_eq!(d.bit(&mut c2[k]), b, "bit {i}");
        }
        assert!(!d.overrun);
    }

    #[test]
    fn skewed_and_adaptive() {
        let mut s = 0x1234u64;
        for skew in [2usize, 8, 50, 95, 99] {
            // bits drawn with rough probability skew% of being 1
            let bits: Vec<u32> = (0..3000)
                .map(|_| u32::from((lcg(&mut s) % 100) < skew as u64))
                .collect();
            roundtrip(&bits, true);
        }
        // alternating + runs
        let mut v = Vec::new();
        for i in 0..3000 {
            v.push((i % 97 < 3) as u32);
        }
        roundtrip(&v, false);
    }

    #[test]
    fn carry_heavy() {
        // p clamped to near 0/1 for long runs -> carry chains
        let mut e = Encoder::new(LIMIT);
        let mut c = Bit::new();
        for _ in 0..3000 {
            e.bit(1, &mut c, 0);
        }
        for _ in 0..3000 {
            e.bit(0, &mut c, 0);
        }
        for i in 0..5000 {
            e.bit(i % 2, &mut c, 0);
        }
        let data = e.finish();
        let mut d = Decoder::new(&data, LIMIT);
        let mut c = Bit::new();
        for _ in 0..3000 {
            assert_eq!(d.bit(&mut c), 1);
        }
        for _ in 0..3000 {
            assert_eq!(d.bit(&mut c), 0);
        }
        for i in 0..5000 {
            assert_eq!(d.bit(&mut c), i % 2);
        }
    }

    #[test]
    fn direct_bits() {
        let mut s = 77u64;
        let mut e = Encoder::new(LIMIT);
        let mut c = Bit::new();
        let vals: Vec<u64> = (0..200).map(|_| lcg(&mut s) % 0x1_FFFF).collect();
        e.bit(1, &mut c, 0);
        for &v in &vals {
            e.direct(v, 17, 1);
        }
        let data = e.finish();
        let mut d = Decoder::new(&data, LIMIT);
        let mut c = Bit::new();
        assert_eq!(d.bit(&mut c), 1);
        for &v in &vals {
            assert_eq!(d.direct(17), v);
        }
    }

    #[test]
    fn empty_and_short() {
        let e = Encoder::new(LIMIT);
        let data = e.finish();
        assert_eq!(data.len(), 5);
        let mut d = Decoder::new(&data, LIMIT);
        let _ = d.direct(64); // p=1/2 shrinks range -> reads past end, no panic
        assert!(d.overrun);
        // empty input decode
        let mut d = Decoder::new(&[], LIMIT);
        assert_eq!(d.direct(8), 0);
        assert!(d.overrun);
    }

    #[test]
    fn extreme_probs() {
        // raw bit_p must clamp out-of-range p (e.g. mixer saturation) so the
        // encoder can never stall on range == 0
        for &p in &[0u32, 1, 63, 64, 32768, 65472, 65535, 65536] {
            for b in [0u32, 1] {
                let mut e = Encoder::new(LIMIT);
                for _ in 0..64 {
                    e.bit_p(b, p, 0);
                }
                let data = e.finish();
                let mut d = Decoder::new(&data, LIMIT);
                for _ in 0..64 {
                    assert_eq!(d.bit_p(p), b, "p={p} b={b}");
                }
            }
        }
    }

    #[test]
    fn recip_monotone() {
        assert_eq!(RECIP[0], 43691);
        for i in 1..256 {
            assert!(RECIP[i] < RECIP[i - 1] || i > 250);
        }
    }
}
