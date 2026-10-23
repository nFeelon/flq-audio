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

/// Exact floor(x/d) via multiply-high for d < TOT, x < 2^52:
/// m = ceil(2^64/d) ⇒ m·d − 2^64 < d, and x·(m·d−2^64)/d < 1/d·x < 2^52·…
/// suffices to never cross an integer boundary. Compiled table.
const fn magics() -> [u64; TOT as usize] {
    let mut t = [0u64; TOT as usize];
    let mut d = 1usize;
    while d < TOT as usize {
        t[d] = (1u128 << 64).div_ceil(d as u128) as u64;
        d += 1;
    }
    t
}
static MAGIC: [u64; TOT as usize] = magics();

#[inline]
fn mulhi(x: u64, m: u64) -> u64 {
    ((x as u128 * m as u128) >> 64) as u64
}

/// `adapt` as a transition table: ADAPT[bit][p] = next p. 16 KiB,
/// L1-resident; identical to the shift-register formula, one load per bit.
const fn adapt_tab(bit: u32) -> [u16; TOT as usize] {
    let mut t = [0u16; TOT as usize];
    let mut p = 0usize;
    while p < TOT as usize {
        let pi = p as i32;
        let v = pi + ((bit as i32 * TOT as i32 - pi) >> 4);
        let v = if v < 4 {
            4
        } else if v > TOT as i32 - 4 {
            TOT as i32 - 4
        } else {
            v
        };
        t[p] = v as u16;
        p += 1;
    }
    t
}
static ADAPT0: [u16; TOT as usize] = adapt_tab(0);
static ADAPT1: [u16; TOT as usize] = adapt_tab(1);

/// Per-context adaptive probability of a 1-bit (kept 4..TOT-4).
#[derive(Clone, Copy)]
pub struct ProbBin {
    p: u16, // P(bit=1) * TOT
}

impl ProbBin {
    pub const fn new() -> Self {
        Self { p: TOT as u16 / 2 }
    }
    #[inline]
    pub fn get(&self) -> u32 {
        u32::from(self.p)
    }
    /// Shift-register adaptation (rate 1/16).
    #[inline]
    pub fn adapt(&mut self, bit: u8) {
        self.p = if bit == 1 {
            ADAPT1[self.p as usize]
        } else {
            ADAPT0[self.p as usize]
        };
    }
    /// One fused get+adapt step: returns the current prob, advances on a
    /// 0-bit. Hot encoder path — one load + one store instead of two.
    #[inline]
    pub fn step0(&mut self) -> u16 {
        let p = self.p;
        self.p = ADAPT0[p as usize];
        p
    }
    #[inline]
    pub fn step1(&mut self) -> u16 {
        let p = self.p;
        self.p = ADAPT1[p as usize];
        p
    }
}

/// Cell sink for `RcEnc`: `Vec<u16>` keeps the blob (exact encodes),
/// `u64` just counts cells (probes — only the length feeds the estimate,
/// so the cells are never materialized).
pub trait Cells {
    fn push(&mut self, c: u16);
    fn len(&self) -> usize;
}
impl Cells for Vec<u16> {
    #[inline]
    fn push(&mut self, c: u16) {
        Vec::push(self, c);
    }
    #[inline]
    fn len(&self) -> usize {
        Vec::len(self)
    }
}
impl Cells for u64 {
    #[inline]
    fn push(&mut self, _: u16) {
        *self += 1;
    }
    #[inline]
    fn len(&self) -> usize {
        *self as usize
    }
}

/// rANS encoder: `put_prob` appends one bit coded with probability `p1`
/// (P(bit=1)·TOT) — feed bits in reverse order with the probabilities a
/// forward model pass recorded.
pub struct RcEnc<S: Cells = Vec<u16>> {
    x: u64,
    out: S,
}

impl Default for RcEnc<Vec<u16>> {
    fn default() -> Self {
        Self::new()
    }
}

impl RcEnc<Vec<u16>> {
    pub fn new() -> Self {
        Self {
            x: L,
            out: Vec::new(),
        }
    }

    pub fn with_capacity(cells: usize) -> Self {
        Self {
            x: L,
            out: Vec::with_capacity(cells),
        }
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

impl RcEnc<u64> {
    /// Counting encoder: emits into a cell counter, no blob storage.
    pub fn counting() -> Self {
        Self { x: L, out: 0 }
    }
}

impl<S: Cells> RcEnc<S> {
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
        let q = mulhi(self.x, MAGIC[f as usize]); // floor(x / f), exact for x < 2^52
        self.x = q * u64::from(TOT) + u64::from(cum) + (self.x - q * u64::from(f));
    }

    /// Cells emitted so far (final blob adds 3 more at `finish`).
    #[inline]
    pub fn len(&self) -> usize {
        self.out.len()
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
            let mut ctxs = [ProbBin::new(); 64];
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
            let mut ctxs = [ProbBin::new(); 64];
            let mut dec = RcDec::new(&blob).unwrap();
            for i in 0..n {
                assert_eq!(dec.get(&mut ctxs[ctx(i)]), bits[i], "n={n} bit {i}");
            }
        }
    }
}
