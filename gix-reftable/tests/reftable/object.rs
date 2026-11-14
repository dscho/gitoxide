use gix_reftable::Table;

fn object_table(count: u8, large_count: Option<u64>, offsets: &[u64]) -> Vec<u8> {
    let mut data = super::reference::empty_table();
    let header_len = if data[4] == 1 { 24 } else { 28 };
    let footer = data.split_off(header_len);
    let refs = super::reference::named_object_record(b"refs/a", 1);
    data.extend(super::reference::block(b'r', header_len, &refs, &[0]));
    let object_offset = data.len();
    let prefix = [0xaa, 0xbb];
    let mut record = vec![0, (prefix.len() as u8) << 3 | count];
    record.extend_from_slice(&prefix);
    if let Some(count) = large_count {
        super::log::varint(count, &mut record);
    }
    let mut previous = 0;
    for (index, offset) in offsets.iter().enumerate() {
        super::log::varint(if index == 0 { *offset } else { offset - previous }, &mut record);
        previous = *offset;
    }
    data.extend(super::reference::block(b'o', 0, &record, &[0]));
    data.extend_from_slice(&footer);
    super::envelope::footer_field(&mut data, 1).copy_from_slice(&(((object_offset as u64) << 5) | 2).to_be_bytes());
    super::envelope::update_checksum(&mut data);
    data
}

fn object_record(prefix: &[u8]) -> Vec<u8> {
    let mut record = vec![0, (prefix.len() as u8) << 3 | 1];
    record.extend_from_slice(prefix);
    super::log::varint(0, &mut record);
    record
}

fn indexed_object_table() -> Vec<u8> {
    let mut data = super::reference::empty_table();
    let header_len = if data[4] == 1 { 24 } else { 28 };
    let footer = data.split_off(header_len);
    let refs = super::reference::named_object_record(b"refs/a", 1);
    data.extend(super::reference::block(b'r', header_len, &refs, &[0]));

    let first_offset = data.len();
    data.extend(super::reference::block(b'o', 0, &object_record(&[0x11, 0x11]), &[0]));
    let second_offset = data.len();
    data.extend(super::reference::block(b'o', 0, &object_record(&[0xee, 0xee]), &[0]));
    let lower_offset = data.len();
    let mut lower = super::reference::index_record(&[0x11, 0x11], first_offset);
    let second_restart = lower.len();
    lower.extend(super::reference::index_record(&[0xee, 0xee], second_offset));
    data.extend(super::reference::block(b'i', 0, &lower, &[0, second_restart]));
    let root_offset = data.len();
    let root = super::reference::index_record(&[0xee, 0xee], lower_offset);
    data.extend(super::reference::block(b'i', 0, &root, &[0]));
    data.extend_from_slice(&footer);
    super::envelope::footer_field(&mut data, 1).copy_from_slice(&(((first_offset as u64) << 5) | 2).to_be_bytes());
    super::envelope::footer_field(&mut data, 2).copy_from_slice(&(root_offset as u64).to_be_bytes());
    super::envelope::update_checksum(&mut data);
    data
}

fn set_object_id_len(data: &mut [u8], len: u8) {
    let field = super::envelope::footer_field(data, 1);
    field[7] = (field[7] & !31) | len;
    super::envelope::update_checksum(data);
}

#[test]
fn absent_object_sections_are_empty() -> crate::Result {
    let data = super::reference::empty_table();
    let table = Table::from_bytes(&data)?;
    assert!(table.objects().next_record()?.is_none());
    Ok(())
}

#[test]
fn inline_and_large_offset_counts_are_delta_decoded() -> crate::Result {
    for (count, large_count) in [(3, None), (0, Some(3))] {
        let data = object_table(count, large_count, &[0, 7, 13]);
        let table = Table::from_bytes(&data)?;
        let mut objects = table.objects();
        let object = objects.next_record()?.expect("one reverse object record");
        assert_eq!(object.hash_prefix, [0xaa, 0xbb]);
        assert_eq!(object.block_offsets, [0, 7, 13]);
        assert!(objects.next_record()?.is_none());
    }
    Ok(())
}

#[test]
fn a_zero_large_count_requests_a_reference_scan() -> crate::Result {
    let data = object_table(0, Some(0), &[]);
    let table = Table::from_bytes(&data)?;
    let mut objects = table.objects();
    let object = objects.next_record()?.expect("one reverse object record");
    assert!(
        object.block_offsets.is_empty(),
        "an empty list is the format's scan-all marker"
    );
    Ok(())
}

#[test]
fn sequential_objects_stop_at_multilevel_index() -> crate::Result {
    let data = indexed_object_table();
    let table = Table::from_bytes(&data)?;
    let mut objects = table.objects();
    for prefix in [[0x11, 0x11], [0xee, 0xee]] {
        assert_eq!(
            objects
                .next_record()?
                .expect("object block precedes the indexes")
                .hash_prefix,
            prefix
        );
    }
    assert!(
        objects.next_record()?.is_none(),
        "lower index levels are not decoded as object records"
    );
    Ok(())
}

#[test]
fn object_abbreviations_need_at_least_two_bytes() {
    let mut data = object_table(1, None, &[0]);
    set_object_id_len(&mut data, 1);
    assert!(
        Table::from_bytes(&data).is_err(),
        "the object abbreviation length must be at least two bytes"
    );
}

#[test]
fn invalid_counts_offsets_and_abbreviation_lengths_are_rejected() -> crate::Result {
    for data in [
        object_table(0, Some(8), &[0]),
        object_table(2, None, &[7, 7]),
        object_table(2, None, &[0, u64::MAX]),
    ] {
        let table = Table::from_bytes(&data)?;
        assert!(table.objects().next_record().is_err());
    }
    let mut data = object_table(1, None, &[0]);
    set_object_id_len(&mut data, 3);
    let table = Table::from_bytes(&data)?;
    assert!(
        table.objects().next_record().is_err(),
        "record prefixes must match the footer width"
    );
    Ok(())
}

#[test]
fn seek_uses_the_object_index() -> crate::Result {
    let data = indexed_object_table();
    let table = Table::from_bytes(&data)?;
    for (query, expected) in [
        ([0, 0], Some([0x11, 0x11])),
        ([0x11, 0x11], Some([0x11, 0x11])),
        ([0x22, 0x22], Some([0xee, 0xee])),
        ([0xee, 0xee], Some([0xee, 0xee])),
        ([0xff, 0xff], None),
    ] {
        let mut objects = table.objects();
        objects.seek(&query)?;
        assert_eq!(
            objects
                .next_record()?
                .map(|record| record.hash_prefix.try_into().expect("two-byte test prefix")),
            expected,
            "{query:?} positions at the lower-bound object abbreviation"
        );
    }
    Ok(())
}
