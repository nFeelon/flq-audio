//! MP3 Huffman: tree-walk decode over vendored Annex B trees, plus encoder
//! tables derived once by DFS.

use crate::bits::{BitReader, BitWriter};
use crate::mp3::tables::HUFFMAN_TABLES;
use std::sync::OnceLock;

/// Walk the decode tree of `table`. Returns (symbol, code bits used):
/// pair tables -> x<<4|y, quad tables -> v<<3|w<<2|x<<1|y. None on overrun
/// or malformed walk.
pub fn decode_symbol(r: &mut BitReader, table: usize) -> Option<(u16, u32)> {
    let data = HUFFMAN_TABLES[table].data;
    if data.is_empty() {
        return Some((0, 0));
    }
    let mut point = 0usize;
    let mut bits = 0u32;
    loop {
        let v = data[point];
        if v & 0xff00 == 0 {
            return Some((v & 0xff, bits));
        }
        bits += 1;
        let bit = r.read(1);
        if r.overrun {
            return None;
        }
        // long jumps: bytes >= 250 skip that many entries first
        let (mut p, mut off) = if bit == 1 {
            (point, usize::from(v & 0xff))
        } else {
            (point, usize::from(v >> 8))
        };
        while off >= 250 {
            p += off;
            if p >= data.len() {
                return None;
            }
            off = if bit == 1 {
                usize::from(data[p] & 0xff)
            } else {
                usize::from(data[p] >> 8)
            };
        }
        point = p + off;
        if point >= data.len() || bits > 32 {
            return None;
        }
    }
}

/// (code, len) per symbol for one table; len 0 = no code (unreachable symbol).
pub struct EncTable {
    pub code: Vec<u32>,
    pub len: Vec<u8>,
}

fn build_enc(table: usize) -> EncTable {
    let t = &HUFFMAN_TABLES[table];
    let nsym = if t.quads { 16 } else { 256 };
    let mut e = EncTable { code: vec![0; nsym], len: vec![0; nsym] };
    if t.data.is_empty() {
        return e;
    }
    fn walk(t: &super::tables::HuffmanTable, point: usize, code: u32, depth: u8, e: &mut EncTable) {
        let v = t.data[point];
        if v & 0xff00 == 0 {
            let sym = if t.quads { usize::from(v & 0xf) } else { usize::from(v & 0xff) };
            e.code[sym] = code;
            e.len[sym] = depth;
            return;
        }
        for b in 0..2u32 {
            let (mut p, mut off) = if b == 1 {
                (point, usize::from(v & 0xff))
            } else {
                (point, usize::from(v >> 8))
            };
            while off >= 250 {
                p += off;
                off = if b == 1 {
                    usize::from(t.data[p] & 0xff)
                } else {
                    usize::from(t.data[p] >> 8)
                };
            }
            walk(t, p + off, (code << 1) | b, depth + 1, e);
        }
    }
    walk(t, 0, 0, 0, &mut e);
    e
}

pub(crate) fn enc_table(table: usize) -> &'static EncTable {
    static ENC: OnceLock<Vec<EncTable>> = OnceLock::new();
    &ENC.get_or_init(|| (0..HUFFMAN_TABLES.len()).map(build_enc).collect())[table]
}

/// Write one symbol's code. Pair tables: `sym` = x<<4|y; quads: 4-bit nibble.
/// Symbol 0 of an empty table (index 0/4/14...) writes nothing.
pub fn encode_symbol(bw: &mut BitWriter, table: usize, sym: usize) {
    let e = enc_table(table);
    if sym < e.len.len() && e.len[sym] > 0 {
        bw.write(u64::from(e.code[sym]), u32::from(e.len[sym]));
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trees_prefix_free_complete() {
        for (i, t) in HUFFMAN_TABLES.iter().enumerate() {
            if t.data.is_empty() {
                continue;
            }
            let e = enc_table(i);
            // Kraft sum == 1 over all coded symbols -> complete prefix-free code
            let kraft: f64 = e
                .len
                .iter()
                .filter(|&&l| l > 0)
                .map(|&l| 2f64.powi(-i32::from(l)))
                .sum();
            assert!((kraft - 1.0).abs() < 1e-9, "table {i} Kraft = {kraft}");
        }
    }

    #[test]
    fn enc_dec_roundtrip_all_symbols() {
        for i in 0..HUFFMAN_TABLES.len() {
            let e = enc_table(i);
            for (sym, &l) in e.len.iter().enumerate() {
                if l == 0 {
                    continue;
                }
                let mut bw = BitWriter::new();
                encode_symbol(&mut bw, i, sym);
                assert_eq!(bw.bits, u64::from(l), "table {i} sym {sym}");
                let bytes = bw.into_bytes();
                let mut r = BitReader::new(&bytes);
                let (got, used) = decode_symbol(&mut r, i).unwrap();
                assert_eq!(got as usize, sym, "table {i} sym {sym}");
                assert_eq!(used, u32::from(l), "table {i} sym {sym} bits");
            }
        }
    }
}
