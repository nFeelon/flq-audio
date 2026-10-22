//! flq-wasm: raw extern-C bindings to the FLQ decoder for the web player.
//! No wasm-bindgen: handles are u32 indices into a thread-local table,
//! output goes through a shared buffer (`flq_out_ptr`/`flq_out_len`), and
//! the last error string via `flq_err_ptr`/`flq_err_len`.

use flq::v1::dec::{Reader, Samples, samples_to_pcm};
use std::cell::RefCell;
use std::io::Cursor;

type Rd = Reader<Cursor<Vec<u8>>>;

thread_local! {
    static READERS: RefCell<Vec<Option<Rd>>> = const { RefCell::new(Vec::new()) };
    static ERR: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static OUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

fn set_err(msg: impl AsRef<str>) {
    ERR.with(|e| *e.borrow_mut() = msg.as_ref().as_bytes().to_vec());
}

fn set_out(bytes: Vec<u8>) {
    OUT.with(|o| *o.borrow_mut() = bytes);
}

fn with_reader<R>(h: u32, f: impl FnOnce(&mut Rd) -> Result<R, String>) -> Option<R> {
    if h == 0 {
        set_err("invalid handle 0");
        return None;
    }
    READERS.with(|t| {
        let mut t = t.borrow_mut();
        let Some(Some(rd)) = t.get_mut((h - 1) as usize) else {
            set_err(format!("invalid handle {h}"));
            return None;
        };
        match f(rd) {
            Ok(v) => Some(v),
            Err(e) => {
                set_err(e);
                None
            }
        }
    })
}

// ------------------------------------------------------------ memory / errors

/// Allocate `len` bytes in wasm memory for the caller to fill.
#[no_mangle]
pub extern "C" fn flq_alloc(len: u32) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(len as usize);
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

/// Free a buffer returned by `flq_alloc`.
///
/// # Safety
/// `ptr`/`len` must come from a previous `flq_alloc` call, at most once.
#[no_mangle]
pub unsafe extern "C" fn flq_free(ptr: *mut u8, len: u32) {
    if !ptr.is_null() {
        drop(Vec::from_raw_parts(ptr, 0, len as usize));
    }
}

/// Last error message bytes (UTF-8); empty when no error.
#[no_mangle]
pub extern "C" fn flq_err_ptr() -> *const u8 {
    ERR.with(|e| e.borrow().as_ptr())
}

#[no_mangle]
pub extern "C" fn flq_err_len() -> u32 {
    ERR.with(|e| e.borrow().len() as u32)
}

/// Last output buffer bytes.
#[no_mangle]
pub extern "C" fn flq_out_ptr() -> *const u8 {
    OUT.with(|o| o.borrow().as_ptr())
}

#[no_mangle]
pub extern "C" fn flq_out_len() -> u32 {
    OUT.with(|o| o.borrow().len() as u32)
}

// ------------------------------------------------------------------ lifecycle

/// Open a .flq held in wasm memory. Returns a handle > 0, or 0 on error
/// (message via flq_err_*).
///
/// # Safety
/// `ptr` must be valid for `len` bytes for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn flq_open(ptr: *const u8, len: u32) -> u32 {
    if ptr.is_null() || len == 0 {
        set_err("flq_open: null or empty input");
        return 0;
    }
    let bytes = std::slice::from_raw_parts(ptr, len as usize).to_vec();
    match Reader::open(Cursor::new(bytes)) {
        Ok(rd) => READERS.with(|t| {
            let mut t = t.borrow_mut();
            t.push(Some(rd));
            t.len() as u32
        }),
        Err(e) => {
            set_err(e);
            0
        }
    }
}

/// Drop a reader handle.
#[no_mangle]
pub extern "C" fn flq_close(h: u32) {
    READERS.with(|t| {
        if let Some(slot) = t.borrow_mut().get_mut((h.wrapping_sub(1)) as usize) {
            *slot = None;
        }
    });
}

// --------------------------------------------------------------------- calls

fn json_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

/// Write track metadata JSON into the out buffer:
/// {"frame_len":..,"tracks":[{"name","origin","rate","channels",
/// "channel_mask","sample_fmt","bits","frames"}]}
/// Returns 1 on success, 0 on error.
#[no_mangle]
pub extern "C" fn flq_info_json(h: u32) -> u32 {
    let r = with_reader(h, |rd| {
        let tracks = rd
            .tracks()
            .iter()
            .map(|m| {
                format!(
                    "{{\"name\":\"{}\",\"origin\":{},\"rate\":{},\"channels\":{},\
                     \"channel_mask\":{},\"sample_fmt\":{},\"bits\":{},\"frames\":{}}}",
                    json_escape(&m.name),
                    m.origin,
                    m.rate,
                    m.channels,
                    m.channel_mask,
                    m.sample_fmt,
                    m.bits,
                    m.frames
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        Ok(format!("{{\"frame_len\":{},\"tracks\":[{}]}}", rd.frame_len(), tracks))
    });
    match r {
        Some(j) => {
            set_out(j.into_bytes());
            1
        }
        None => 0,
    }
}

/// Decode `[start, start+len)` samples of `track` as interleaved
/// little-endian f32 into the out buffer. Int tracks scale by
/// `2^(bits-1)`; f32 words pass through; f64 words narrow to f32.
/// `start` is a sample index passed as a JS number (f64), floored.
/// Returns 1 on success, 0 on error.
#[no_mangle]
pub extern "C" fn flq_decode_f32(h: u32, track: u32, start: f64, len: u32) -> u32 {
    let r = with_reader(h, |rd| {
        let t = track as usize;
        if t >= rd.tracks().len() {
            return Err(format!("track {t} out of range"));
        }
        let bits = u32::from(rd.tracks()[t].bits);
        let scale = (1i64 << (bits - 1).min(62)) as f32;
        let mut pcm = Vec::<u8>::new();
        rd.decode_range_with(t, start.max(0.0) as u64, u64::from(len), |s| {
            match s {
                Samples::Int(x) => {
                    let n = x.first().map_or(0, |c| c.len());
                    for i in 0..n {
                        for c in &x {
                            pcm.extend((c[i] as f32 / scale).to_le_bytes());
                        }
                    }
                }
                Samples::Float(w) => {
                    let n = w.first().map_or(0, |c| c.len());
                    for i in 0..n {
                        for c in &w {
                            let f = match bits {
                                32 => f32::from_bits(c[i] as u32),
                                _ => f64::from_bits(c[i]) as f32,
                            };
                            pcm.extend(f.to_le_bytes());
                        }
                    }
                }
            }
            Ok(())
        })?;
        Ok(pcm)
    });
    match r {
        Some(pcm) => {
            set_out(pcm);
            1
        }
        None => 0,
    }
}

/// Write the full track as WAV bytes (identical to `flq decode
/// --format wav`) into the out buffer. Returns 1 on success, 0 on error.
#[no_mangle]
pub extern "C" fn flq_wav(h: u32, track: u32) -> u32 {
    let r = with_reader(h, |rd| {
        let t = track as usize;
        let Some(m) = rd.tracks().get(t).cloned() else {
            return Err(format!("track {t} out of range"));
        };
        let mut pcm = Vec::<u8>::new();
        rd.decode_range_with(t, 0, m.frames, |s| {
            pcm.extend(samples_to_pcm(&s, m.bits));
            Ok(())
        })?;
        Ok(flq::formats::make_wav_v1(
            &pcm,
            m.sample_fmt,
            m.bits,
            m.channels,
            m.rate,
            m.channel_mask,
        ))
    });
    match r {
        Some(w) => {
            set_out(w);
            1
        }
        None => 0,
    }
}
