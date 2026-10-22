#![no_main]

// Payload-level fuzz: v1_reader gates decode behind frame CRCs, so this
// target calls the codec reader directly on fuzz bytes to reach the
// residual/LPC/float decode paths a corrupt container can't reach.
use flq::bits::BitReader;
use flq::codec::{FloatMeta, TrackMeta, read_track};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 8 {
        return;
    }
    let ch = usize::from(data[0] % 16 + 1);
    let mut frames = usize::from(data[1]) * (usize::from(data[2]) + 1);
    let e = i8::from_ne_bytes([data[4]]) as i16;
    let float = match data[3] % 4 {
        1 => Some(FloatMeta {
            mw: 23,
            e,
            stream_bits: 32,
            pcm_bits: 32,
        }),
        2 => Some(FloatMeta {
            mw: 23,
            e,
            stream_bits: 32,
            pcm_bits: 64,
        }),
        3 => Some(FloatMeta {
            mw: 52,
            e,
            stream_bits: 64,
            pcm_bits: 64,
        }),
        _ => None,
    };
    // keep `frames` feasible against the payload or read_track rejects up
    // front before reaching the interesting paths
    let nsig = if ch == 2 { 2 } else { ch } as u64;
    let payload_bits = (data.len() - 8) as u64 * 8;
    let max_frames = (payload_bits / (16 * nsig)).saturating_mul(4096);
    frames = (frames as u64).min(max_frames) as usize;
    if frames == 0 {
        return;
    }
    let meta = TrackMeta {
        channels: ch,
        frames,
        float,
    };
    let mut r = BitReader::new(&data[8..]);
    let _ = read_track(&mut r, &meta);
});
