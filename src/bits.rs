//! Bit-level IO (MSB-first), partitioned Rice coding with escapes, CRC32.

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
    acc: u64,   // bits are staged MSB-first in the low `nb` bits of acc
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
        Self { buf, pos: 0, overrun: false }
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
}

pub struct ResidualPlan {
    pub p: u8,
    pub params: Vec<PartParam>,
    pub cost: u64, // exact bits the encoder will emit
}

fn rice_cost(sum_q: u64, n_p: u64, k: u64) -> u64 {
    // payload: unary (u>>k zeros + stop) + k low bits per sample
    sum_q
        .saturating_add(n_p.saturating_mul(k + 1))
        .saturating_add(5)
}

fn escape_cost(n_p: u64, width: u64) -> u64 {
    if width > 63 {
        return u64::MAX / 4; // width field only holds 0..=63
    }
    n_p.saturating_mul(width).saturating_add(11) // 5 param + 6 width
}

/// Plan a partitioned Rice encoding of `e` (raw signed residuals).
/// Cost accounting must equal `write_residual` output bit-for-bit.
pub fn plan_residual(e: &[i64]) -> ResidualPlan {
    let n = e.len();
    if n == 0 {
        return ResidualPlan { p: 0, params: Vec::new(), cost: 0 };
    }
    let u: Vec<u64> = e.iter().map(|&v| zigzag(v)).collect();
    let mean = u.iter().fold(0u128, |a, &v| a + v as u128) / n as u128;
    let mean1 = (mean + 1).min(u64::MAX as u128) as u64;
    let center = bit_len(mean1).saturating_sub(1);
    let k_lo = center.saturating_sub(4);
    let k_hi = (center + 4).min(30);
    let ks: Vec<u64> = (u64::from(k_lo)..=u64::from(k_hi)).collect();

    // prefix sums of u>>k for every k in the window
    let ps: Vec<Vec<u64>> = ks
        .iter()
        .map(|&k| {
            let mut acc = vec![0u64; n + 1];
            for i in 0..n {
                acc[i + 1] = acc[i].saturating_add(u[i] >> k);
            }
            acc
        })
        .collect();

    // valid partition orders need n % 2^p == 0; finest = min(8, v2(n))
    let pf = (n.trailing_zeros() as u8).min(MAX_PART_ORDER);
    // per-finest-partition max u, merged upward for coarser p
    let nf = 1usize << pf;
    let fp_len = n >> pf;
    let mut fine_max = vec![0u64; nf];
    for (j, m) in fine_max.iter_mut().enumerate() {
        *m = u[j * fp_len..(j + 1) * fp_len].iter().fold(0, |a, &v| a.max(v));
    }

    let mut best = ResidualPlan { p: 0, params: Vec::new(), cost: u64::MAX };
    for p in 0..=pf {
        let parts = 1usize << p;
        let n_p = (n >> p) as u64;
        let span = nf >> p; // finest partitions per partition at level p
        let mut params = Vec::with_capacity(parts);
        let mut total = 4u64;
        for j in 0..parts {
            let (s, t) = (j * (n_p as usize), (j + 1) * (n_p as usize));
            let max_u = fine_max[j * span..(j + 1) * span]
                .iter()
                .fold(0, |a, &v| a.max(v));
            let width = u64::from(bit_len(max_u));
            let mut part = (escape_cost(n_p, width), PartParam::Escape(width as u8));
            for (ki, &k) in ks.iter().enumerate() {
                let sum_q = ps[ki][t].saturating_sub(ps[ki][s]);
                let c = rice_cost(sum_q, n_p, k);
                if c < part.0 {
                    part = (c, PartParam::Rice(k as u8));
                }
            }
            total = total.saturating_add(part.0);
            params.push(part.1);
        }
        if total < best.cost {
            best = ResidualPlan { p, params, cost: total };
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
                w.write(u64::from(k), 5);
                for &v in s {
                    let u = zigzag(v);
                    w.write_unary(u >> k);
                    w.write(u & mask(u32::from(k)), u32::from(k));
                }
            }
            PartParam::Escape(width) => {
                w.write(31, 5);
                w.write(u64::from(width), 6);
                for &v in s {
                    w.write(zigzag(v) & mask(u32::from(width)), u32::from(width));
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
    if !n.is_multiple_of(parts) {
        return Err("partition order does not divide residual length".into());
    }
    let n_p = n >> p;
    let mut out = vec![0i64; n];
    for j in 0..parts {
        let s = &mut out[j * n_p..(j + 1) * n_p];
        let param = r.read(5);
        if param == 31 {
            let width = r.read(6) as u32;
            if width > 63 {
                return Err("escape width out of range".into());
            }
            for v in s.iter_mut() {
                *v = unzigzag(r.read(width));
            }
        } else {
            let k = param as u32;
            for v in s.iter_mut() {
                let q = r.read_unary()?;
                let u = (u64::from(q) << k) | r.read(k);
                *v = unzigzag(u);
            }
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
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC_TABLE: [u32; 256] = crc_table();

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = CRC_TABLE[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
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
        assert!(cost <= 64, "all-zero residual cost {cost} must be <= 64 bits");
    }

    #[test]
    fn rice_laplacian() {
        let mut rng = Lcg(0x9E3779B97F4A7C15);
        // Laplacian-like: mostly small, occasional large
        let e: Vec<i64> = (0..4096)
            .map(|_| {
                let v = rng.next();
                let mag = (v % 50) as i64 - 25;
                if v.is_multiple_of(97) { mag * 4000 } else { mag }
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
