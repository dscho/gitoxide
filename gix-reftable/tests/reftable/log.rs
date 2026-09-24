use std::io::Write;

use gix_reftable::Table;
use gix_testtools::bstr::{BString, ByteSlice};

pub(super) fn varint(mut value: u64, out: &mut Vec<u8>) {
    let mut bytes = [0; 10];
    let mut pos = bytes.len() - 1;
    bytes[pos] = (value & 127) as u8;
    while value > 127 {
        value = (value >> 7) - 1;
        pos -= 1;
        bytes[pos] = (value & 127) as u8 | 128;
    }
    out.extend_from_slice(&bytes[pos..]);
}

fn log_key(name: &[u8], index: u64) -> Vec<u8> {
    let mut out = name.to_vec();
    out.push(0);
    out.extend_from_slice(&(!index).to_be_bytes());
    out
}

pub(super) fn record(name: &[u8], index: u64, update: bool) -> Vec<u8> {
    let key = log_key(name, index);
    let mut out = vec![0];
    varint(((key.len() as u64) << 3) | u64::from(update), &mut out);
    out.extend(key);
    if update {
        out.extend(std::iter::repeat_n(1, gix_testtools::object_hash().len_in_bytes()));
        out.extend(std::iter::repeat_n(2, gix_testtools::object_hash().len_in_bytes()));
        for value in [b"committer".as_slice(), b"committer@example.com"] {
            varint(value.len() as u64, &mut out);
            out.extend_from_slice(value);
        }
        varint(u64::MAX, &mut out);
        out.extend_from_slice(&(-150_i16).to_be_bytes());
        varint(8, &mut out);
        out.extend_from_slice(b"message\xff");
    }
    out
}

fn compressed_block(header_skip: usize, records: &[u8], restarts: &[usize]) -> crate::Result<Vec<u8>> {
    let block = super::reference::block(b'g', header_skip, records, restarts);
    let mut encoder = gix_zlib::stream::deflate::Write::new(Vec::new(), gix_zlib::Compression::DEFAULT);
    encoder.write_all(&block[4..])?;
    encoder.flush()?;
    let mut out = block[..4].to_vec();
    out.extend(encoder.into_inner());
    Ok(out)
}

pub(super) fn log_table(records: &[u8], restarts: &[usize]) -> crate::Result<Vec<u8>> {
    let mut data = super::reference::empty_table();
    let header_len = if data[4] == 1 { 24 } else { 28 };
    data.splice(header_len..header_len, compressed_block(header_len, records, restarts)?);
    Ok(data)
}

fn indexed_log_table() -> crate::Result<Vec<u8>> {
    let mut data = super::reference::empty_table();
    let header_len = if data[4] == 1 { 24 } else { 28 };
    let footer = data.split_off(header_len);
    data.extend(compressed_block(header_len, &record(b"refs/a", 10, true), &[0])?);
    let second_offset = data.len();
    data.extend(compressed_block(0, &record(b"refs/z", 9, true), &[0])?);
    let lower_offset = data.len();
    let mut lower = super::reference::index_record(&log_key(b"refs/a", 10), 0);
    let second_restart = lower.len();
    lower.extend(super::reference::index_record(&log_key(b"refs/z", 9), second_offset));
    data.extend(super::reference::block(b'i', 0, &lower, &[0, second_restart]));
    let root_offset = data.len();
    let root = super::reference::index_record(&log_key(b"refs/z", 9), lower_offset);
    data.extend(super::reference::block(b'i', 0, &root, &[0]));
    data.extend_from_slice(&footer);
    super::envelope::footer_field(&mut data, 4).copy_from_slice(&(root_offset as u64).to_be_bytes());
    super::envelope::update_checksum(&mut data);
    Ok(data)
}

fn padded_root_log_index_table() -> crate::Result<Vec<u8>> {
    let mut data = super::reference::empty_table();
    let header_len = if data[4] == 1 { 24 } else { 28 };
    let mut footer = data.split_off(header_len);
    for header in [&mut data, &mut footer] {
        header[5..8].copy_from_slice(&128_u32.to_be_bytes()[1..]);
    }
    data.extend(compressed_block(header_len, &record(b"refs/a", 10, true), &[0])?);
    let second_offset = data.len();
    data.extend(compressed_block(0, &record(b"refs/z", 9, true), &[0])?);
    let root_offset = data.len();
    let first = super::reference::index_record(&log_key(b"refs/a", 10), 0);
    let first = super::reference::block(b'i', 0, &first, &[0]);
    data.extend_from_slice(&first);
    data.resize(root_offset + 128, 0);
    let second = super::reference::index_record(&log_key(b"refs/z", 9), second_offset);
    data.extend(super::reference::block(b'i', 0, &second, &[0]));
    data.extend_from_slice(&footer);
    super::envelope::footer_field(&mut data, 4).copy_from_slice(&(root_offset as u64).to_be_bytes());
    super::envelope::update_checksum(&mut data);
    Ok(data)
}

#[test]
fn log_only_tables_preserve_binary_values_and_tombstones() -> crate::Result {
    let mut records = record(b"refs/a", 10, true);
    records.extend(record(b"refs/b", 9, false));
    let data = log_table(&records, &[0])?;
    let table = Table::from_bytes(&data)?;
    assert!(
        table.references().next_record()?.is_none(),
        "log-only tables do not invent reference records"
    );
    let mut logs = table.logs();
    let entry = logs.next_record()?.expect("the first log record is an update");
    assert_eq!(
        entry.name, "refs/a",
        "the binary update index is not part of the returned name"
    );
    assert_eq!(
        entry.update_index, 10,
        "reverse encoding preserves the full update index"
    );
    let update = entry.data.expect("the record includes log data");
    assert_eq!(
        update.previous_object_id.as_bytes(),
        vec![1; gix_testtools::object_hash().len_in_bytes()],
        "the previous object ID uses the table hash width"
    );
    assert_eq!(
        update.new_object_id.as_bytes(),
        vec![2; gix_testtools::object_hash().len_in_bytes()],
        "the new object ID uses the table hash width"
    );
    assert_eq!(update.committer.name, "committer");
    assert_eq!(update.committer.email, "committer@example.com");
    assert_eq!(
        update.time_seconds,
        u64::MAX,
        "binary timestamps are not truncated to signed time"
    );
    assert_eq!(update.timezone_minutes, -150, "timezone offsets are signed minutes");
    assert_eq!(
        update.message,
        b"message\xff".as_bstr(),
        "messages remain uninterpreted bytes"
    );

    let deletion = logs.next_record()?.expect("tombstones are returned, not filtered");
    assert_eq!(deletion.name, "refs/b");
    assert_eq!(deletion.update_index, 9);
    assert_eq!(deletion.data, None, "the absence of data denotes a deletion");
    assert!(logs.next_record()?.is_none());
    Ok(())
}

#[test]
fn malformed_compression_and_declared_lengths_are_rejected() -> crate::Result {
    let original = log_table(&record(b"refs/a", 10, true), &[0])?;
    let header_len = if original[4] == 1 { 24 } else { 28 };
    let size = u32::from_be_bytes([
        0,
        original[header_len + 1],
        original[header_len + 2],
        original[header_len + 3],
    ]);
    for declared in [1, size - 1, size + 1] {
        let mut data = original.clone();
        data[header_len + 1..header_len + 4].copy_from_slice(&declared.to_be_bytes()[1..]);
        let table = Table::from_bytes(&data)?;
        let mut logs = table.logs();
        assert!(
            logs.next_record().is_err(),
            "declared length {declared} must match inflated data"
        );
        assert!(logs.next_record()?.is_none(), "a corrupt stream is reported once");
    }
    let mut data = original;
    data[header_len + 4] ^= 0xff;
    let table = Table::from_bytes(&data)?;
    assert!(
        table.logs().next_record().is_err(),
        "invalid zlib bytes are not accepted as an empty log"
    );
    Ok(())
}

#[test]
fn log_keys_require_a_separator_and_an_in_range_update_index() -> crate::Result {
    let mut bad_separator = record(b"refs/a", 10, true);
    bad_separator[8] = 1;
    for bytes in [bad_separator, record(b"refs/a", 11, true)] {
        let data = log_table(&bytes, &[0])?;
        let table = Table::from_bytes(&data)?;
        assert!(
            table.logs().next_record().is_err(),
            "malformed log keys are explicit errors"
        );
    }
    Ok(())
}

#[test]
fn log_indices_may_precede_the_header_minimum() -> crate::Result {
    let data = log_table(&record(b"refs/a", 6, false), &[0])?;
    let table = Table::from_bytes(&data)?;
    let mut logs = table.logs();
    let record = logs.next_record()?.expect("historical log record");
    assert_eq!(record.update_index, 6);
    assert_eq!(record.data, None, "historical tombstones cancel older stack entries");
    Ok(())
}

#[test]
fn sequential_logs_stop_at_multilevel_index() -> crate::Result {
    let data = indexed_log_table()?;
    let table = Table::from_bytes(&data)?;
    let mut logs = table.logs();
    for name in ["refs/a", "refs/z"] {
        assert_eq!(
            logs.next_record()?.expect("each log block precedes the indexes").name,
            name
        );
    }
    assert!(
        logs.next_record()?.is_none(),
        "lower index levels are not decoded as log blocks"
    );
    Ok(())
}

#[test]
fn seek_uses_the_log_index_and_reverse_update_order() -> crate::Result {
    let data = indexed_log_table()?;
    let table = Table::from_bytes(&data)?;
    for (name, update_index, expected) in [
        ("refs/0", u64::MAX, Some(("refs/a", 10))),
        ("refs/a", u64::MAX, Some(("refs/a", 10))),
        ("refs/b", u64::MAX, Some(("refs/z", 9))),
        ("refs/z", u64::MAX, Some(("refs/z", 9))),
        ("refs/zz", u64::MAX, None),
    ] {
        let mut logs = table.logs();
        logs.seek(name.into(), update_index)?;
        assert_eq!(
            logs.next_record()?
                .map(|record| (record.name.to_str().expect("test names are UTF-8"), record.update_index)),
            expected,
            "{name}@{update_index} positions at the lower-bound log key"
        );
    }
    Ok(())
}

#[test]
fn seeking_beyond_the_last_unindexed_log_does_not_read_the_footer() -> crate::Result {
    let data = log_table(&record(b"refs/a", 10, true), &[0])?;
    let table = Table::from_bytes(&data)?;
    let mut logs = table.logs();
    for (name, update_index) in [("refs/z", u64::MAX), ("refs/a", 0)] {
        logs.seek(name.into(), update_index)?;
        assert!(
            logs.next_record()?.is_none(),
            "{name}@{update_index} follows the last log key"
        );
    }
    Ok(())
}

#[test]
fn log_index_root_blocks_follow_table_padding() -> crate::Result {
    let data = padded_root_log_index_table()?;
    let table = Table::from_bytes(&data)?;
    let mut logs = table.logs();
    logs.seek("refs/z".into(), u64::MAX)?;
    assert_eq!(
        logs.next_record()?
            .expect("the second padded root block points to refs/z")
            .name,
        "refs/z"
    );
    Ok(())
}

#[test]
fn native_log_records_agree_with_git() -> crate::Result {
    let mut actual = Vec::new();
    for path in super::table_paths()? {
        let data = std::fs::read(path)?;
        let table = Table::from_bytes(&data)?;
        let mut logs = table.logs();
        while let Some(entry) = logs.next_record()? {
            if entry.name != "refs/heads/topic" {
                continue;
            }
            let update = entry.data.expect("the fixture contains no deleted topic log entries");
            assert_eq!(update.committer.name, "committer");
            assert_eq!(update.committer.email, "committer@example.com");
            assert_eq!(
                update.time_seconds, 946_771_200,
                "the fixture's isolated timestamp is retained"
            );
            assert_eq!(update.timezone_minutes, 0, "the fixture uses UTC");
            actual.push((
                entry.update_index,
                update.previous_object_id.to_owned(),
                update.new_object_id.to_owned(),
                BString::from(update.message),
            ));
        }
    }
    actual.sort_by_key(|(index, ..)| std::cmp::Reverse(*index));
    let expected = super::baseline("files", "topic.log")?;
    let expected = expected.lines().collect::<Vec<_>>();
    assert_eq!(
        actual.len(),
        expected.len(),
        "native decoding returns every topic log entry"
    );
    for ((_, _, object_id, message), expected) in actual.iter().zip(expected) {
        let (hex, expected_message) = expected.split_once_str(" ").expect("Git emits an ID and a message");
        assert_eq!(
            *object_id,
            gix_hash::ObjectId::from_hex(hex)?,
            "log order agrees with Git"
        );
        assert_eq!(
            message
                .strip_suffix(b"\n")
                .expect("Git stores reflog messages with a final LF"),
            expected_message.as_bytes(),
            "Git's formatted subject agrees after removing only the stored record terminator"
        );
    }
    assert_eq!(
        actual.last().expect("topic creation exists").1,
        gix_testtools::object_hash().null(),
        "creation has no previous object"
    );
    assert_eq!(
        actual[0].1, actual[1].2,
        "the newer log record preserves the previous value from the earlier record"
    );
    Ok(())
}
