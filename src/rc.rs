//! rANS entropy coder over an adaptive binary alphabet: the quotient bits
//! of a Rice decomposition are coded with per-position adaptive
//! probabilities instead of a fixed unary code.
//!
//! Renormalization is in u16 cells (Giesen-style):
//! encode: emit while `x >= p * X_MAX_F`, decode: pull while `x < L`.
//! LIFO discipline: bits are fed to `RcEnc` in REVERSE order with the
//! probability recorded for each position, so the caller first makes a
//! forward pass adapting the model and recording `p1[i]`, then encodes
//! backward; `RcDec` yields bits forward re-adapting identically.

/// Probability quantum: adaptive probs are multiples of 1/4096.
pub const TOT: u32 = 4096; // 1 << 12
const L: u64 = 1 << 24;
/// x_max factor: emit while `x >= p * (1 << 28)`.
const XMAX_SHIFT: u32 = 28;

/// Per-context adaptive probability of a 1-bit (kept 4..TOT-4).
#[derive(Clone)]
pub struct ProbBin {
    p: u16, // P(bit=1) * TOT
}

impl ProbBin {
    pub fn new() -> Self {
        Self { p: TOT as u16 / 2 }
    }
    #[inline]
    pub fn get(&self) -> u32 {
        u32::from(self.p)
    }
    /// Shift-register adaptation (rate 1/16).
    #[inline]
    pub fn adapt(&mut self, bit: u8) {
        let p = i32::from(self.p);
        let target = if bit == 1 { TOT as i32 } else { 0 };
        self.p = (p + ((target - p) >> 4)).clamp(4, TOT as i32 - 4) as u16;
    }
}

/// rANS encoder: `put_prob` appends one bit coded with probability `p1`
/// (P(bit=1)·TOT) — feed bits in reverse order with the probabilities a
/// forward model pass recorded.
pub struct RcEnc {
    x: u64,
    out: Vec<u16>,
}

impl Default for RcEnc {
    fn default() -> Self {
        Self::new()
    }
}

impl RcEnc {
    pub fn new() -> Self {
        Self {
            x: L,
            out: Vec::new(),
        }
    }

    #[inline]
    pub fn put_prob(&mut self, p1: u32, bit: u8) {
        let (f, cum) = if bit == 1 {
            (p1, TOT - p1)
        } else {
            (TOT - p1, 0)
        };
        let xmax = u64::from(f) << XMAX_SHIFT;
        while self.x >= xmax {
            self.out.push((self.x & 0xFFFF) as u16);
            self.x >>= 16;
        }
        self.x = (self.x / u64::from(f)) * u64::from(TOT) + u64::from(cum) + self.x % u64::from(f);
    }

    /// Flush: append the final state (3 u16 cells cover x < 2^44).
    pub fn finish(mut self) -> Vec<u16> {
        for _ in 0..3 {
            self.out.push((self.x & 0xFFFF) as u16);
            self.x >>= 16;
        }
        self.out
    }
}

/// rANS decoder for blobs produced by `RcEnc::finish`.
///
/// The payload cells are a stack: pulled last-emitted first, i.e.
/// backwards from `cells[..len-3]`.
pub struct RcDec<'a> {
    x: u64,
    cells: &'a [u16],
    pos: usize, // pulls cells[pos-1]
}

impl<'a> RcDec<'a> {
    /// `cells` = the encoder's blob; the last 3 cells are the flushed state.
    pub fn new(cells: &'a [u16]) -> Result<Self, String> {
        if cells.len() < 3 {
            return Err("rANS blob too short".into());
        }
        let n = cells.len();
        let mut x = 0u64;
        // encoder flushed lo->hi: cells[n-3]=x[15:0] .. [n-1]=x[47:32]
        for i in 0..3 {
            x |= u64::from(cells[n - 3 + i]) << (16 * i);
        }
        Ok(Self {
            x,
            cells,
            pos: n - 3,
        })
    }

    /// Decode one bit against the caller's live context `pb` (adapts it).
    /// The pull guard mirrors the encoder's state range: post-decode x is
    /// always >= L unless cells were emitted for this symbol.
    #[inline]
    pub fn get(&mut self, pb: &mut ProbBin) -> u8 {
        let p1 = pb.get();
        let c = (self.x % u64::from(TOT)) as u32;
        let bit = (c >= TOT - p1) as u8;
        let (f, cum) = if bit == 1 {
            (p1, TOT - p1)
        } else {
            (TOT - p1, 0)
        };
        self.x = u64::from(f) * (self.x / u64::from(TOT)) + u64::from(c) - u64::from(cum);
        while self.x < L && self.pos > 0 {
            self.pos -= 1;
            self.x = (self.x << 16) | u64::from(self.cells[self.pos]);
        }
        pb.adapt(bit);
        bit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rans_roundtrip() {
        let mut st = 0x9E3779B97F4A7C15u64;
        let mut rng = move || {
            st = st.wrapping_mul(6364136223846793005).wrapping_add(1);
            (st >> 33) as u32
        };
        for n in [0usize, 1, 7, 1000, 50_000] {
            let bits: Vec<u8> = (0..n).map(|_| (rng() % 3 == 0) as u8).collect();
            let ctx = |i: usize| (i + 1).min(63);
            // pass 1 (forward): adapt contexts, record per-bit probabilities
            let mut ctxs = vec![ProbBin::new(); 64];
            let probs: Vec<u16> = bits
                .iter()
                .enumerate()
                .map(|(i, &b)| {
                    let p = ctxs[ctx(i)].get() as u16;
                    ctxs[ctx(i)].adapt(b);
                    p
                })
                .collect();
            // pass 2 (reverse): encode with recorded probs
            let mut enc = RcEnc::new();
            for (i, &b) in bits.iter().enumerate().rev() {
                enc.put_prob(u32::from(probs[i]), b);
            }
            let blob = enc.finish();
            // decode forward
            let mut ctxs = vec![ProbBin::new(); 64];
            let mut dec = RcDec::new(&blob).unwrap();
            for i in 0..n {
                assert_eq!(dec.get(&mut ctxs[ctx(i)]), bits[i], "n={n} bit {i}");
            }
        }
    }
}
