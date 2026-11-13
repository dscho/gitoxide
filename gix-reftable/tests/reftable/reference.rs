use std::collections::BTreeMap;

use gix_reftable::{Table, reference::ValueRef};
use gix_testtools::bstr::{BStr, ByteSlice};

pub(super) fn table_with_records(records: &[u8], restarts: &[usize]) -> Vec<u8> {
    let mut data = empty_table();
    let header_len = if data[4] == 1 { 24 } else { 28 };
    data.splice(header_len..header_len, block(b'r', header_len, records, restarts));
    data
}

pub(super) fn empty_table() -> Vec<u8> {
    let (version, hash) = if gix_testtools::object_hash().len_in_bytes() == 20 {
        (1, b"sha1")
    } else {
        (2, b"s256")
    };
    let header_len = if version == 1 { 24 } else { 28 };
    let mut data = super::envelope::empty_table(version, hash);
    for base in [0, header_len] {
        data[base + 8..base + 16].copy_from_slice(&7_u64.to_be_bytes());
        data[base + 16..base + 24].copy_from_slice(&10_u64.to_be_bytes());
    }
    super::envelope::update_checksum(&mut data);
    data
}

pub(super) fn block(kind: u8, header_skip: usize, records: &[u8], restarts: &[usize]) -> Vec<u8> {
    let size = u32::try_from(header_skip + 4 + records.len() + 3 * restarts.len() + 2).expect("small test block");
    let mut block = vec![kind];
    block.extend_from_slice(&size.to_be_bytes()[1..]);
    block.extend_from_slice(records);
    for restart in restarts {
        let offset = u32::try_from(header_skip + 4 + restart).expect("small test offset");
        block.extend_from_slice(&offset.to_be_bytes()[1..]);
    }
    block.extend_from_slice(&u16::try_from(restarts.len()).expect("few test restarts").to_be_bytes());
    block
}

pub(super) fn index_record(name: &[u8], offset: usize) -> Vec<u8> {
    let mut record = vec![0];
    super::log::varint((name.len() as u64) << 3, &mut record);
    record.extend_from_slice(name);
    super::log::varint(offset as u64, &mut record);
    record
}

fn object_record() -> Vec<u8> {
    let mut record = vec![0, (6 << 3) | 1];
    record.extend_from_slice(b"refs/a");
    record.push(0);
    record.extend(std::iter::repeat_n(1, gix_testtools::object_hash().len_in_bytes()));
    record
}

#[test]
fn prefix_compressed_references_retain_all_value_kinds() -> crate::Result {
    let hash_len = gix_testtools::object_hash().len_in_bytes();
    let one = vec![1; hash_len];
    let two = vec![2; hash_len];
    let three = vec![3; hash_len];
    let mut records = object_record();
    records.extend_from_slice(&[6, (1 << 3) | 2, b'b', 1]);
    records.extend_from_slice(&two);
    records.extend_from_slice(&three);
    records.extend_from_slice(&[6, (1 << 3) | 3, b'c', 2, 6]);
    records.extend_from_slice(b"refs/a");
    records.extend_from_slice(&[6, 1 << 3, b'd', 3]);
    let data = table_with_records(&records, &[0]);
    let table = Table::from_bytes(&data)?;
    let mut iter = table.references();
    for (name, update_index, value) in [
        (
            "refs/a",
            7,
            ValueRef::Object {
                object_id: gix_hash::oid::try_from_bytes(&one)?,
                peeled_id: None,
            },
        ),
        (
            "refs/ab",
            8,
            ValueRef::Object {
                object_id: gix_hash::oid::try_from_bytes(&two)?,
                peeled_id: Some(gix_hash::oid::try_from_bytes(&three)?),
            },
        ),
        ("refs/ac", 9, ValueRef::Symbolic("refs/a".into())),
        ("refs/ad", 10, ValueRef::Deletion),
    ] {
        let reference = iter.next_record()?.expect("each encoded record must be returned");
        assert_eq!(
            reference.name, name,
            "prefix compression reconstructs the complete name"
        );
        assert_eq!(
            reference.update_index, update_index,
            "indices include the header's minimum"
        );
        assert_eq!(reference.value, value, "the record type preserves its value");
    }
    assert!(iter.next_record()?.is_none(), "the block ends once");
    assert!(iter.next_record()?.is_none(), "exhaustion is stable");
    Ok(())
}

#[test]
fn empty_tables_have_no_references() -> crate::Result {
    let data = super::envelope::empty_table(1, b"sha1");
    let table = Table::from_bytes(&data)?;
    assert!(
        table.references().next_record()?.is_none(),
        "the footer must not be interpreted as a reference block"
    );
    Ok(())
}

#[test]
fn truncated_reference_values_and_invalid_restarts_report_errors() -> crate::Result {
    let record = object_record();
    for end in 0..record.len() {
        let data = table_with_records(&record[..end], &[0]);
        let table = Table::from_bytes(&data)?;
        let mut iter = table.references();
        assert!(
            iter.next_record().is_err(),
            "a {end}-byte prefix is not a complete record"
        );
        assert!(
            iter.next_record()?.is_none(),
            "iteration stops after reporting corruption"
        );
    }
    for restarts in [vec![], vec![1], vec![0, 0], vec![record.len()]] {
        let data = table_with_records(&record, &restarts);
        let table = Table::from_bytes(&data)?;
        assert!(
            table.references().next_record().is_err(),
            "restart offsets {restarts:?} do not describe complete independent keys"
        );
    }
    Ok(())
}

#[test]
fn invalid_prefixes_types_and_update_indices_are_rejected() -> crate::Result {
    let record = object_record();
    for (offset, value) in [(0, 1), (1, (6 << 3) | 4), (8, 4)] {
        let mut record = record.clone();
        record[offset] = value;
        let data = table_with_records(&record, &[0]);
        let table = Table::from_bytes(&data)?;
        assert!(
            table.references().next_record().is_err(),
            "changing byte {offset} to {value} violates the reference encoding"
        );
    }
    let mut overflow = record.clone();
    overflow.splice(8..9, b"\x80\xfe\xfe\xfe\xfe\xfe\xfe\xfe\xfe\x7f".iter().copied());
    let data = table_with_records(&overflow, &[0]);
    let table = Table::from_bytes(&data)?;
    assert!(
        table.references().next_record().is_err(),
        "adding the minimum update index cannot wrap a valid u64 delta"
    );

    let data = table_with_records(&record.repeat(2), &[0]);
    let table = Table::from_bytes(&data)?;
    let mut iter = table.references();
    assert!(iter.next_record()?.is_some());
    assert!(
        iter.next_record().is_err(),
        "a table cannot contain duplicate reference keys"
    );
    Ok(())
}

#[test]
fn unadvertised_section_transitions_are_errors() -> crate::Result {
    let record = object_record();
    let mut data = table_with_records(&record, &[0]);
    let header_len = if data[4] == 1 { 24 } else { 28 };
    let footer = data.split_off(data.len() - header_len - 44);
    data.extend(block(b'o', 0, &record, &[0]));
    data.extend_from_slice(&footer);
    super::envelope::update_checksum(&mut data);

    let table = Table::from_bytes(&data)?;
    let mut references = table.references();
    assert!(references.next_record()?.is_some());
    assert!(
        references.next_record().is_err(),
        "an object block without an advertised object section is corruption"
    );
    Ok(())
}

#[test]
fn aligned_blocks_skip_padding_but_allow_an_unpadded_last_block() -> crate::Result {
    let record = object_record();
    let mut data = table_with_records(&record, &[0]);
    let header_len = if data[4] == 1 { 24 } else { 28 };
    let mut footer = data.split_off(data.len() - header_len - 44);
    let padding_start = data.len();
    for header in [&mut data, &mut footer] {
        header[5..8].copy_from_slice(&128_u32.to_be_bytes()[1..]);
    }
    data.resize(128, 0);
    let mut second = record;
    second[7] = b'b';
    data.extend(block(b'r', 0, &second, &[0]));
    data.extend_from_slice(&footer);
    super::envelope::update_checksum(&mut data);

    let table = Table::from_bytes(&data)?;
    let mut iter = table.references();
    for name in ["refs/a", "refs/b"] {
        assert_eq!(
            iter.next_record()?.expect("two reference blocks").name,
            name,
            "the file header belongs only to the first block"
        );
    }
    assert!(iter.next_record()?.is_none());

    data[padding_start + 1] = 1;
    let table = Table::from_bytes(&data)?;
    assert!(
        table.references().next_record().is_err(),
        "alignment padding is required to contain only NUL bytes"
    );
    Ok(())
}

#[test]
fn names_are_bytes_and_not_utf8_strings() -> crate::Result {
    let data = table_with_records(&[0, 2 << 3, 0xff, 0xfe, 0], &[0]);
    let table = Table::from_bytes(&data)?;
    let mut iter = table.references();
    assert_eq!(
        iter.next_record()?.expect("one binary name").name,
        b"\xff\xfe".as_bstr(),
        "the storage layer does not impose Unicode or reference-name policy"
    );
    Ok(())
}

#[test]
fn decoded_git_records_match_the_recorded_fixture_values() -> crate::Result {
    let mut objects = BTreeMap::new();
    let mut symbolic = BTreeMap::new();
    // Combining the fixture's ordered tables is an oracle, not a production stack implementation.
    for path in super::table_paths()? {
        let data = std::fs::read(path)?;
        let table = Table::from_bytes(&data)?;
        let mut iter = table.references();
        while let Some(reference) = iter.next_record()? {
            match reference.value {
                ValueRef::Deletion => {
                    objects.remove(reference.name);
                    symbolic.remove(reference.name);
                }
                ValueRef::Object { object_id, peeled_id } => {
                    symbolic.remove(reference.name);
                    objects.insert(
                        reference.name.to_owned(),
                        (object_id.to_owned(), peeled_id.map(ToOwned::to_owned)),
                    );
                }
                ValueRef::Symbolic(target) => {
                    objects.remove(reference.name);
                    symbolic.insert(reference.name.to_owned(), target.to_owned());
                }
            }
        }
    }
    let mut names = objects
        .keys()
        .chain(symbolic.keys())
        .filter(|name| name.starts_with_str("refs/"))
        .collect::<Vec<_>>();
    names.sort();
    let expected = super::baseline("files", "refs")?;
    assert_eq!(
        names,
        expected.lines().map(ByteSlice::as_bstr).collect::<Vec<_>>(),
        "native decoding returns the same reference names as Git"
    );
    let first_commit_id = gix_hash::ObjectId::from_hex(super::baseline("files", "first.id")?.trim_end())?;
    let second_commit_id = gix_hash::ObjectId::from_hex(super::baseline("files", "second.id")?.trim_end())?;
    let tag_id = gix_hash::ObjectId::from_hex(super::baseline("files", "annotated.id")?.trim_end())?;
    assert_eq!(
        objects.get(BStr::new("refs/heads/main")),
        Some(&(second_commit_id, None)),
        "direct values retain the full object ID"
    );
    assert_eq!(
        objects.get(BStr::new("refs/tags/annotated")),
        Some(&(tag_id, Some(first_commit_id))),
        "annotated tags retain both the tag object and its peeled target"
    );
    assert_eq!(
        symbolic.get(BStr::new("HEAD")).expect("the table stores HEAD"),
        "refs/heads/main",
        "HEAD is decoded as a symbolic record without resolving it"
    );
    Ok(())
}
