use gix_reftable::{Header, ReferenceRef, Table, Version, reference::ValueRef, write::Writer};
use gix_testtools::bstr::{BStr, ByteSlice};

fn header() -> Header {
    let object_hash = gix_testtools::object_hash();
    Header {
        version: if object_hash == gix_hash::Kind::Sha1 {
            Version::V1
        } else {
            Version::V2
        },
        object_hash,
        block_size: 0,
        min_update_index: 18,
        max_update_index: 18,
    }
}

fn deletion(name: &BStr, update_index: u64) -> ReferenceRef<'_> {
    ReferenceRef {
        name,
        update_index,
        value: ValueRef::Deletion,
    }
}

fn reference_index_root(bytes: &[u8]) -> crate::Result<usize> {
    Ok(usize::try_from(u64::from_be_bytes(
        bytes[bytes.len() - 44..bytes.len() - 36].try_into()?,
    ))?)
}

#[derive(Default)]
struct FailOnce {
    failed: bool,
    bytes: Vec<u8>,
}

impl std::io::Write for FailOnce {
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        if !std::mem::replace(&mut self.failed, true) {
            Err(std::io::Error::other("injected write failure"))
        } else {
            std::io::Write::write(&mut self.bytes, input)
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn empty_tables_have_a_complete_envelope() -> crate::Result {
    let header = header();
    let bytes = Writer::new(Vec::new(), header)?.finish()?;
    assert_eq!(
        bytes.len(),
        if header.version == Version::V1 { 92 } else { 100 },
        "an empty table consists of its header and footer"
    );
    let table = Table::from_bytes(&bytes)?;
    assert_eq!(*table.header(), header, "the writer preserves requested metadata");
    assert!(
        table.references().next_record()?.is_none(),
        "empty tables have no references"
    );
    Ok(())
}

#[test]
fn version_two_can_write_sha1() -> crate::Result {
    let mut header = header();
    header.version = Version::V2;
    header.object_hash = gix_hash::Kind::Sha1;
    let mut writer = Writer::new(Vec::new(), header)?;
    writer.push_reference(deletion(BStr::new(b"refs/heads/example"), 18))?;
    let bytes = writer.finish()?;
    let table = Table::from_bytes(&bytes)?;
    assert_eq!(*table.header(), header, "version 2 advertises its SHA-1 hash ID");
    assert_eq!(
        table.references().next_record()?.expect("written reference").name,
        "refs/heads/example"
    );
    Ok(())
}

#[test]
fn stored_reference_values_roundtrip() -> crate::Result {
    let object_hash = gix_testtools::object_hash();
    let direct_bytes = vec![1; object_hash.len_in_bytes()];
    let peeled_bytes = vec![2; object_hash.len_in_bytes()];
    let direct = gix_hash::oid::try_from_bytes(&direct_bytes)?;
    let peeled = gix_hash::oid::try_from_bytes(&peeled_bytes)?;
    let records = [
        (
            BStr::new(b"refs/heads/alpha"),
            ValueRef::Object {
                object_id: direct,
                peeled_id: None,
            },
        ),
        (
            BStr::new(b"refs/heads/beta"),
            ValueRef::Symbolic(BStr::new(b"refs/heads/alpha")),
        ),
        (BStr::new(b"refs/heads/gone"), ValueRef::Deletion),
        (
            BStr::new(b"refs/tags/peeled"),
            ValueRef::Object {
                object_id: direct,
                peeled_id: Some(peeled),
            },
        ),
    ];
    let mut header = header();
    header.max_update_index = 21;
    let mut writer = Writer::new(Vec::new(), header)?;
    for (index, (name, value)) in records.into_iter().enumerate() {
        writer.push_reference(ReferenceRef {
            name,
            update_index: 18 + index as u64,
            value,
        })?;
    }
    let bytes = writer.finish()?;
    let table = Table::from_bytes(&bytes)?;
    let mut references = table.references();
    for (index, (name, value)) in records.into_iter().enumerate() {
        let reference = references.next_record()?.expect("every written record is readable");
        assert_eq!(reference.name, name, "reference names remain in byte order");
        assert_eq!(
            reference.update_index,
            18 + index as u64,
            "update indices retain their full value"
        );
        assert_eq!(reference.value, value, "stored values preserve their type and contents");
    }
    assert!(references.next_record()?.is_none(), "the writer emits no extra records");
    Ok(())
}

#[test]
fn invalid_records_leave_the_writer_usable() -> crate::Result {
    let mut writer = Writer::new(Vec::new(), header())?;
    writer.push_reference(deletion(BStr::new(b"refs/b"), 18))?;
    for (record, reason) in [
        (deletion(BStr::new(b"refs/b"), 18), "duplicate names"),
        (deletion(BStr::new(b"refs/a"), 18), "unsorted names"),
        (deletion(BStr::new(b"refs/c"), 17), "below-range update indices"),
        (deletion(BStr::new(b"refs/c"), 19), "out-of-range update indices"),
    ] {
        assert!(writer.push_reference(record).is_err(), "{reason} must not be encoded");
    }
    let wrong_len = if header().object_hash == gix_hash::Kind::Sha1 {
        32
    } else {
        20
    };
    let wrong_bytes = vec![1; wrong_len];
    let wrong_id = gix_hash::oid::try_from_bytes(&wrong_bytes)?;
    assert!(
        writer
            .push_reference(ReferenceRef {
                name: BStr::new(b"refs/c"),
                update_index: 18,
                value: ValueRef::Object {
                    object_id: wrong_id,
                    peeled_id: None,
                },
            })
            .is_err(),
        "object IDs must use the table's hash kind"
    );
    writer.push_reference(deletion(BStr::new(b"refs/c"), 18))?;
    let bytes = writer.finish()?;
    let table = Table::from_bytes(&bytes)?;
    let mut references = table.references();
    for name in ["refs/b", "refs/c"] {
        assert_eq!(references.next_record()?.expect("valid record").name, name);
    }
    assert!(
        references.next_record()?.is_none(),
        "invalid edits must not leave records behind"
    );

    let mut invalid_header = header();
    invalid_header.min_update_index = 19;
    assert!(
        Writer::new(Vec::new(), invalid_header).is_err(),
        "header bounds must be ordered"
    );
    invalid_header = header();
    invalid_header.block_size = 1 << 24;
    assert!(
        Writer::new(Vec::new(), invalid_header).is_err(),
        "block sizes must fit the format"
    );
    if invalid_header.object_hash == gix_hash::Kind::Sha256 {
        invalid_header.version = Version::V1;
        invalid_header.block_size = 0;
        assert!(
            Writer::new(Vec::new(), invalid_header).is_err(),
            "version 1 implies SHA-1"
        );
    }
    let mut bounded_header = header();
    bounded_header.block_size = 64;
    let mut writer = Writer::new(Vec::new(), bounded_header)?;
    let long_name = format!("refs/heads/{}", "a".repeat(64));
    assert!(
        writer
            .push_reference(deletion(BStr::new(long_name.as_bytes()), 18))
            .is_err(),
        "oversized records do not fill the block"
    );
    writer.push_reference(deletion(BStr::new(b"refs/ok"), 18))?;
    let bytes = writer.finish()?;
    let table = Table::from_bytes(&bytes)?;
    assert_eq!(table.references().next_record()?.expect("valid record").name, "refs/ok");
    Ok(())
}

#[test]
fn write_errors_identify_the_failed_section() -> crate::Result {
    for (capacity, section) in [(0, "block"), (32, "footer")] {
        let mut output = vec![0; capacity];
        let error = Writer::new(&mut output[..], header())?
            .finish()
            .expect_err("the output cannot hold a table");
        assert!(error.to_string().contains(section), "writing the {section} must fail");
    }
    Ok(())
}

#[test]
fn aligned_reference_blocks_have_padding_and_an_index() -> crate::Result {
    let mut header = header();
    header.block_size = 128;
    let mut writer = Writer::new(Vec::new(), header)?;
    for index in 0..40 {
        let name = format!("refs/heads/{index:02}");
        writer.push_reference(deletion(name.as_bytes().as_bstr(), 18))?;
    }
    let bytes = writer.finish()?;
    let block_header = if header.version == Version::V1 { 24 } else { 28 };
    let first_len = u32::from_be_bytes([
        0,
        bytes[block_header + 1],
        bytes[block_header + 2],
        bytes[block_header + 3],
    ]) as usize;
    assert!(first_len < 128, "the first block leaves room for padding");
    assert!(
        bytes[first_len..128].iter().all(|byte| *byte == 0),
        "the first block is NUL-padded"
    );
    assert_eq!(bytes[128], b'r', "the next reference block starts at the alignment");
    let root = reference_index_root(&bytes)?;
    assert!(
        root > 128 && root.is_multiple_of(128),
        "the index follows aligned ref blocks"
    );
    assert_eq!(bytes[root], b'i', "the footer points to the reference index");

    let table = Table::from_bytes(&bytes)?;
    let mut cursor = table.references();
    for index in 0..40 {
        let name = format!("refs/heads/{index:02}");
        assert_eq!(
            cursor.next_record()?.expect("each written reference is readable").name,
            name.as_bytes().as_bstr()
        );
    }
    assert!(cursor.next_record()?.is_none(), "the index is not a reference block");
    for index in [0, 19, 39] {
        let name = format!("refs/heads/{index:02}");
        cursor.seek(name.as_bytes().as_bstr())?;
        assert_eq!(
            cursor.next_record()?.expect("indexed reference").name,
            name.as_bytes().as_bstr()
        );
    }
    Ok(())
}

#[test]
fn unaligned_reference_blocks_are_indexed() -> crate::Result {
    let mut writer = Writer::new(Vec::new(), header())?;
    for index in 0..800 {
        let name = format!("refs/heads/{index:04}");
        writer.push_reference(deletion(name.as_bytes().as_bstr(), 18))?;
    }
    let bytes = writer.finish()?;
    let root = reference_index_root(&bytes)?;
    assert!(root > 4096, "multiple unaligned blocks require an index");
    assert_eq!(bytes[root], b'i', "the footer points to the reference index");
    let table = Table::from_bytes(&bytes)?;
    let mut cursor = table.references();
    for index in [0, 200, 799] {
        let name = format!("refs/heads/{index:04}");
        cursor.seek(name.as_bytes().as_bstr())?;
        assert_eq!(
            cursor.next_record()?.expect("indexed reference").name,
            name.as_bytes().as_bstr()
        );
    }
    Ok(())
}

#[test]
fn an_oversized_restart_does_not_flush_an_incomplete_table() -> crate::Result {
    let mut header = header();
    header.block_size = 128;
    let first = format!("refs/heads/{}", "a".repeat(50));
    let oversized = format!("{first}/{}", "b".repeat(60));
    let mut writer = Writer::new(Vec::new(), header)?;
    writer.push_reference(deletion(first.as_bytes().as_bstr(), 18))?;
    assert!(
        writer
            .push_reference(deletion(oversized.as_bytes().as_bstr(), 18))
            .is_err(),
        "a key that fits only with prefix compression cannot start a new block"
    );
    let bytes = writer.finish()?;
    let table = Table::from_bytes(&bytes)?;
    let mut cursor = table.references();
    assert_eq!(
        cursor.next_record()?.expect("first reference").name,
        first.as_bytes().as_bstr()
    );
    assert!(
        cursor.next_record()?.is_none(),
        "a rejected record leaves a complete table"
    );
    Ok(())
}

#[test]
fn a_failed_block_write_cannot_be_finished() -> crate::Result {
    let mut header = header();
    header.block_size = 128;
    let mut writer = Writer::new(FailOnce::default(), header)?;
    let mut failed_on_write = false;
    for index in 0..40 {
        let name = format!("refs/heads/{index:02}");
        if let Err(error) = writer.push_reference(deletion(name.as_bytes().as_bstr(), 18)) {
            assert!(
                error.to_string().contains("Could not write the reference block"),
                "the first full block fails while writing"
            );
            failed_on_write = true;
            break;
        }
    }
    assert!(failed_on_write, "a full block must reach the failing output");
    assert!(
        writer
            .push_reference(deletion(BStr::new(b"refs/heads/99"), 18))
            .is_err(),
        "a failed output must reject further records"
    );
    assert!(
        writer.finish().is_err(),
        "a failed write cannot produce a complete table"
    );
    Ok(())
}

#[test]
fn git_reads_a_native_table_appended_to_a_disposable_stack() -> crate::Result {
    if *gix_testtools::GIT_VERSION < (2, 45, 0) {
        return Ok(());
    }
    let first = super::baseline("files", "first.id")?;
    let object_id = gix_hash::ObjectId::from_hex(first.trim_end())?;
    let filename = "0x000000000012-0x000000000012-00000001.ref";
    for block_size in [0, 128] {
        let dir = super::writable_fixture("reftable")?;
        let repo = dir.path().join("repo");
        let reftable = repo.join(".git/reftable");
        let mut table_header = header();
        table_header.block_size = block_size;
        let mut writer = Writer::new(std::fs::File::create(reftable.join(filename))?, table_header)?;
        writer.push_reference(deletion(BStr::new(b"refs/heads/topic"), 18))?;
        writer.push_reference(ReferenceRef {
            name: BStr::new(b"refs/heads/written"),
            update_index: 18,
            value: ValueRef::Object {
                object_id: object_id.as_ref(),
                peeled_id: None,
            },
        })?;
        for index in 0..160 {
            let name = format!("refs/heads/written-{index:03}");
            writer.push_reference(ReferenceRef {
                name: name.as_bytes().as_bstr(),
                update_index: 18,
                value: ValueRef::Object {
                    object_id: object_id.as_ref(),
                    peeled_id: None,
                },
            })?;
        }
        writer.push_reference(ReferenceRef {
            name: BStr::new(b"refs/heads/written-alias"),
            update_index: 18,
            value: ValueRef::Symbolic(BStr::new(b"refs/heads/written")),
        })?;
        writer.finish()?;
        let manifest = reftable.join("tables.list");
        let mut names = std::fs::read(&manifest)?;
        names.extend_from_slice(filename.as_bytes());
        names.push(b'\n');
        std::fs::write(manifest, names)?;
        let listed = gix_testtools::git(&repo, "for-each-ref --format=%(refname)")?;
        assert!(
            listed
                .as_bytes()
                .split(|byte| *byte == b'\n')
                .any(|line| line == b"refs/heads/written-159".as_slice()),
            "Git lists the last reference with {block_size}-byte alignment: {listed:?}"
        );
        for name in [
            "refs/heads/written",
            "refs/heads/written-159",
            "refs/heads/written-alias",
        ] {
            let resolved = gix_testtools::git(&repo, &format!("rev-parse --verify {name}"))?;
            assert_eq!(resolved.as_bytes().as_bstr(), first.as_bstr(), "Git resolves {name}");
        }
        assert!(
            gix_testtools::git(&repo, "rev-parse --verify refs/heads/topic").is_err(),
            "a written tombstone hides a reference from older tables"
        );
    }
    Ok(())
}
