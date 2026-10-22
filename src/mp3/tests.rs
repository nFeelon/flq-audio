//! MP3 parser tests: synthetic valid files, rebuild identity on arbitrary
//! input, and exact bit-accounting.

use super::frame::{
    Block, ChSide, ParsedGrCh, SideInfo, encode_grch, header, scalefac_layout, write_side,
};
use super::tables::HUFFMAN_TABLES;
use super::{Mp3Stats, parse, rebuild, stats};
use crate::bits::{BitReader, BitWriter};

pub(crate) struct Lcg(u64);

impl Lcg {
    pub(crate) fn new(seed: u64) -> Self {
        Lcg(seed | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    pub(crate) fn range(&mut self, lo: usize, hi: usize) -> usize {
        if hi <= lo { lo } else { lo + (self.next() % (hi - lo) as u64) as usize }
    }
    fn bit(&mut self) -> bool {
        self.next() & 1 == 1
    }
}

#[derive(Default)]
pub(crate) struct Coverage {
    long: bool,
    short: bool,
    mixed: bool,
    start_end: bool,
    linbits: bool,
    count1_a: bool,
    count1_b: bool,
    scfsi_reuse: bool,
    reservoir: bool,
    crc_on: bool,
    crc_off: bool,
    mono: bool,
    stereo: bool,
}

const RATES: [u32; 3] = [44100, 48000, 32000];
const BITRATES: [u32; 16] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0];

/// Random valid values for one granule channel; payload bits come from the
/// real encoder so part2_3 is exact by construction.
fn synth_grch(
    rng: &mut Lcg,
    block: Block,
    granule: usize,
    scfsi: &[bool; 4],
    rate_idx: usize,
    cov: &mut Coverage,
    max_bv: usize,
) -> Option<(ChSide, ParsedGrCh)> {
    let mut cs = ChSide {
        window_switching: matches!(block, Block::Switched { .. }),
        block,
        scalefac_compress: rng.range(0, 16) as u8,
        big_values: rng.range(0, max_bv.min(289)) as u16,
        preflag: rng.bit(),
        scalefac_scale: rng.bit(),
        count1: rng.bit(),
        ..Default::default()
    };
    match cs.block {
        Block::Long { .. } => {
            cov.long = true;
            cs.tabs = [
                rng.range(0, 32) as u8,
                rng.range(0, 32) as u8,
                rng.range(0, 32) as u8,
            ];
        }
        Block::Switched { block_type, mixed } => {
            match block_type {
                2 if mixed => cov.mixed = true,
                2 => cov.short = true,
                _ => cov.start_end = true,
            }
            cs.tabs = [rng.range(0, 32) as u8, rng.range(0, 32) as u8, 0];
            cs.subblock = [rng.range(0, 8) as u8, rng.range(0, 8) as u8, rng.range(0, 8) as u8];
        }
    }
    if cs.count1 {
        cov.count1_b = true;
    } else {
        cov.count1_a = true;
    }

    let layout = scalefac_layout(&cs, scfsi, granule);
    let mut d = ParsedGrCh {
        scalefacs: Vec::new(),
        ix: [0; 576],
        count1_quads: 0,
        sf_bits: 0,
        code_bits: 0,
        lin_bits: 0,
        sign_bits: 0,
        quad_code_bits: 0,
    };
    for &(len, count) in &layout {
        for _ in 0..count {
            let v = if len == 0 { 0 } else { (rng.next() as u8) & ((1 << len) - 1) };
            d.scalefacs.push(v);
        }
    }
    if granule == 1 && layout.iter().any(|&(l, c)| l == 0 && c > 0) {
        cov.scfsi_reuse = true;
    }

    let (r1, r2) = super::frame::region_bounds(&cs, rate_idx)?;
    let is_len = usize::from(cs.big_values) * 2;
    let mut pos = 0usize;
    while pos < is_len {
        let table = usize::from(if pos < r1 { cs.tabs[0] } else if pos < r2 { cs.tabs[1] } else { cs.tabs[2] });
        let lin = i64::from(HUFFMAN_TABLES[table].linbits as u32);
        let e = super::huffman::enc_table(table);
        // pick a symbol that exists in this table (encoders always do)
        let sym = if e.len.iter().all(|&l| l == 0) {
            0
        } else {
            loop {
                let s = rng.range(0, 256);
                if e.len.get(s).is_some_and(|&l| l > 0) {
                    break s;
                }
            }
        };
        let (mut x, mut y) = ((sym >> 4) as i64, (sym & 15) as i64);
        if lin > 0 && x == 15 {
            cov.linbits = true;
            x += rng.range(0, 1usize << lin.min(6)) as i64;
        }
        if lin > 0 && y == 15 {
            cov.linbits = true;
            y += rng.range(0, 1usize << lin.min(6)) as i64;
        }
        if x > 0 && rng.bit() {
            x = -x;
        }
        if y > 0 && rng.bit() {
            y = -y;
        }
        d.ix[pos] = x as i16;
        d.ix[pos + 1] = y as i16;
        pos += 2;
    }
    d.count1_quads = rng.range(0, 12) as u16;
    for q in 0..usize::from(d.count1_quads) {
        let i = is_len + q * 4;
        if i + 4 > 576 {
            d.count1_quads = q as u16;
            break;
        }
        for k in 0..4 {
            d.ix[i + k] = (rng.range(0, 3) as i16) - 1;
        }
    }
    // encode once to learn the exact part2_3 length
    let mut bw = BitWriter::new();
    encode_grch(&mut bw, &cs, scfsi, granule, rate_idx, &d);
    if bw.bits > 4095 {
        return None;
    }
    cs.part2_3 = bw.bits as u16;
    Some((cs, d))
}

struct SynthFrame {
    hdr: [u8; 4],
    crc: Option<u16>,
    side: SideInfo,
    nch: usize,
    /// per-frame granule payload, MSB-first bits
    pay: Vec<bool>,
}

fn synth_frame(
    rng: &mut Lcg,
    off_f: usize,
    prev_end: usize,
    mdb: u16,
    cov: &mut Coverage,
) -> SynthFrame {
    let rate_idx = rng.range(0, 3);
    let bri = rng.range(1, 15);
    let pad = rng.range(0, 2) as u8;
    let mono = rng.range(0, 4) == 0;
    let mode = if mono { 3u8 } else { rng.range(0, 3) as u8 };
    let nch = if mode == 3 { 1 } else { 2 };
    let has_crc = rng.bit();
    if has_crc {
        cov.crc_on = true;
    } else {
        cov.crc_off = true;
    }
    if mono {
        cov.mono = true;
    } else {
        cov.stereo = true;
    }
    let frame_len =
        144_000 * BITRATES[bri] as usize / RATES[rate_idx] as usize + usize::from(pad);
    let crc_len = usize::from(has_crc) * 2;
    let side_len = if nch == 1 { 17 } else { 32 };
    let budget_bits = 8 * (usize::from(mdb) + frame_len - 4 - crc_len - side_len);

    // granule-channel payloads; regenerate until they fit the slot budget
    let per_grch = budget_bits / (2 * nch);
    let mut attempt = 0usize;
    let (side, pay) = loop {
        attempt += 1;
        // ~2 bits/pair minimum -> shrink the value space until it fits
        let max_bv = if attempt > 64 { 1 } else { (per_grch / 8).max(2) };
        let mut side = SideInfo {
            mdb,
            private: (rng.next() as u16) & if nch == 1 { 0x1f } else { 0x7 },
            scfsi: [[false; 4]; 2],
            gr: [
                [ChSide::default(), ChSide::default()],
                [ChSide::default(), ChSide::default()],
            ],
        };
        for c in 0..nch {
            for band in &mut side.scfsi[c] {
                *band = rng.bit();
            }
        }
        let mut datas = Vec::new();
        let mut ok = true;
        for g in 0..2 {
            for c in 0..nch {
                let block = match rng.range(0, 6) {
                    0 | 1 => Block::Long {
                        region0: rng.range(0, 8) as u8,
                        region1: rng.range(0, 6) as u8,
                    },
                    2 => Block::Switched { block_type: 2, mixed: false },
                    3 => Block::Switched { block_type: 2, mixed: true },
                    4 => Block::Switched { block_type: 1, mixed: false },
                    _ => Block::Switched { block_type: 3, mixed: false },
                };
                // scfsi reuse only legal on long blocks of granule 1
                let scf = &side.scfsi[c];
                let Some((cs, d)) = synth_grch(rng, block, g, scf, rate_idx, cov, max_bv) else {
                    ok = false;
                    break;
                };
                side.gr[g][c] = cs;
                datas.push(d);
            }
            if !ok {
                break;
            }
        }
        if !ok {
            continue;
        }
        // serialize all gr/ch payloads; total must fit the slot + reservoir
        let mut pay = Vec::new();
        let mut it = datas.iter();
        let mut total_bits = 0usize;
        for g in 0..2 {
            for c in 0..nch {
                let cs = &side.gr[g][c];
                let mut bw = BitWriter::new();
                encode_grch(&mut bw, cs, &side.scfsi[c], g, rate_idx, it.next().unwrap());
                total_bits += cs.part2_3 as usize;
                let bytes = bw.into_bytes();
                let mut r = BitReader::new(&bytes);
                for _ in 0..cs.part2_3 {
                    pay.push(r.read(1) == 1);
                }
            }
        }
        if total_bits <= budget_bits {
            break (side, pay);
        }
        let _ = (off_f, prev_end); // budget depends on mdb chosen by caller
    };
    let mut hdr = [0xff, 0xfa, 0, 0];
    if !has_crc {
        hdr[1] |= 1;
    }
    hdr[2] = ((bri as u8) << 4) | ((rate_idx as u8) << 2) | (pad << 1);
    hdr[3] = mode << 6;
    let crc = if has_crc {
        let mut bw = BitWriter::new();
        write_side(&mut bw, &side, nch);
        Some(super::model::mp3_crc16(u32::from_be_bytes(hdr), &bw.into_bytes()))
    } else {
        None
    };
    SynthFrame { hdr, crc, side, nch, pay }
}

/// Build a valid MPEG-1 L3 stream: `n` frames, mixed coverage, reservoir
/// offsets, CRC on/off, mono/stereo. Returns (file bytes, coverage seen).
pub(crate) fn synth_mp3(rng: &mut Lcg, n: usize) -> (Vec<u8>, Coverage) {
    let mut cov = Coverage::default();
    // first pass: plan frames sequentially — payloads decide placement
    let mut frames: Vec<SynthFrame> = Vec::with_capacity(n);
    let mut slot_of = Vec::with_capacity(n);
    let mut off = 0usize; // bit offset in M (multiple of 8)
    let mut prev_end = 0usize;
    // build bit stream M in parallel
    let mut m: Vec<bool> = Vec::new();
    for i in 0..n {
        let off_f = off;
        let max_mdb = ((off_f - prev_end) / 8).min(511);
        let mdb = if i == 0 { 0 } else { rng.range(0, max_mdb + 1) } as u16;
        if mdb > 0 {
            cov.reservoir = true;
        }
        let f = synth_frame(rng, off_f, prev_end, mdb, &mut cov);
        let data_start = off_f - 8 * usize::from(mdb);
        let end = data_start + f.pay.len();
        // write payload bits into M
        if m.len() < end {
            m.resize(end, false);
        }
        for (k, &b) in f.pay.iter().enumerate() {
            m[data_start + k] = b;
        }
        prev_end = end;
        // slot length: from the real header
        let h = header(&f.hdr, 0).unwrap();
        let slot = h.len - 4 - usize::from(f.crc.is_some()) * 2 - h.side_len;
        debug_assert!(8 * (off_f / 8 + slot) >= end);
        let slot_end = off_f + 8 * slot;
        while m.len() < slot_end {
            m.push(rng.bit());
        }
        off = slot_end;
        slot_of.push(slot);
        frames.push(f);
    }
    // pack M MSB-first
    let mbytes: Vec<u8> = m
        .chunks(8)
        .map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | u8::from(b)))
        .collect();
    // serialize
    let mut file = Vec::new();
    let mut mo = 0usize;
    for (i, f) in frames.iter().enumerate() {
        file.extend_from_slice(&f.hdr);
        if let Some(c) = f.crc {
            file.extend_from_slice(&c.to_be_bytes());
        }
        let mut bw = BitWriter::new();
        write_side(&mut bw, &f.side, f.nch);
        file.extend_from_slice(&bw.into_bytes());
        file.extend_from_slice(&mbytes[mo..mo + slot_of[i]]);
        mo += slot_of[i];
    }
    (file, cov)
}

fn total_bits(s: &Mp3Stats) -> u64 {
    s.raw_bytes * 8
        + s.header_crc_bits
        + s.side_bits
        + s.scalefac_bits
        + s.bigval_code_bits
        + s.linbits_bits
        + s.sign_bits
        + s.count1_code_bits
        + s.gap_bits
}

fn assert_invariant(file: &[u8]) -> Mp3Stats {
    let f = parse(file);
    let rb = rebuild(&f);
    if rb != file {
        let i = rb
            .iter()
            .zip(file)
            .position(|(a, b)| a != b)
            .unwrap_or(rb.len().min(file.len()));
        panic!(
            "rebuild != input ({} B): first diff at byte {i}, len {} vs {}",
            file.len(),
            rb.len(),
            file.len()
        );
    }
    let s = stats(&f);
    assert_eq!(total_bits(&s), file.len() as u64 * 8, "stats bit sum");
    s
}

#[test]
fn synth_roundtrip() {
    let mut rng = Lcg::new(0x5eed);
    let (file, cov) = synth_mp3(&mut rng, 48);
    let s = assert_invariant(&file);
    assert_eq!(s.frames, 48);
    assert_eq!(s.unparsed_grch, 0);
    assert_eq!(s.invalid_frames, 0);
    assert_eq!(s.raw_bytes, 0);
    for (name, got) in [
        ("long", cov.long),
        ("short", cov.short),
        ("mixed", cov.mixed),
        ("start_end", cov.start_end),
        ("linbits", cov.linbits),
        ("count1_a", cov.count1_a),
        ("count1_b", cov.count1_b),
        ("scfsi", cov.scfsi_reuse),
        ("reservoir", cov.reservoir),
        ("crc_on", cov.crc_on),
        ("crc_off", cov.crc_off),
        ("mono", cov.mono),
        ("stereo", cov.stereo),
    ] {
        assert!(got, "synthetic file missing coverage: {name}");
    }
    assert!(s.scalefac_bits > 0 && s.bigval_code_bits > 0);
}

fn id3v2(payload: &[u8]) -> Vec<u8> {
    let n = payload.len();
    let mut v = b"ID3\x04\x00\x00".to_vec();
    v.extend([
        ((n >> 21) & 0x7f) as u8,
        ((n >> 14) & 0x7f) as u8,
        ((n >> 7) & 0x7f) as u8,
        (n & 0x7f) as u8,
    ]);
    v.extend_from_slice(payload);
    v
}

#[test]
fn tags_and_junk() {
    let mut rng = Lcg::new(42);
    let (mp3, _) = synth_mp3(&mut rng, 12);
    let mut file = id3v2(&vec![7u8; 300]);
    let tag_len = file.len();
    file.extend(&mp3);
    file.extend(b"TAG");
    file.extend(vec![0u8; 125]);
    let s = assert_invariant(&file);
    assert_eq!(s.raw_bytes as usize, tag_len + 128);
    assert_eq!(s.frames, 12);
    assert_eq!(s.unparsed_grch, 0);
}

#[test]
fn junk_between_runs_and_truncated_tail() {
    let mut rng = Lcg::new(7);
    let (a, _) = synth_mp3(&mut rng, 8);
    let (b_part, _) = synth_mp3(&mut rng, 8);
    let mut file = a.clone();
    file.extend(vec![0x00u8; 37]); // junk without sync bytes
    let cut = b_part.len() - 13; // chop inside the last frame
    file.extend(&b_part[..cut]);
    let s = assert_invariant(&file);
    assert!(s.raw_bytes >= 37 + 13);
    assert_eq!(s.frames, 15); // 8 + 7 full frames of b_part
}

#[test]
fn garbage_and_empty() {
    assert_invariant(b"");
    let g: Vec<u8> = (0u32..997).map(|i| (i.wrapping_mul(31) & 0xff) as u8).collect();
    let s = assert_invariant(&g);
    assert_eq!(s.frames, 0);
    assert_eq!(s.raw_bytes, g.len() as u64);
}

#[test]
fn rebuild_identity_random() {
    let mut rng = Lcg::new(0xC0FFEE);
    let (mp3, _) = synth_mp3(&mut rng, 24);
    for case in 0..400 {
        let mut buf = mp3.clone();
        match case % 4 {
            // random byte flips
            0 => {
                for _ in 0..rng.range(1, 9) {
                    let i = rng.range(0, buf.len());
                    buf[i] ^= rng.next() as u8;
                }
            }
            // truncation
            1 => buf.truncate(rng.range(0, buf.len())),
            // junk insertion
            2 => {
                let i = rng.range(0, buf.len() + 1);
                for _ in 0..rng.range(1, 40) {
                    buf.insert(i, rng.next() as u8);
                }
            }
            // fresh random buffer
            _ => buf = (0..rng.range(0, 2000)).map(|_| rng.next() as u8).collect(),
        }
        let f = parse(&buf);
        assert_eq!(rebuild(&f), buf, "case {case}");
        // model round-trip on arbitrary parses
        let (c, _) = super::model::compress(&f);
        match super::model::decompress(&c, buf.len() as u64) {
            Ok(d) => assert_eq!(rebuild(&d), buf, "model case {case}"),
            Err(e) => panic!("model case {case}: decompress failed: {e}"),
        }
    }
}

#[test]
fn model_rt_tags_junk() {
    let mut rng = Lcg::new(4242);
    let (mp3, _) = synth_mp3(&mut rng, 16);
    let mut file = id3v2(&[9u8; 200]);
    file.extend(&mp3);
    file.extend(b"TAG");
    file.extend(vec![0xDEu8; 125]); // ID3v1 + junk tail
    let f = parse(&file);
    let (c, _) = super::model::compress(&f);
    assert!(c.len() < file.len());
    let d = super::model::decompress(&c, file.len() as u64).unwrap();
    assert_eq!(rebuild(&d), file);
}

#[test]
fn model_crc_frames() {
    // every CRC-carrying frame must hold the real MPEG CRC-16 (flag path)
    let mut rng = Lcg::new(0xC0C0);
    let (mp3, _) = synth_mp3(&mut rng, 24);
    let f = parse(&mp3);
    let mut seen = 0;
    if let super::Item::Run(r) = &f.items[0] {
        for fr in &r.frames {
            if let Some(c) = fr.crc {
                assert_eq!(Some(c), Some(super::model::mp3_crc16(fr.hdr, &{
                    let mut bw = BitWriter::new();
                    super::frame::write_side(&mut bw, &fr.side, fr.hdr_fields.nch);
                    bw.into_bytes()
                })));
                seen += 1;
            }
        }
    }
    assert!(seen > 0);
    let (c, _) = super::model::compress(&f);
    let d = super::model::decompress(&c, mp3.len() as u64).unwrap();
    assert_eq!(rebuild(&d), mp3);
}

#[test]
fn model_corrupt() {
    let mut rng = Lcg::new(31337);
    let (mp3, _) = synth_mp3(&mut rng, 16);
    let f = parse(&mp3);
    let (c, _) = super::model::compress(&f);
    // absurd bounds must fail fast without huge allocation
    assert!(super::model::decompress(&c, 10).is_err());
    for case in 0..300 {
        let mut bad = c.clone();
        match case % 3 {
            0 => {
                let i = rng.range(0, bad.len());
                bad[i] ^= 1u8 << (case % 8);
            }
            1 => bad.truncate(rng.range(0, bad.len())),
            _ => {
                let i = rng.range(0, bad.len() + 1);
                bad.insert(i, rng.next() as u8);
            }
        }
        if let Ok(d) = super::model::decompress(&bad, mp3.len() as u64) {
            assert_ne!(rebuild(&d), mp3, "case {case}: corrupt decoded to original");
        }
    }
}

#[test]
fn sniff_cases() {
    use super::sniff;
    assert!(sniff(b"ID3\x04"));
    assert!(!sniff(b""));
    assert!(!sniff(b"RIFFxxxxWAVE"));
    let mut rng = Lcg::new(9);
    let (mp3, _) = synth_mp3(&mut rng, 4);
    assert!(sniff(&mp3));
    assert!(!sniff(&mp3[..4])); // single header, no successor, not whole file
}

/// Any `testdata/*.mp3` files (not shipped — drop one in to enable).
fn testdata_mp3s() -> Vec<std::path::PathBuf> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    let mut v: Vec<_> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "mp3"))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

#[test]
#[ignore]
fn model_debug() {
    for p in testdata_mp3s() {
        let p = p.display().to_string();
        let b = std::fs::read(&p).unwrap();
        let f = parse(&b);
        let (c, costs) = super::model::compress(&f);
        let (c2a, _) = super::model::compress_opts(&f, super::rc::LIMIT, false);
        println!("{p}: {} B -> compressed {} B (2a {} B), ideal {:.0} B", b.len(), c.len(), c2a.len(), costs.iter().sum::<f64>() / 8.0);
        const NAMES: [&str; 10] = [
            "hdr+crc", "side", "scalefac", "mag0", "mag-unary", "mag-esc",
            "sign", "gap", "raw", "misc",
        ];
        for (n, &v) in NAMES.iter().zip(&costs) {
            println!("  {n:10} ideal {:>12.0} B", v / 8.0);
        }
        match super::model::decompress(&c, b.len() as u64) {
            Ok(d) => {
                let r = rebuild(&d);
                println!("  rebuild len {} exact: {}", r.len(), r == b);
                assert_eq!(r, b);
            }
            Err(e) => panic!("  decompress err: {e}"),
        }
    }
}


#[test]
#[ignore]
fn limit_sweep() {
    for p in testdata_mp3s() {
        let p = p.display().to_string();
        let b = std::fs::read(&p).unwrap();
        let f = parse(&b);
        for limit in [20u8, 40, 127] {
            let t = std::time::Instant::now();
            let (c, _) = super::model::compress_opts(&f, limit, false);
            println!("{p} LIMIT={limit} 2a: {} B ({:?})", c.len(), t.elapsed());
            let t = std::time::Instant::now();
            let (c, _) = super::model::compress_opts(&f, limit, true);
            println!("{p} LIMIT={limit} 2b: {} B ({:?})", c.len(), t.elapsed());
        }
    }
}

#[test]
fn squash_stretch_integer() {
    use super::model::{squash, stretch};
    // pinned lpaq1 table values (integer-only, platform-independent)
    assert_eq!(squash(0), 2047);
    assert_eq!(squash(-2047), 1);
    assert_eq!(squash(2047), 4094);
    assert_eq!(squash(2048), 4095);
    assert_eq!(squash(-2048), 0);
    let mut prev = 0;
    for d in -2047..=2047 {
        let v = squash(d);
        assert!(v >= prev, "squash not monotone at {d}");
        prev = v;
    }
    let mut prev = -2048;
    for p in 0..4096 {
        let s = stretch(p);
        assert!(s >= prev, "stretch not monotone at {p}");
        prev = s;
    }
    assert_eq!(stretch(4095), 2047);
    // stretch inverts squash to within one interpolation step near the center
    for d in (-1000..=1000).step_by(37) {
        let s = stretch(squash(d) as usize);
        assert!((s - d).abs() <= 8, "d={d} stretch={s}");
    }
    assert!(stretch(squash(0) as usize).abs() <= 2);
}

#[test]
fn model_extreme_confidence() {
    // saturate the mixer/context models with many all-zero granules, then end
    // with granules holding large linbits magnitudes (synth generates them).
    // Must not hang (range-coder clamp) and must round-trip byte-exact.
    let mut rng = Lcg::new(777);
    let (bytes, _) = synth_mp3(&mut rng, 64);
    let mut f = parse(&bytes);
    let run = match f.items.iter_mut().find(|i| matches!(i, super::Item::Run(_))) {
        Some(super::Item::Run(r)) => r,
        _ => panic!("synthetic mp3 must contain a run"),
    };
    let nfr = run.frames.len();
    for ch in run.chunks.iter_mut() {
        let super::Chunk::GrCh { frame, data, .. } = ch else {
            continue;
        };
        if *frame < nfr - 4 {
            data.ix = [0; 576];
            data.count1_quads = 0;
        }
    }
    // refresh part2_3 for the mutated granules (all-zero data shrinks it)
    let mut p23 = std::collections::HashMap::new();
    for ch in run.chunks.iter() {
        if let super::Chunk::GrCh { frame, granule, ch: c, data } = ch {
            let fr = &run.frames[*frame];
            let mut bw = BitWriter::new();
            encode_grch(&mut bw, &fr.side.gr[*granule][*c], &fr.side.scfsi[*c], *granule, fr.hdr_fields.rate_idx, data);
            p23.insert((*frame, *granule, *c), bw.bits as u16);
        }
    }
    for ((fi, g, c), v) in &p23 {
        run.frames[*fi].side.gr[*g][*c].part2_3 = *v;
    }
    // rebuild the chunk list so gaps absorb the shrunk regions (region
    // positions come from the side-info geometry; gap content is arbitrary)
    let mut datas = std::collections::HashMap::new();
    for ch in std::mem::take(&mut run.chunks) {
        if let super::Chunk::GrCh { frame, granule, ch: c, data } = ch {
            datas.insert((frame, granule, c), data);
        }
    }
    let mut new_chunks: Vec<super::Chunk> = Vec::new();
    let mut off = 0i64;
    let mut cursor = 0i64;
    let gap = |n: i64| {
        (n > 0).then(|| {
            super::Chunk::Gap(super::PackedBits {
                nbits: n as u64,
                bytes: vec![0; ((n + 7) / 8) as usize],
            })
        })
    };
    for (fi, fr) in run.frames.iter().enumerate() {
        let mut pos = 8 * (off - i64::from(fr.side.mdb));
        for g in 0..2 {
            for c in 0..fr.hdr_fields.nch {
                if let Some(&np23) = p23.get(&(fi, g, c)) {
                    if pos >= cursor {
                        new_chunks.extend(gap(pos - cursor));
                        new_chunks.push(super::Chunk::GrCh {
                            frame: fi,
                            granule: g,
                            ch: c,
                            data: datas.remove(&(fi, g, c)).expect("parsed granule data"),
                        });
                        cursor = pos + i64::from(np23);
                    }
                    pos += i64::from(np23);
                } else {
                    pos += i64::from(fr.side.gr[g][c].part2_3);
                }
            }
        }
        off += fr.slot as i64;
    }
    new_chunks.extend(gap(8 * run.m_bytes as i64 - cursor));
    run.chunks = new_chunks;
    // reparse so the parser itself re-derives consistent granule geometry
    let bytes2 = rebuild(&f);
    let f2 = parse(&bytes2);
    assert_eq!(rebuild(&f2), bytes2);
    let (c, _) = super::model::compress(&f2);
    let d = super::model::decompress(&c, bytes2.len() as u64)
        .expect("saturated stream must decompress");
    assert_eq!(rebuild(&d), bytes2);
}

#[test]
#[ignore]
fn timing_split() {
    use std::time::Instant;
    let Some(p) = testdata_mp3s().into_iter().next() else {
        return;
    };
    let b = std::fs::read(&p).unwrap();
    let t = Instant::now();
    let f = parse(&b);
    println!("parse               {:?}", t.elapsed());
    let t = Instant::now();
    let (c, _) = super::model::compress(&f);
    println!("compress            {:?}", t.elapsed());
    let t = Instant::now();
    let d = super::model::decompress(&c, b.len() as u64).unwrap();
    let r = rebuild(&d);
    println!("decompress+rebuild  {:?}", t.elapsed());
    assert_eq!(r, b);
}

#[test]
fn model_rt_synth() {
    let mut rng = Lcg::new(999);
    for it in 0..20 {
        let (bytes, _cov) = synth_mp3(&mut rng, 12);
        let f = parse(&bytes);
        let (c, _costs) = super::model::compress(&f);
        let d = match super::model::decompress(&c, bytes.len() as u64) {
            Ok(d) => d,
            Err(e) => panic!("iter {it}: decompress: {e}"),
        };
        assert_eq!(rebuild(&d), bytes);
    }
}

