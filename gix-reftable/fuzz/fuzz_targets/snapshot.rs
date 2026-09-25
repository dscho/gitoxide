#![no_main]

use gix_reftable::{
    Table,
    bstr::{BStr, BString},
    stack::Snapshot,
};
use libfuzzer_sys::fuzz_target;
use std::hint::black_box;

fuzz_target!(|data: &[u8]| {
    // A hash byte and table count precede length-prefixed tables and a lookup name.
    let Some((&kind, input)) = data.split_first() else {
        return;
    };
    let Some((&table_count, mut input)) = input.split_first() else {
        return;
    };
    let object_hash = if kind & 1 == 0 {
        gix_hash::Kind::Sha1
    } else {
        gix_hash::Kind::Sha256
    };
    let mut tables = Vec::new();
    for _ in 0..table_count {
        let Some((length, rest)) = input.split_at_checked(2) else {
            break;
        };
        let len = usize::from(u16::from_be_bytes([length[0], length[1]]));
        let Some((table, rest)) = rest.split_at_checked(len) else {
            break;
        };
        input = rest;
        if Table::from_bytes(table).is_ok_and(|table| table.header().object_hash == object_hash) {
            tables.push((BString::from(format!("{}.ref", tables.len())), table.to_vec()));
        }
    }
    if let Ok(snapshot) = Snapshot::from_tables(object_hash, tables) {
        let name = BStr::new(input);
        _ = black_box(snapshot.try_find_reference(name));
        if let Ok(mut references) = snapshot.references_prefixed(name) {
            while let Ok(Some(reference)) = references.next_record() {
                _ = black_box(reference);
            }
        }
        if let Ok(mut logs) = snapshot.logs(name) {
            while let Ok(Some(log)) = logs.next_record() {
                _ = black_box(log);
            }
        }
    }
});
