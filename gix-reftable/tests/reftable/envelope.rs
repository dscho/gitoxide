use gix_reftable::{Table, Version};

pub(super) fn empty_table(version: u8, hash: &[u8; 4]) -> Vec<u8> {
    let mut header = b"REFT".to_vec();
    header.push(version);
    header.extend_from_slice(&[0; 19]);
    if version == 2 {
        header.extend_from_slice(hash);
    }
    let mut data = header.clone();
    data.extend_from_slice(&header);
    data.extend_from_slice(&[0; 40]);
    let checksum = crc32fast::hash(&data[header.len()..]);
    data.extend_from_slice(&checksum.to_be_bytes());
    data
}

pub(super) fn update_checksum(data: &mut [u8]) {
    let header_len = if data[4] == 1 { 24 } else { 28 };
    let end = data.len() - 4;
    let start = end - header_len - 40;
    let checksum = crc32fast::hash(&data[start..end]);
    data[end..].copy_from_slice(&checksum.to_be_bytes());
}

pub(super) fn footer_field(data: &mut [u8], index: usize) -> &mut [u8] {
    let start = data.len() - 44 + index * 8;
    &mut data[start..start + 8]
}

#[test]
fn git_table_envelopes_identify_the_selected_hash() -> crate::Result {
    for path in super::table_paths()? {
        let data = std::fs::read(&path)?;
        let table = Table::from_bytes(&data)?;
        assert_eq!(
            table.header().object_hash,
            gix_testtools::object_hash(),
            "{}: the table declares the fixture's hash format",
            path.display()
        );
        assert!(
            table.header().min_update_index <= table.header().max_update_index,
            "{}: update-index bounds are ordered",
            path.display()
        );
        assert_eq!(table.as_bytes(), data, "a table borrows the original encoded bytes");
    }
    Ok(())
}

#[test]
fn empty_tables_support_both_versions_and_hash_ids() -> crate::Result {
    for (version, hash, expected_version, expected_hash) in [
        (1, b"sha1", Version::V1, 1),
        (2, b"sha1", Version::V2, 1),
        (2, b"s256", Version::V2, 2),
    ] {
        let data = empty_table(version, hash);
        let table = Table::from_bytes(&data)?;
        assert_eq!(table.header().version, expected_version, "the version is retained");
        assert_eq!(
            table.header().object_hash,
            gix_hash::Kind::try_from(expected_hash).expect("tests enable both hashes"),
            "version 1 implies SHA-1; version 2 uses its explicit hash ID"
        );
        assert_eq!(table.header().block_size, 0, "zero denotes an unaligned table");
        assert_eq!(
            table.header().min_update_index,
            0,
            "an empty table can have zero bounds"
        );
        assert_eq!(
            table.header().max_update_index,
            0,
            "an empty table can have zero bounds"
        );
    }
    Ok(())
}

#[test]
fn every_truncated_empty_envelope_is_rejected() {
    for version in [1, 2] {
        let data = empty_table(version, b"sha1");
        for end in 0..data.len() {
            assert!(
                Table::from_bytes(&data[..end]).is_err(),
                "version {version}: a {end}-byte prefix is not a complete table"
            );
        }
    }
}

#[test]
fn signatures_versions_hash_ids_and_header_copies_are_checked() {
    for offset in [0, 4, 8, 24, 28, 32] {
        let mut data = empty_table(2, b"sha1");
        data[offset] ^= 0xff;
        update_checksum(&mut data);
        let error = Table::from_bytes(&data).expect_err("the modified envelope must be rejected");
        assert!(
            if offset == 4 {
                error.is_unsupported()
            } else {
                error.is_corrupted()
            },
            "changing byte {offset} cannot be hidden by updating the footer checksum"
        );
    }
    let mut data = empty_table(2, b"sha1");
    data[24..28].copy_from_slice(b"xxxx");
    data[52..56].copy_from_slice(b"xxxx");
    update_checksum(&mut data);
    assert!(
        Table::from_bytes(&data)
            .expect_err("the unknown hash must be rejected")
            .is_unsupported(),
        "a matching header and footer still need a recognized hash ID"
    );
}

#[test]
fn checksums_and_update_index_bounds_are_checked() {
    let mut data = empty_table(1, b"sha1");
    let last = data.len() - 1;
    data[last] ^= 1;
    assert!(
        Table::from_bytes(&data).is_err(),
        "a changed footer checksum is rejected"
    );

    let mut data = empty_table(1, b"sha1");
    data[8..16].copy_from_slice(&2_u64.to_be_bytes());
    data[32..40].copy_from_slice(&2_u64.to_be_bytes());
    update_checksum(&mut data);
    assert!(
        Table::from_bytes(&data).is_err(),
        "the minimum update index cannot exceed the maximum"
    );
}

#[test]
fn section_offsets_cannot_point_outside_the_table_body() {
    for section in 0..5 {
        for offset in [1_u64, 24, u64::MAX >> 5] {
            let mut data = empty_table(1, b"sha1");
            let value = if section == 1 { (offset << 5) | 2 } else { offset };
            footer_field(&mut data, section).copy_from_slice(&value.to_be_bytes());
            update_checksum(&mut data);
            assert!(
                Table::from_bytes(&data).is_err(),
                "section {section} at offset {offset} is not in an empty table's body"
            );
        }
    }
}

#[test]
fn section_positions_require_the_advertised_block_type() {
    let mut data = empty_table(1, b"sha1");
    data.splice(24..24, b"rx".iter().copied());
    footer_field(&mut data, 3).copy_from_slice(&25_u64.to_be_bytes());
    update_checksum(&mut data);
    assert!(
        Table::from_bytes(&data).is_err(),
        "the advertised log section must start with a log block"
    );
}

#[test]
fn absent_object_sections_require_zero_abbreviation_length() {
    let mut data = empty_table(1, b"sha1");
    footer_field(&mut data, 1).copy_from_slice(&1_u64.to_be_bytes());
    update_checksum(&mut data);
    assert!(
        Table::from_bytes(&data).is_err(),
        "an absent object section cannot retain an abbreviation length"
    );
}
