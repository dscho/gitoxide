#![no_main]

use libfuzzer_sys::fuzz_target;
use std::hint::black_box;

fuzz_target!(|data: &[u8]| {
    if let Ok(table) = gix_reftable::Table::from_bytes(data) {
        let mut references = table.references();
        while let Ok(Some(reference)) = references.next_record() {
            _ = black_box(reference);
        }
        let mut logs = table.logs();
        while let Ok(Some(log)) = logs.next_record() {
            _ = black_box(log);
        }
    }
});
