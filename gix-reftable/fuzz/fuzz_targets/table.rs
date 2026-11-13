#![no_main]

use libfuzzer_sys::fuzz_target;
use std::hint::black_box;

fuzz_target!(|data: &[u8]| {
    _ = black_box(gix_reftable::Table::from_bytes(data));
});
