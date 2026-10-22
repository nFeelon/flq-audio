#![no_main]

use libfuzzer_sys::fuzz_target;
use std::io::Cursor;

fuzz_target!(|data: &[u8]| {
    let Ok(mut r) = flq::v1::dec::Reader::open(Cursor::new(data)) else {
        return;
    };
    for t in 0..r.tracks().len() {
        let _ = r.decode_track(t);
    }
    let _ = r.scan();
});
