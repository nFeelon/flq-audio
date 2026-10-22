use flq::{
    Input, PcmOut, Stats, TrackKind, TrackStats, decode, decode_pcm, decode_subset, encode_opts,
    tag_blocks,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

const USAGE: &str = "usage:
  flq encode [--no-joint] <in1> <in2> [more ...] <out.flq>
       inputs: .wav .aiff/.aif/.aifc .flac .mp3 .m4a/.mp4 (ALAC or AAC) .aac .ogg (Vorbis/Opus)
       --no-joint disables all reference analysis (plain LPC/Rice coding)
  flq decode <in.flq> [-o <dir>] [--format flac|wav] [--only <name> [<name> ...]]
       --format picks the output of FLAC/ALAC tracks (default: flac)
       --only decodes just the named tracks (dependencies are decoded too)
  flq bench  [--no-joint] <in1> <in2> [more ...]   # same inputs as encode";

/// Split `encode`/`bench` args into (paths, joint): `--no-joint` is the only
/// flag and may appear anywhere among the inputs.
fn positional(args: &[String]) -> (Vec<String>, bool) {
    let joint = !args.iter().any(|a| a == "--no-joint");
    let pos = args.iter().filter(|a| a.as_str() != "--no-joint").cloned().collect();
    (pos, joint)
}

struct Counting;
static CUR: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grow(n: usize) {
    PEAK.fetch_max(CUR.fetch_add(n, Relaxed) + n, Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            grow(l.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            grow(l.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        CUR.fetch_sub(l.size(), Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new >= l.size() {
                grow(new - l.size());
            } else {
                CUR.fetch_sub(l.size() - new, Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

#[cfg(windows)]
fn cpu_time() -> Option<Duration> {
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        lo: u32,
        hi: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn GetProcessTimes(h: isize, c: *mut FileTime, e: *mut FileTime, k: *mut FileTime, u: *mut FileTime) -> i32;
    }
    let [mut c, mut e, mut k, mut u] = [(); 4].map(|_| FileTime::default());
    let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
    let ns = |f: &FileTime| ((f.hi as u64) << 32 | f.lo as u64) * 100;
    (ok != 0).then(|| Duration::from_nanos(ns(&k) + ns(&u)))
}

#[cfg(not(windows))]
fn cpu_time() -> Option<Duration> {
    None
}

#[cfg(windows)]
fn physical_ram() -> Option<u64> {
    #[repr(C)]
    #[derive(Default)]
    struct MemStatus {
        len: u32,
        load: u32,
        total: u64,
        avail: u64,
        total_pf: u64,
        avail_pf: u64,
        total_v: u64,
        avail_v: u64,
        avail_ev: u64,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalMemoryStatusEx(s: *mut MemStatus) -> i32;
    }
    let mut s = MemStatus { len: std::mem::size_of::<MemStatus>() as u32, ..Default::default() };
    let ok = unsafe { GlobalMemoryStatusEx(&mut s) };
    (ok != 0).then_some(s.total)
}

#[cfg(not(windows))]
fn physical_ram() -> Option<u64> {
    None
}

/// Peak-heap estimate + warning when it is a large share of physical RAM.
fn print_heap_note(s: &Stats) {
    let est = s.est_encode_heap;
    match physical_ram() {
        Some(ram) => {
            let share = est as f64 / ram as f64;
            if share > 0.5 {
                println!(
                    "WARNING: estimated peak encode heap {:.0} MiB is {:.0}% of {:.0} MiB physical RAM",
                    mib(est as usize),
                    100.0 * share,
                    mib(ram as usize)
                );
            } else {
                println!(
                    "est. peak encode heap {:.0} MiB ({:.1}% of physical RAM)",
                    mib(est as usize),
                    100.0 * share
                );
            }
        }
        None => println!("est. peak encode heap {:.0} MiB", mib(est as usize)),
    }
}

struct Metrics {
    wall: Duration,
    cpu: Option<Duration>,
    peak: usize,
    base: usize,
}

fn measure<T>(f: impl FnOnce() -> T) -> (T, Metrics) {
    let base = CUR.load(Relaxed);
    PEAK.store(base, Relaxed);
    let c0 = cpu_time();
    let t0 = Instant::now();
    let v = f();
    let wall = t0.elapsed();
    let cpu = cpu_time().zip(c0).map(|(b, a)| b.saturating_sub(a));
    (v, Metrics { wall, cpu, peak: PEAK.load(Relaxed), base })
}

fn sep(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn mib(b: usize) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

fn pct(a: i64, b: i64) -> f64 {
    if b == 0 { 0.0 } else { 100.0 * a as f64 / b as f64 }
}

fn secs(t: &TrackStats) -> f64 {
    if t.rate == 0 { 0.0 } else { t.frames as f64 / t.rate as f64 }
}

fn kind(t: &TrackStats) -> String {
    if let Some(m) = &t.mp3 {
        let br = if m.bitrate_min == m.bitrate_max {
            format!("{}k", m.bitrate_min)
        } else {
            format!("{}-{}k", m.bitrate_min, m.bitrate_max)
        };
        let mode = match m.mode {
            1 => " [modeled]",
            2 => " [chained]",
            _ => "",
        };
        return format!("mp3 {br} {}ch {} Hz{mode}", m.channels, m.rate);
    }
    if t.kind == TrackKind::Raw {
        return format!("{} [stored]", t.format);
    }
    let k = match (t.float_mode, t.bits) {
        (1, _) => "f32".to_string(),
        (2, _) => "f64 (stored as f32)".to_string(),
        (3, _) => "f64".to_string(),
        (_, 8) => "u8".to_string(),
        (_, b) => format!("i{b}"),
    };
    format!("{} {k} {}ch {} Hz", t.format, t.channels, t.rate)
}

/// The joint-coding section: every multi-track bundle, plus a single track
/// that used self-references (track 0 can carry self-ref savings).
fn show_joint_section(s: &Stats) -> bool {
    s.tracks.len() > 1 || s.tracks.iter().any(|t| t.self_ref_blocks > 0)
}

/// True when nothing in the bundle gained from shared content: no PCM
/// reference blocks and no chained (mode-2) MP3 track.
fn no_shared_content(s: &Stats) -> bool {
    s.tracks
        .iter()
        .all(|t| t.self_ref_blocks == 0 && t.cross_ref_blocks == 0)
        && !s
            .tracks
            .iter()
            .any(|t| t.mp3.as_ref().is_some_and(|m| m.mode == 2))
}

fn print_stats(s: &Stats) {
    let total_in: u64 = s.tracks.iter().map(|t| t.input_bytes).sum();
    println!("Inputs");
    for (i, t) in s.tracks.iter().enumerate() {
        println!("  {} {:<26} {:>14} B   {:<26} {:>8.2} s", i + 1, t.name, sep(t.input_bytes), kind(t), secs(t));
    }
    println!("    {:<26} {:>14} B", "total", sep(total_in));
    println!(
        "Output .flq {:>31} B   ratio {:.3}x   ({:.2}% of inputs)",
        sep(s.total_bytes),
        total_in as f64 / s.total_bytes as f64,
        100.0 * s.total_bytes as f64 / total_in as f64
    );
    println!("Per track (payload bytes)    independent       stereo        final   float bits       meta  % of input");
    for (i, t) in s.tracks.iter().enumerate() {
        let tot = t.final_bits.div_ceil(8) + t.meta_bytes;
        let pcm_coded = matches!(t.kind, TrackKind::Wav | TrackKind::Aiff | TrackKind::Pcm);
        let (ind, st, flt) = if !pcm_coded {
            ("-".into(), "-".into(), "-".into())
        } else {
            (
                sep(t.independent_bits / 8),
                sep(t.stereo_bits / 8),
                sep(t.float_bits / 8),
            )
        };
        println!(
            "  {} {:<22} {:>14} {:>12} {:>12} {:>12} {:>10} {:>10.2}%",
            i + 1,
            t.name,
            ind,
            st,
            sep(t.final_bits / 8),
            flt,
            sep(t.meta_bytes),
            100.0 * tot as f64 / t.input_bytes as f64
        );
    }
    if show_joint_section(s) {
        println!("Joint coding (reference pool per track)");
        for (i, t) in s.tracks.iter().enumerate() {
            let ncc = match t.max_ncc {
                Some(v) if !t.joint_reason.contains("NCC") => format!(", max NCC {v:.3}"),
                _ => String::new(),
            };
            let on = if t.joint { "on" } else { "off" };
            let refs = if t.refs.is_empty() {
                String::new()
            } else {
                format!(
                    " [refs: {}]",
                    t.refs.iter().map(|r| (r + 1).to_string()).collect::<Vec<_>>().join(",")
                )
            };
            let saved = if t.joint {
                format!(
                    ", bundle +{} B (self {} blk, cross {} blk)",
                    sep(t.stereo_bits.saturating_sub(t.final_bits) / 8),
                    t.self_ref_blocks,
                    t.cross_ref_blocks
                )
            } else {
                String::new()
            };
            println!("  {} {}: {on} - {}{}{}{}", i + 1, t.name, t.joint_reason, ncc, refs, saved);
        }
        let saved_bits: u64 = s
            .tracks
            .iter()
            .filter(|t| matches!(t.kind, TrackKind::Wav | TrackKind::Aiff | TrackKind::Pcm))
            .map(|t| t.stereo_bits.saturating_sub(t.final_bits))
            .sum();
        println!(
            "bundle: references saved {} B ({:.2}% of input)",
            sep(saved_bits / 8),
            100.0 * saved_bits as f64 / 8.0 / total_in as f64
        );
        if no_shared_content(s) {
            println!(
                "note: files share no exploitable content — bundling them gains only the shared header (~18 B/track)"
            );
        }
    }
    for (i, t) in s.tracks.iter().enumerate() {
        let Some(m) = &t.mp3 else { continue };
        let fb = 8.0 * t.input_bytes as f64;
        let row = |label: &str, bits: u64| {
            let bytes = if bits.is_multiple_of(8) {
                sep(bits / 8)
            } else {
                format!("{:.2}", bits as f64 / 8.0)
            };
            println!("    {label:<22} {bytes:>12} B {:>7.3}%", 100.0 * bits as f64 / fb);
        };
        println!(
            "MP3 structure {} {}: {} frames, {}ch {} Hz, {}-{} kbps, mode {} -> {} B payload ({:.2}% of file)",
            i + 1,
            t.name,
            sep(m.frames),
            m.channels,
            m.rate,
            m.bitrate_min,
            m.bitrate_max,
            m.mode,
            sep(m.payload_bytes),
            100.0 * m.payload_bytes as f64 / t.input_bytes as f64
        );
        row("raw bytes", 8 * m.raw_bytes);
        row("headers + CRC", m.header_crc_bits);
        row("side info", m.side_bits);
        row("scalefactors", m.scalefac_bits);
        row("big-values Huffman", m.bigval_code_bits);
        row("linbits", m.linbits_bits);
        row("sign bits", m.sign_bits);
        row("count1 codes", m.count1_code_bits);
        row("gaps", m.gap_bits);
        println!(
            "    unparsed granule-channels: {}   invalid frames: {}",
            m.unparsed_grch, m.invalid_frames
        );
        if m.mode != 0 {
            println!("    coded (ideal -log2 p, bytes) vs original bits:");
            // (label, original bits, coded cost bits)
            let rows: [(&str, u64, f64); 9] = [
                ("headers + CRC", m.header_crc_bits, m.coded[0]),
                ("side info", m.side_bits, m.coded[1]),
                ("scalefactors", m.scalefac_bits, m.coded[2]),
                (
                    "coef magnitudes",
                    m.bigval_code_bits + m.count1_code_bits + m.linbits_bits,
                    m.coded[3] + m.coded[4] + m.coded[5],
                ),
                ("  zero flags", 0u64, m.coded[3]),
                ("  unary", 0u64, m.coded[4]),
                ("  escapes", 0u64, m.coded[5]),
                ("signs", m.sign_bits, m.coded[6]),
                ("gaps", m.gap_bits, m.coded[7]),
            ];
            for (label, orig, cost) in rows {
                let saved = if orig == 0 {
                    String::new()
                } else {
                    format!("  (-{:.1}%)", 100.0 * (1.0 - cost / orig as f64))
                };
                println!(
                    "    {label:<20} {bytes:>12.1} B of {orig:>12} b{saved}",
                    bytes = cost / 8.0,
                    orig = sep(orig)
                );
            }
            println!(
                "    raw items + misc      {:>12.1} B   raw={:.1} misc={:.1}",
                (m.coded[8] + m.coded[9]) / 8.0,
                m.coded[8] / 8.0,
                m.coded[9] / 8.0
            );
        }
    }
    let pcm_like = |t: &&TrackStats| {
        matches!(t.kind, TrackKind::Wav | TrackKind::Aiff | TrackKind::Pcm)
    };
    if s.tracks.iter().any(|t| pcm_like(&t)) {
        let sum = |f: fn(&TrackStats) -> u64| {
            s.tracks
                .iter()
                .filter(|t| pcm_like(t))
                .map(f)
                .sum::<u64>() as i64
        };
        let (ind, st, fin) =
            (sum(|t| t.independent_bits), sum(|t| t.stereo_bits), sum(|t| t.final_bits));
        println!("Gains");
        println!("  within-track stereo coding: {:>10} B ({:.3}% of independent)", (ind - st) / 8, pct(ind - st, ind));
        println!("  joint coding (self+cross):  {:>10} B ({:.3}% of stereo)", (st - fin) / 8, pct(st - fin, st));
    }
}

fn print_metrics(label: &str, m: &Metrics, audio: Option<f64>) {
    let cpu = m.cpu.map_or("n/a".to_string(), |c| format!("{:.3} s", c.as_secs_f64()));
    let speed = audio.map_or(String::new(), |a| format!("   speed {:.1}x realtime", a / m.wall.as_secs_f64().max(1e-9)));
    println!(
        "{label:<7} wall {:.3} s   cpu {cpu}   peak heap {:.1} MiB (baseline {:.1} MiB){speed}",
        m.wall.as_secs_f64(),
        mib(m.peak),
        mib(m.base)
    );
}

fn read_inputs(paths: &[String]) -> Result<Vec<Input>, String> {
    paths
        .iter()
        .map(|p| {
            let bytes = fs::read(p).map_err(|e| format!("{p}: {e}"))?;
            let name = Path::new(p).file_name().and_then(|n| n.to_str()).ok_or(format!("{p}: bad file name"))?;
            Ok(Input { name: name.to_string(), bytes })
        })
        .collect()
}

fn write_new(path: &Path, data: &[u8]) -> Result<(), String> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .and_then(|mut f| f.write_all(data))
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn audio_secs(s: &Stats) -> f64 {
    s.tracks.iter().map(secs).sum()
}

fn cmd_encode(ins: &[String], out: &str, joint: bool) -> Result<(), String> {
    let inputs = read_inputs(ins)?;
    let (r, m) = measure(|| encode_opts(&inputs, joint));
    let (bytes, stats) = r?;
    write_new(Path::new(out), &bytes)?;
    print_stats(&stats);
    print_heap_note(&stats);
    print_metrics("Encode", &m, Some(audio_secs(&stats)));
    println!("Wrote {out}");
    Ok(())
}

fn cmd_decode(input: &str, dir: Option<&str>, fmt: PcmOut, only: Option<Vec<String>>) -> Result<(), String> {
    let data = fs::read(input).map_err(|e| format!("{input}: {e}"))?;
    let (r, m) = measure(|| match &only {
        Some(names) => decode_subset(&data, fmt, names),
        None => decode(&data, fmt),
    });
    let outs = r?;
    let p = Path::new(input);
    let dir = dir.map(PathBuf::from).unwrap_or_else(|| {
        p.with_file_name(format!("{}_decoded", p.file_stem().unwrap_or_default().to_string_lossy()))
    });
    if let Some(o) = outs.iter().find(|o| dir.join(&o.name).exists()) {
        return Err(format!("{} already exists, refusing to overwrite", dir.join(&o.name).display()));
    }
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for o in &outs {
        let path = dir.join(&o.name);
        write_new(&path, &o.bytes)?;
        let note = if o.exact { "CRC32 OK (bit-exact)" } else { "CRC32 OK (audio verified)" };
        println!("  {} {:>14} B  {note}", path.display(), sep(o.bytes.len() as u64));
    }
    print_metrics("Decode", &m, None);
    Ok(())
}

fn cmd_bench(ins: &[String], joint: bool) -> Result<(), String> {
    let inputs = read_inputs(ins)?;
    let (r, em) = measure(|| encode_opts(&inputs, joint));
    let (bytes, stats) = r?;
    let (r, dm) = measure(|| decode(&bytes, PcmOut::Flac));
    let outs = r?;
    print_stats(&stats);
    print_heap_note(&stats);
    if stats.total_bytes != bytes.len() as u64 {
        println!("WARNING: stats.total_bytes {} != container length {}", stats.total_bytes, bytes.len());
    }
    let audio = audio_secs(&stats);
    print_metrics("Encode", &em, Some(audio));
    print_metrics("Decode", &dm, Some(audio));
    println!("Accuracy (decoded vs original file bytes)");
    let mut ok = outs.len() == inputs.len();
    for (i, (inp, out)) in inputs.iter().zip(&outs).enumerate() {
        if stats.tracks[i].kind == TrackKind::Pcm {
            // audio-exact: compare decoded samples, not file bytes
            let a = decode_pcm(&inp.bytes);
            let b = decode_pcm(&out.bytes);
            let same = matches!((&a, &b), (Ok(x), Ok(y)) if x == y);
            let tags_ok = matches!(
                (tag_blocks(&inp.bytes), tag_blocks(&out.bytes)),
                (Ok(x), Ok(y)) if x == y
            );
            let n: u64 = a
                .as_ref()
                .map(|v| v.3.iter().map(|c| c.len() as u64).sum())
                .unwrap_or(0);
            ok &= same && tags_ok;
            println!(
                "  {} {}: {}, {} samples identical, file re-encoded, tags {}",
                i + 1,
                inp.name,
                if same && tags_ok { "audio-exact" } else { "MISMATCH" },
                sep(n),
                if tags_ok { "match" } else { "MISMATCH" }
            );
            continue;
        }
        let same = inp.bytes.iter().zip(&out.bytes).filter(|(a, b)| a == b).count() as u64;
        let exact = inp.bytes == out.bytes;
        ok &= exact;
        println!(
            "  {} {}: {}, {}/{} bytes identical (decoded {} B)",
            i + 1,
            inp.name,
            if exact { "bit-exact" } else { "MISMATCH" },
            sep(same),
            sep(inp.bytes.len() as u64),
            sep(out.bytes.len() as u64)
        );
    }
    if ok { Ok(()) } else { Err("accuracy check failed".into()) }
}

/// `flq decode` option parsing (after the input path): `-o dir`,
/// `--format flac|wav`, `--only name ...` — repeated `--only` accumulates.
#[allow(clippy::type_complexity)]
fn decode_opts(a: &[String]) -> Result<(Option<&str>, PcmOut, Option<Vec<String>>), String> {
    let (mut dir, mut fmt, mut only) = (None, PcmOut::Flac, None);
    let mut i = 2;
    let n = a.len();
    while i < n {
        match a[i].as_str() {
            "-o" if i + 1 < n => {
                dir = Some(a[i + 1].as_str());
                i += 2;
            }
            "--format" if i + 1 < n => {
                match a[i + 1].as_str() {
                    "flac" => fmt = PcmOut::Flac,
                    "wav" => fmt = PcmOut::Wav,
                    _ => return Err(USAGE.into()),
                }
                i += 2;
            }
            "--only" => {
                i += 1;
                let names = only.get_or_insert_with(Vec::new);
                let before = names.len();
                while i < n && !matches!(a[i].as_str(), "-o" | "--format" | "--only") {
                    names.push(a[i].clone());
                    i += 1;
                }
                if names.len() == before {
                    return Err(USAGE.into());
                }
            }
            _ => return Err(USAGE.into()),
        }
    }
    Ok((dir, fmt, only))
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let r = match (a.first().map(String::as_str), a.len()) {
        (Some("encode"), n) if n >= 3 => match positional(&a[1..]) {
            (pos, joint) if pos.len() >= 2 => {
                let (ins, out) = pos.split_at(pos.len() - 1);
                cmd_encode(ins, &out[0], joint)
            }
            _ => Err(USAGE.into()),
        },
        (Some("decode"), n) if n >= 2 => match decode_opts(&a) {
            Ok((dir, fmt, only)) => cmd_decode(&a[1], dir, fmt, only),
            Err(e) => Err(e),
        },
        (Some("bench"), n) if n >= 2 => match positional(&a[1..]) {
            (pos, joint) if !pos.is_empty() => cmd_bench(&pos, joint),
            _ => Err(USAGE.into()),
        },
        _ => Err(USAGE.into()),
    };
    if let Err(e) = r {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flq::Mp3Stats;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    fn tstat() -> TrackStats {
        TrackStats {
            name: "t".into(),
            kind: TrackKind::Wav,
            format: "wav".into(),
            input_bytes: 0,
            float_mode: 0,
            bits: 0,
            channels: 0,
            rate: 0,
            frames: 0,
            meta_bytes: 0,
            independent_bits: 0,
            stereo_bits: 0,
            final_bits: 0,
            float_bits: 0,
            joint: false,
            joint_reason: String::new(),
            max_ncc: None,
            self_ref_blocks: 0,
            cross_ref_blocks: 0,
            refs: Vec::new(),
            blocks: 0,
            mp3: None,
        }
    }

    fn stats(ts: Vec<TrackStats>) -> Stats {
        Stats { tracks: ts, total_bytes: 0, est_encode_heap: 0 }
    }

    #[test]
    fn shared_content_gating() {
        // plain two-track bundle: section shows, "no shared content" note OK
        let mut s = stats(vec![tstat(), tstat()]);
        assert!(show_joint_section(&s));
        assert!(no_shared_content(&s));
        // a chained (mode-2) mp3 track is shared content -> suppress the note
        s.tracks[1].mp3 = Some(Mp3Stats { mode: 2, ..Default::default() });
        assert!(!no_shared_content(&s));
        // reference blocks also suppress it
        s.tracks[1].mp3 = None;
        s.tracks[1].cross_ref_blocks = 3;
        assert!(!no_shared_content(&s));

        // single track: section hidden unless it has self-ref savings
        let mut one = stats(vec![tstat()]);
        assert!(!show_joint_section(&one));
        one.tracks[0].self_ref_blocks = 2;
        assert!(show_joint_section(&one));
        assert!(!no_shared_content(&one));
    }

    #[test]
    fn decode_opts_only_accumulates() {
        // repeated --only must union the names, not drop earlier ones
        let a = args(&["decode", "f.flq", "--only", "a", "--only", "b", "c"]);
        let (dir, fmt, only) = decode_opts(&a).unwrap();
        assert_eq!(only.unwrap(), vec!["a".to_string(), "b".to_string(), "c".to_string()]);
        assert!(dir.is_none() && fmt == PcmOut::Flac);

        // --only mixed with other options, in any order
        let a = args(&[
            "decode", "f.flq", "-o", "out", "--only", "a", "--format", "wav", "--only", "b",
        ]);
        let (dir, fmt, only) = decode_opts(&a).unwrap();
        assert_eq!(only.unwrap(), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(dir, Some("out"));
        assert!(fmt == PcmOut::Wav);

        // a bare --only (no names) is still a usage error
        assert!(decode_opts(&args(&["decode", "f.flq", "--only"])).is_err());
        assert!(decode_opts(&args(&["decode", "f.flq", "--only", "-o", "x"])).is_err());
        assert!(decode_opts(&args(&["decode", "f.flq", "--bogus"])).is_err());
    }
}
