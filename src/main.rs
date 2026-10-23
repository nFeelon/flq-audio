use flq::{aiff, codec, formats, v1, wav};
use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::{self, OpenOptions};
use std::io::{BufReader, BufWriter, Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

const USAGE: &str = "usage:
  flq encode [-0|-5|-8] [-j N] [--frame N] [--no-index] <in1> <in2> [more ...] <out.flq>
       framed lossless archive; inputs: .wav .aiff/.aif/.aifc .flac .m4a (ALAC)
       -0/-5/-8 select the LPC search preset (default -5); -j N sets encode threads
       --frame N sets the frame length (multiple of 4096, 4096..262144, default 16384)
       --no-index writes no frame index (slower --only/--range decoding)
  flq decode <in.flq> [-o <dir>] [-j N] [--format flac|wav] [--only <name> [<name> ...]] [--range a:b]
       -j N sets decode threads (default: all cores; needs the frame index)
       --format picks the output format (default: wav for WAV/AIFF origins,
       flac for FLAC/ALAC origins)
       --only decodes just the named tracks
       --range a:b decodes [a,b) seconds (b optional = to end)
  flq info <in.flq>
  flq bench [-0|-5|-8] [-j N] [--frame N] <in1> <in2> [more ...]   # encode+decode in memory";

/// Split `encode`/`bench` args into (paths, opts). Flags may appear anywhere
/// among the inputs: `-0`/`-5`/`-8` (last wins), `-j N` or `-jN` (N >= 1),
/// `--frame N`, `--no-index`. Returns a usage error for a bad or missing
/// `-j`/`--frame` value.
fn positional(args: &[String]) -> Result<(Vec<String>, v1::enc::V1Opts), String> {
    let mut opts = v1::enc::V1Opts::default();
    let mut pos = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-0" => opts.preset = 0,
            "-5" => opts.preset = 5,
            "-8" => opts.preset = 8,
            "--no-index" => opts.index = false,
            "--frame" => {
                i += 1;
                match args.get(i).and_then(|s| s.parse::<u32>().ok()) {
                    Some(n) if n % 4096 == 0 && (4096..=262144).contains(&n) => opts.frame_len = n,
                    _ => return Err(USAGE.into()),
                }
            }
            "-j" => {
                i += 1;
                let n = args.get(i).and_then(|s| s.parse::<usize>().ok());
                match n {
                    Some(n) if n >= 1 => opts.threads = n,
                    _ => return Err(USAGE.into()),
                }
            }
            a if a.starts_with("-j") => match a[2..].parse::<usize>() {
                Ok(n) if n >= 1 => opts.threads = n,
                _ => return Err(USAGE.into()),
            },
            a if a.starts_with('-') => return Err(USAGE.into()),
            a => pos.push(a.to_string()),
        }
        i += 1;
    }
    Ok((pos, opts))
}

/// Resolved thread count for reporting (0 = available_parallelism, fallback 1).
fn effective_threads(opts: &v1::enc::V1Opts) -> usize {
    cores(opts.threads)
}

/// Resolved worker count for decode: 0 = all cores.
fn cores(n: usize) -> usize {
    if n == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    } else {
        n
    }
}

struct Counting;
static CUR: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grow(n: usize) {
    PEAK.fetch_max(CUR.fetch_add(n, Relaxed) + n, Relaxed);
}

// SAFETY: pure accounting around System — same layout passed through
// unchanged, counts updated only on success.
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
        fn GetProcessTimes(
            h: isize,
            c: *mut FileTime,
            e: *mut FileTime,
            k: *mut FileTime,
            u: *mut FileTime,
        ) -> i32;
    }
    let [mut c, mut e, mut k, mut u] = [(); 4].map(|_| FileTime::default());
    // SAFETY: GetProcessTimes writes FILETIME-sized output into each
    // pointer; FileTime is #[repr(C)] and 8 bytes, matching the API.
    let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
    let ns = |f: &FileTime| ((f.hi as u64) << 32 | f.lo as u64) * 100;
    (ok != 0).then(|| Duration::from_nanos(ns(&k) + ns(&u)))
}

#[cfg(not(windows))]
fn cpu_time() -> Option<Duration> {
    None
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
    (
        v,
        Metrics {
            wall,
            cpu,
            peak: PEAK.load(Relaxed),
            base,
        },
    )
}

fn sep(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn mib(b: usize) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

fn print_metrics(label: &str, m: &Metrics, audio: Option<f64>) {
    let cpu = m
        .cpu
        .map_or("n/a".to_string(), |c| format!("{:.3} s", c.as_secs_f64()));
    let speed = audio.map_or(String::new(), |a| {
        format!(
            "   speed {:.1}x realtime",
            a / m.wall.as_secs_f64().max(1e-9)
        )
    });
    println!(
        "{label:<7} wall {:.3} s   cpu {cpu}   peak heap {:.1} MiB (baseline {:.1} MiB){speed}",
        m.wall.as_secs_f64(),
        mib(m.peak),
        mib(m.base)
    );
}

fn v1_inputs(ins: &[String]) -> Result<Vec<v1::enc::V1Input>, String> {
    ins.iter()
        .map(|p| {
            let name = Path::new(p)
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or(format!("{p}: bad file name"))?;
            Ok(v1::enc::V1Input {
                name: name.to_string(),
                path: PathBuf::from(p),
            })
        })
        .collect()
}

fn v1_fmt_label(m: &v1::TrackInfo) -> String {
    let k = match (m.sample_fmt, m.bits) {
        (1, 32) => "f32".to_string(),
        (1, _) => "f64".to_string(),
        (_, 8) => "u8".to_string(),
        (_, b) => format!("i{b}"),
    };
    let origin = ["wav", "aiff", "flac", "alac"][m.origin as usize];
    format!("{origin} {k} {}ch {} Hz", m.channels, m.rate)
}

fn print_stats(s: &v1::enc::V1Stats, metas: &[v1::TrackInfo]) {
    let total_in: u64 = s.tracks.iter().map(|t| t.input_bytes).sum();
    println!("Inputs");
    for (i, (t, m)) in s.tracks.iter().zip(metas).enumerate() {
        let secs = t.frames as f64 / t.rate as f64;
        println!(
            "  {} {:<26} {:>14} B   {:<26} {:>8.2} s",
            i + 1,
            t.name,
            sep(t.input_bytes),
            v1_fmt_label(m),
            secs
        );
    }
    println!("    {:<26} {:>14} B", "total", sep(total_in));
    println!(
        "Output .flq {:>31} B   ratio {:.3}x   ({:.2}% of inputs)",
        sep(s.total_bytes),
        total_in as f64 / s.total_bytes as f64,
        100.0 * s.total_bytes as f64 / total_in.max(1) as f64
    );
    println!("Per track (frame bytes incl. headers)   coded   % of input   frames");
    for (i, t) in s.tracks.iter().enumerate() {
        println!(
            "  {} {:<22} {:>14} B {:>8.2}% {:>10}",
            i + 1,
            t.name,
            sep(t.coded_bytes),
            100.0 * t.coded_bytes as f64 / t.input_bytes.max(1) as f64,
            t.nframes
        );
    }
    println!(
        "  header+meta+index {:>14} B",
        sep(s.header_meta_index_bytes)
    );
}

/// `.part` doubles as an exclusive lock — an existing one usually means a
/// crashed run, not a live writer, so say so in the error.
fn part_open_err(part: &Path, e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::AlreadyExists {
        format!(
            "{} already exists — stale temp file? delete it or choose another output",
            part.display()
        )
    } else {
        format!("{}: {e}", part.display())
    }
}

/// `flq encode` — the framed format.
fn cmd_encode(ins: &[String], out: &str, opts: &v1::enc::V1Opts) -> Result<(), String> {
    if Path::new(out).exists() {
        return Err(format!("{out} already exists, refusing to overwrite"));
    }
    let inputs = v1_inputs(ins)?;
    let out_path = PathBuf::from(out);
    let part = PathBuf::from(format!("{out}.part"));
    let (r, m) = measure(
        move || -> Result<(v1::enc::V1Stats, Vec<v1::TrackInfo>), String> {
            let f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&part)
                .map_err(|e| part_open_err(&part, e))?;
            let mut w = std::io::BufWriter::new(f);
            let res = (|| -> Result<v1::enc::V1Stats, String> {
                let st = v1::enc::encode(&inputs, &mut w, opts)?;
                w.flush().map_err(|e| format!("{}: {e}", part.display()))?;
                Ok(st)
            })();
            drop(w);
            let st = match res {
                Ok(st) => {
                    fs::rename(&part, &out_path).map_err(|e| format!("{out}: {e}"))?;
                    st
                }
                Err(e) => {
                    let _ = fs::remove_file(&part);
                    return Err(e);
                }
            };
            // re-read the metas for the summary print
            let rd = v1::dec::Reader::open(std::io::BufReader::new(
                std::fs::File::open(&out_path).map_err(|e| format!("{out}: {e}"))?,
            ))?;
            Ok((st, rd.tracks().to_vec()))
        },
    );
    let (stats, metas) = r?;
    codec::prof::dump();
    print_stats(&stats, &metas);
    println!(
        "preset -{}, {} threads, frame {}",
        opts.preset,
        effective_threads(opts),
        opts.frame_len
    );
    let audio: f64 = stats
        .tracks
        .iter()
        .map(|t| t.frames as f64 / t.rate.max(1) as f64)
        .sum();
    print_metrics("Encode", &m, Some(audio));
    println!("Wrote {out}");
    Ok(())
}

/// Default output extension for a track (origin wav/aiff -> wav,
/// flac/alac -> flac).
fn default_ext(m: &v1::TrackInfo) -> &'static str {
    if m.origin >= v1::ORIGIN_FLAC {
        "flac"
    } else {
        "wav"
    }
}

/// Sample range for a `--range` option on one track: (start, len) with len
/// left unclamped (the reader clamps); errors when start >= track length.
fn range_bounds(
    t: &v1::TrackInfo,
    range: Option<(f64, Option<f64>)>,
) -> Result<(u64, u64), String> {
    match range {
        Some((a, b)) => {
            let secs = t.frames as f64 / t.rate as f64;
            if a >= secs {
                return Err(format!(
                    "track {}: --range start {a} beyond track ({secs:.3} s)",
                    t.name
                ));
            }
            let start = (a * t.rate as f64) as u64;
            let len = b.map_or(u64::MAX, |b| ((b - a).max(0.0) * t.rate as f64) as u64);
            Ok((start, len))
        }
        None => Ok((0, t.frames)),
    }
}

/// Stream a WAV export: header first (sizes known), then each frame's
/// canonical PCM to a BufWriter through `decode_range_with`. Full-track
/// writes verify pcm_crc incrementally. The file is written to
/// `<out>.part` and renamed on success; on any error the .part is deleted.
fn stream_wav_out<R: Read + Seek>(
    rd: &mut v1::dec::Reader<R>,
    track: usize,
    t: &v1::TrackInfo,
    range: Option<(f64, Option<f64>)>,
    threads: usize,
    out: &Path,
) -> Result<u64, String> {
    let (start, len) = range_bounds(t, range)?;
    let out_frames = len.min(t.frames.saturating_sub(start));
    let align = u64::from(t.channels) * u64::from(t.bits) / 8;
    let pcm_len = (out_frames * align) as usize;
    let head = formats::wav_header_v1(
        pcm_len,
        t.sample_fmt,
        t.bits,
        t.channels,
        t.rate,
        t.channel_mask,
    );
    let part = PathBuf::from(format!("{}.part", out.display()));
    let f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)
        .map_err(|e| part_open_err(&part, e))?;
    let mut w = BufWriter::new(f);
    let res = (|| -> Result<(), String> {
        w.write_all(&head)
            .map_err(|e| format!("{}: {e}", part.display()))?;
        let mut crc = v1::dec::Crc32::new();
        let check = range.is_none();
        rd.decode_range_with_par(track, start, len, threads, |s| {
            let pcm = v1::dec::samples_to_pcm(&s, t.bits);
            if check {
                crc.update(&pcm);
            }
            w.write_all(&pcm)
                .map_err(|e| format!("{}: {e}", part.display()))
        })?;
        if pcm_len % 2 == 1 {
            w.write_all(&[0])
                .map_err(|e| format!("{}: {e}", part.display()))?;
        }
        w.flush().map_err(|e| format!("{}: {e}", part.display()))?;
        if check && crc.finish() != t.pcm_crc {
            return Err(format!("track {}: pcm_crc mismatch", t.name));
        }
        Ok(())
    })();
    drop(w);
    match res {
        Ok(()) => {
            fs::rename(&part, out).map_err(|e| format!("{}: {e}", out.display()))?;
            Ok((head.len() + pcm_len + (pcm_len & 1)) as u64)
        }
        Err(e) => {
            let _ = fs::remove_file(&part);
            Err(e)
        }
    }
}

/// Stream a FLAC export: decoded frames flow through a channel — the v1
/// decoder pushes on a scoped thread, flacenc pulls and writes
/// `<out>.part` here. The file is then re-decoded and CRC-verified, and
/// renamed on success; on any error the .part is deleted. Peak memory is
/// O(threads * frame) + the encoded output, never O(track).
fn stream_flac_out<R: Read + Seek + Send>(
    rd: &mut v1::dec::Reader<R>,
    track: usize,
    t: &v1::TrackInfo,
    range: Option<(f64, Option<f64>)>,
    threads: usize,
    out: &Path,
) -> Result<u64, String> {
    if t.sample_fmt != v1::FMT_INT {
        return Err(format!(
            "track {}: FLAC can't hold f{}; use --format wav",
            t.name, t.bits
        ));
    }
    if t.bits == 32 {
        return Err(format!(
            "track {}: FLAC can't hold i32; use --format wav",
            t.name
        ));
    }
    let (start, len) = range_bounds(t, range)?;
    let out_frames = len.min(t.frames.saturating_sub(start));
    let part = PathBuf::from(format!("{}.part", out.display()));
    let f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)
        .map_err(|e| part_open_err(&part, e))?;
    let res = std::thread::scope(|sc| -> Result<u32, String> {
        let (tx, rx) =
            std::sync::mpsc::sync_channel::<Result<Vec<Vec<i64>>, String>>(threads.max(4));
        let rd = &mut *rd;
        let prod = sc.spawn(move || -> Result<u32, String> {
            let mut crc = v1::dec::Crc32::new();
            rd.decode_range_with_par(track, start, len, threads, |s| {
                let v1::dec::Samples::Int(x) = s else {
                    return Err(format!("track {}: unexpected float samples", t.name));
                };
                crc.update(&wav::ints_to_pcm(
                    &x,
                    u16::from(t.bits),
                    x.first().map_or(0, Vec::len),
                ));
                tx.send(Ok(x)).map_err(|_| "flac pipe closed".to_string())
            })?;
            Ok(crc.finish())
        });
        let enc = (|| -> Result<(), String> {
            let mut w = BufWriter::new(f);
            formats::encode_flac_stream(
                rx,
                &mut w,
                u16::from(t.bits),
                u16::from(t.channels),
                t.rate,
                &t.tags,
                out_frames,
            )?;
            w.flush().map_err(|e| format!("{}: {e}", part.display()))
        })();
        let prod = prod.join().map_err(|_| "decode worker panicked".to_string());
        enc.and(prod?)
    });
    let done = res.and_then(|crc| {
        formats::verify_flac_file(
            &part,
            u16::from(t.channels),
            t.rate,
            u16::from(t.bits),
            crc,
        )?;
        fs::rename(&part, out).map_err(|e| format!("{}: {e}", out.display()))?;
        fs::metadata(out)
            .map(|m| m.len())
            .map_err(|e| format!("{}: {e}", out.display()))
    });
    match done {
        Err(e) => {
            let _ = fs::remove_file(&part);
            Err(e)
        }
        ok => ok,
    }
}

fn cmd_decode(
    input: &str,
    dir: Option<&str>,
    fmt: Option<&'static str>,
    only: Option<Vec<String>>,
    range: Option<(f64, Option<f64>)>,
    threads: usize,
) -> Result<(), String> {
    let f = std::fs::File::open(input).map_err(|e| format!("{input}: {e}"))?;
    let mut rd = v1::dec::Reader::open(BufReader::new(f))?;
    let infos = rd.tracks().to_vec();
    let picks: Vec<usize> = match &only {
        Some(names) => names
            .iter()
            .map(|n| {
                infos.iter().position(|t| &t.name == n).ok_or_else(|| {
                    let avail = infos
                        .iter()
                        .map(|t| t.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("unknown track {n:?} (tracks: {avail})")
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
        None => (0..infos.len()).collect(),
    };
    let p = Path::new(input);
    let dir = dir.map(PathBuf::from).unwrap_or_else(|| {
        p.with_file_name(format!(
            "{}_decoded",
            p.file_stem().unwrap_or_default().to_string_lossy()
        ))
    });
    let outname = |name: &str, ext: &str| {
        let stem = Path::new(name)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy();
        format!("{stem}.{ext}")
    };
    let outs: Vec<(usize, &'static str, PathBuf)> = picks
        .iter()
        .map(|&i| {
            let ext: &'static str = fmt.unwrap_or_else(|| default_ext(&infos[i]));
            (i, ext, dir.join(outname(&infos[i].name, ext)))
        })
        .collect();
    let mut seen = std::collections::HashSet::new();
    for &(i, _, ref path) in &outs {
        if !v1::valid_name(&infos[i].name) {
            return Err(format!("track {}: invalid name {:?}", i, infos[i].name));
        }
        if !seen.insert(path.clone()) {
            return Err(format!("{}: outputs collide", path.display()));
        }
    }
    if let Some((_, _, path)) = outs.iter().find(|(_, _, p)| p.exists()) {
        return Err(format!(
            "{} already exists, refusing to overwrite",
            path.display()
        ));
    }
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let (r, m) = measure(move || -> Result<Vec<(PathBuf, u64)>, String> {
        outs.iter()
            .map(|&(i, ext, ref path)| {
                let t = &infos[i];
                if ext == "wav" {
                    return stream_wav_out(&mut rd, i, t, range, threads, path)
                        .map(|n| (path.clone(), n));
                }
                stream_flac_out(&mut rd, i, t, range, threads, path)
                    .map(|n| (path.clone(), n))
            })
            .collect()
    });
    let written = r?;
    for (path, n) in &written {
        println!(
            "  {} {:>14} B  CRC32 OK (audio verified)",
            path.display(),
            sep(*n)
        );
    }
    print_metrics("Decode", &m, None);
    Ok(())
}

/// `flq decode` option parsing (after the input path): `-o dir`,
/// `--format flac|wav`, `--only name ...` — repeated `--only` accumulates —
/// `--range a:b` (seconds, `b` optional = to end), `-j N` decode threads
/// (0/absent = all cores; parallel frame decode needs an index).
#[allow(clippy::type_complexity)]
fn decode_opts(
    a: &[String],
) -> Result<
    (
        Option<&str>,
        Option<&'static str>,
        Option<Vec<String>>,
        Option<(f64, Option<f64>)>,
        usize,
    ),
    String,
> {
    let (mut dir, mut fmt, mut only, mut range) = (None, None, None, None);
    let mut threads = 0usize;
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
                    "flac" => fmt = Some("flac"),
                    "wav" => fmt = Some("wav"),
                    _ => return Err(USAGE.into()),
                }
                i += 2;
            }
            "--range" if i + 1 < n => {
                let (a_s, b_s) = a[i + 1].split_once(':').unwrap_or((a[i + 1].as_str(), ""));
                let a_v = a_s.parse::<f64>().map_err(|_| USAGE.to_string())?;
                let b_v = if b_s.is_empty() {
                    None
                } else {
                    Some(b_s.parse::<f64>().map_err(|_| USAGE.to_string())?)
                };
                if !a_v.is_finite()
                    || a_v < 0.0
                    || b_v.is_some_and(|b| !b.is_finite() || b <= a_v)
                {
                    return Err(USAGE.into());
                }
                range = Some((a_v, b_v));
                i += 2;
            }
            "-j" if i + 1 < n => {
                threads = a[i + 1]
                    .parse::<usize>()
                    .ok()
                    .filter(|&t| t >= 1)
                    .ok_or_else(|| USAGE.to_string())?;
                i += 2;
            }
            "--only" => {
                i += 1;
                let names = only.get_or_insert_with(Vec::new);
                let before = names.len();
                while i < n && !a[i].starts_with('-') {
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
    Ok((dir, fmt, only, range, threads))
}

fn cmd_info(input: &str) -> Result<(), String> {
    let f = std::fs::File::open(input).map_err(|e| format!("{input}: {e}"))?;
    let mut rd = v1::dec::Reader::open(BufReader::new(f))?;
    println!(
        "FLQ v1 framed archive: {} tracks, frame_len {}, index {}",
        rd.tracks().len(),
        rd.frame_len(),
        if rd.has_index() { "yes" } else { "no" }
    );
    let infos = rd.tracks().to_vec();
    for (i, t) in infos.iter().enumerate() {
        let coded = rd.coded_bytes(i).map_or("-".to_string(), sep);
        println!(
            "  {} {:<26} {:<28} {:>8.3} s  {:>6} frames  {:>14} B coded",
            i + 1,
            t.name,
            v1_fmt_label(t),
            t.frames as f64 / t.rate as f64,
            t.nframes,
            coded
        );
    }
    Ok(())
}

/// Per-track source samples for the bench accuracy check.
fn source_samples(path: &str) -> Result<v1::dec::Samples, String> {
    let bytes = fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    match formats::sniff(&bytes) {
        formats::Sniff::Wav => {
            let w = wav::parse(&bytes)?;
            Ok(if w.tag == 3 {
                v1::dec::Samples::Float(wav::channel_words(&w))
            } else {
                v1::dec::Samples::Int(wav::channel_ints(&w))
            })
        }
        formats::Sniff::Aiff => {
            let w = aiff::parse(&bytes)?;
            Ok(if w.tag == 3 {
                v1::dec::Samples::Float(wav::channel_words(&w))
            } else {
                v1::dec::Samples::Int(wav::channel_ints(&w))
            })
        }
        _ => {
            let (_, _, _, x) = formats::decode_pcm(&bytes)?;
            Ok(v1::dec::Samples::Int(x))
        }
    }
}

/// `flq bench` — encode into memory, verify by streaming decode (pcm_crc;
/// no whole-track buffer on the decode side). Accuracy compares each
/// source's canonical-PCM crc against the crc stored in the meta — the
/// same end-to-end check as decoding the samples, at a fraction of the
/// peak.
fn cmd_bench(ins: &[String], opts: &v1::enc::V1Opts) -> Result<(), String> {
    let inputs = v1_inputs(ins)?;
    let (r, em) = measure(move || {
        let mut cur = Cursor::new(Vec::new());
        v1::enc::encode(&inputs, &mut cur, opts).map(|st| (cur.into_inner(), st))
    });
    let (bytes, stats) = r?;
    let dec_threads = effective_threads(opts);
    let (r, dm) = measure(|| {
        let mut rd = v1::dec::Reader::open(Cursor::new(&bytes))?;
        (0..rd.tracks().len())
            .map(|t| rd.verify_track_par(t, dec_threads))
            .collect::<Result<Vec<_>, _>>()
    });
    r?;
    let rd = v1::dec::Reader::open(Cursor::new(&bytes))?;
    print_stats(&stats, rd.tracks());
    println!(
        "preset -{}, {} threads, frame {}",
        opts.preset,
        effective_threads(opts),
        opts.frame_len
    );
    let audio: f64 = stats
        .tracks
        .iter()
        .map(|t| t.frames as f64 / t.rate.max(1) as f64)
        .sum();
    print_metrics("Encode", &em, Some(audio));
    print_metrics("Decode", &dm, Some(audio));
    println!("Accuracy (source canonical pcm vs stored pcm_crc)");
    let mut ok = true;
    for (i, p) in ins.iter().enumerate() {
        let name = Path::new(p)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(p);
        let src = source_samples(p)?;
        let t = &rd.tracks()[i];
        let mut crc = v1::dec::Crc32::new();
        crc.update(&v1::dec::samples_to_pcm(&src, t.bits));
        let same = crc.finish() == t.pcm_crc;
        ok &= same;
        let n = t.frames * u64::from(t.channels);
        println!(
            "  {} {}: {}, {} samples identical",
            i + 1,
            name,
            if same { "audio-exact" } else { "MISMATCH" },
            sep(n)
        );
    }
    if ok {
        Ok(())
    } else {
        Err("accuracy check failed".into())
    }
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let r = match (a.first().map(String::as_str), a.len()) {
        (Some("encode"), n) if n >= 3 => match positional(&a[1..]) {
            Ok((pos, opts)) if pos.len() >= 2 => {
                let (ins, out) = pos.split_at(pos.len() - 1);
                cmd_encode(ins, &out[0], &opts)
            }
            Ok(_) => Err(USAGE.into()),
            Err(e) => Err(e),
        },
        (Some("decode"), n) if n >= 2 => match decode_opts(&a) {
            Ok((dir, fmt, only, range, j)) => cmd_decode(&a[1], dir, fmt, only, range, cores(j)),
            Err(e) => Err(e),
        },
        (Some("info"), 2) => cmd_info(&a[1]),
        (Some("bench"), n) if n >= 2 => match positional(&a[1..]) {
            Ok((pos, opts)) if !pos.is_empty() => cmd_bench(&pos, &opts),
            Ok(_) => Err(USAGE.into()),
            Err(e) => Err(e),
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

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn decode_opts_only_accumulates() {
        // repeated --only must union the names, not drop earlier ones
        let a = args(&["decode", "f.flq", "--only", "a", "--only", "b", "c"]);
        let (dir, fmt, only, range, _) = decode_opts(&a).unwrap();
        assert_eq!(
            only.unwrap(),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert!(dir.is_none() && fmt.is_none() && range.is_none());

        // --only mixed with other options, in any order
        let a = args(&[
            "decode", "f.flq", "-o", "out", "--only", "a", "--format", "wav", "--only", "b",
        ]);
        let (dir, fmt, only, ..) = decode_opts(&a).unwrap();
        assert_eq!(only.unwrap(), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(dir, Some("out"));
        assert!(fmt == Some("wav"));

        // --range a:b and --range a (open end)
        let a = args(&["decode", "f.flq", "--range", "1.5:3", "--only", "x"]);
        let (_, _, only, range, _) = decode_opts(&a).unwrap();
        assert_eq!(range, Some((1.5, Some(3.0))));
        assert_eq!(only.unwrap(), vec!["x".to_string()]);
        let a = args(&["decode", "f.flq", "--range", "2:"]);
        assert_eq!(decode_opts(&a).unwrap().3, Some((2.0, None)));

        // -j sets decode threads (0/absent = all cores)
        let a = args(&["decode", "f.flq", "-j", "4", "--only", "x"]);
        let (_, _, only, _, j) = decode_opts(&a).unwrap();
        assert_eq!(j, 4);
        assert_eq!(only.unwrap(), vec!["x".to_string()]);

        // a bare --only (no names) is still a usage error
        assert!(decode_opts(&args(&["decode", "f.flq", "--only"])).is_err());
        assert!(decode_opts(&args(&["decode", "f.flq", "--only", "-o", "x"])).is_err());
        assert!(decode_opts(&args(&["decode", "f.flq", "--bogus"])).is_err());
    }

    #[test]
    fn decode_corrupt_leaves_no_part() {
        // a decode that fails mid-stream must remove the .part file and
        // leave no output behind
        let dir = std::env::temp_dir().join(format!("flqdec_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // small 16-bit mono WAV on disk for the encoder
        let x: Vec<Vec<i64>> = vec![
            (0..9000)
                .map(|i| (8000.0 * (i as f64 * 0.05).sin()) as i64)
                .collect(),
        ];
        let wav_b = formats::make_wav(&x, 16, 1, 44100);
        let wav_p = dir.join("in.wav");
        fs::write(&wav_p, &wav_b).unwrap();
        let inputs = [v1::enc::V1Input {
            name: "t.wav".into(),
            path: wav_p,
        }];
        let mut cur = Cursor::new(Vec::new());
        v1::enc::encode(
            &inputs,
            &mut cur,
            &v1::enc::V1Opts {
                preset: 8,
                threads: 1,
                frame_len: 4096,
                index: true,
            },
        )
        .unwrap();
        let mut bytes = cur.into_inner();
        // corrupt a payload byte of the last frame (offset via the index)
        let flq_p = dir.join("t.flq");
        let n = rd_nframes(&bytes);
        let idx = index_off(&bytes);
        let last_off =
            u64::from_le_bytes(bytes[idx + (n - 1) * 8..idx + n * 8].try_into().unwrap()) as usize;
        bytes[last_off + 20] ^= 0xFF;
        fs::write(&flq_p, &bytes).unwrap();
        let out_dir = dir.join("out");
        let r = cmd_decode(
            flq_p.to_str().unwrap(),
            Some(out_dir.to_str().unwrap()),
            None,
            None,
            None,
            1,
        );
        assert!(r.is_err());
        let leftovers: Vec<_> = fs::read_dir(&out_dir)
            .map(|d| d.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "leftover outputs: {leftovers:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    fn index_off(b: &[u8]) -> usize {
        u64::from_le_bytes(b[16..24].try_into().unwrap()) as usize
    }
    fn rd_nframes(b: &[u8]) -> usize {
        v1::dec::Reader::open(Cursor::new(b)).unwrap().tracks()[0].nframes as usize
    }
}
