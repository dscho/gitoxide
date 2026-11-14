use gix_reftable::{LogRef, ReferenceRef, Table, log::DataRef, reference::ValueRef, write::Writer};
use gix_testtools::bstr::{BStr, ByteSlice};

fn object_ids() -> crate::Result<(gix_hash::ObjectId, gix_hash::ObjectId)> {
    let len = gix_testtools::object_hash().len_in_bytes();
    Ok((
        gix_hash::ObjectId::from_hex(&b"11".repeat(len))?,
        gix_hash::ObjectId::from_hex(&b"22".repeat(len))?,
    ))
}

fn update<'a>(old: &'a gix_hash::oid, new: &'a gix_hash::oid, message: &'a BStr) -> DataRef<'a> {
    DataRef {
        previous_object_id: old,
        new_object_id: new,
        committer: gix_actor::IdentityRef {
            name: BStr::new(b"native"),
            email: BStr::new(b"native@example.com"),
        },
        time_seconds: 1_700_000_000,
        timezone_minutes: -150,
        message,
    }
}

fn log<'a>(name: &'a BStr, update_index: u64, data: Option<DataRef<'a>>) -> LogRef<'a> {
    LogRef {
        name,
        update_index,
        data,
    }
}

fn log_index_root(bytes: &[u8]) -> crate::Result<usize> {
    Ok(usize::try_from(u64::from_be_bytes(
        bytes[bytes.len() - 12..bytes.len() - 4].try_into()?,
    ))?)
}

#[test]
fn compressed_log_only_tables_preserve_updates_and_deletions() -> crate::Result {
    let (old, new) = object_ids()?;
    let mut data = update(old.as_ref(), new.as_ref(), BStr::new(b"message\xff"));
    data.time_seconds = u64::MAX;
    let entry = log(BStr::new(b"refs/heads/topic"), 18, Some(data));
    let deleted = log(BStr::new(b"refs/stash"), 17, None);
    let mut writer = Writer::new(Vec::new(), super::write::header())?;
    writer.push_log(entry)?;
    writer.push_log(deleted)?;
    let bytes = writer.finish()?;
    let table = Table::from_bytes(&bytes)?;
    assert!(
        table.references().next_record()?.is_none(),
        "a log-only table has no references"
    );
    let mut logs = table.logs();
    assert_eq!(
        logs.next_record()?,
        Some(entry),
        "binary reflog data survives compression"
    );
    assert_eq!(
        logs.next_record()?,
        Some(deleted),
        "tombstones retain their update index"
    );
    assert!(logs.next_record()?.is_none(), "no extra log records are written");
    Ok(())
}

#[test]
fn mixed_tables_keep_references_and_logs_separate() -> crate::Result {
    let (old, new) = object_ids()?;
    let mut header = super::write::header();
    header.block_size = 128;
    let mut writer = Writer::new(Vec::new(), header)?;
    writer.push_reference(ReferenceRef {
        name: BStr::new(b"refs/heads/topic"),
        update_index: 18,
        value: ValueRef::Object {
            object_id: new.as_ref(),
            peeled_id: None,
        },
    })?;
    let entry = log(
        BStr::new(b"refs/heads/topic"),
        18,
        Some(update(old.as_ref(), new.as_ref(), BStr::new(b"native update"))),
    );
    let oversized = vec![b'x'; 512];
    assert!(
        writer
            .push_log(log(
                BStr::new(b"refs/heads/topic"),
                18,
                Some(update(old.as_ref(), new.as_ref(), oversized.as_bytes().as_bstr())),
            ))
            .is_err(),
        "an oversized first log cannot publish a partial reference section"
    );
    writer.push_log(entry)?;
    let bytes = writer.finish()?;
    assert_eq!(bytes[128], b'g', "the log section follows the aligned ref block");
    let table = Table::from_bytes(&bytes)?;
    assert_eq!(
        table.references().next_record()?.expect("reference").name,
        "refs/heads/topic"
    );
    assert_eq!(table.logs().next_record()?, Some(entry), "log data remains readable");
    Ok(())
}

#[test]
fn invalid_logs_do_not_change_the_table_or_reorder_updates() -> crate::Result {
    let mut header = super::write::header();
    header.max_update_index = 19;
    let mut writer = Writer::new(Vec::new(), header)?;
    let entry = log(BStr::new(b"refs/heads/topic"), 18, None);
    writer.push_log(entry)?;
    for (invalid, reason) in [
        (entry, "duplicates"),
        (
            log(BStr::new(b"refs/heads/topic"), 19, None),
            "newer entries after older ones",
        ),
        (log(BStr::new(b"refs/heads/z"), 20, None), "out-of-range indices"),
        (log(BStr::new(b""), 18, None), "empty names"),
    ] {
        assert!(writer.push_log(invalid).is_err(), "{reason} must not be written");
    }
    let (_, new) = object_ids()?;
    let wrong_len = if header.object_hash == gix_hash::Kind::Sha1 {
        32
    } else {
        20
    };
    let wrong = gix_hash::ObjectId::from_hex(&b"33".repeat(wrong_len))?;
    assert!(
        writer
            .push_log(log(
                BStr::new(b"refs/heads/z"),
                19,
                Some(update(wrong.as_ref(), new.as_ref(), BStr::new(b"invalid"))),
            ))
            .is_err(),
        "reflog object IDs must use the table hash"
    );
    assert!(
        writer
            .push_reference(ReferenceRef {
                name: BStr::new(b"refs/heads/other"),
                update_index: 18,
                value: ValueRef::Deletion,
            })
            .is_err(),
        "reference blocks cannot follow log blocks"
    );
    let older = log(BStr::new(b"refs/heads/topic"), 17, None);
    writer.push_log(older)?;
    let bytes = writer.finish()?;
    let table = Table::from_bytes(&bytes)?;
    let mut logs = table.logs();
    assert_eq!(logs.next_record()?, Some(entry));
    assert_eq!(
        logs.next_record()?,
        Some(older),
        "logs may predate the table's minimum index"
    );
    assert!(logs.next_record()?.is_none());
    Ok(())
}

#[test]
fn a_failed_compressed_log_write_reports_an_error() -> crate::Result {
    let mut output = [0; 32];
    let mut writer = Writer::new(&mut output[..], super::write::header())?;
    writer.push_log(log(BStr::new(b"refs/heads/topic"), 18, None))?;
    let error = writer.finish().expect_err("the output cannot hold a compressed log");
    assert!(
        error
            .to_string()
            .contains("Could not write the compressed reflog block"),
        "an output error must not appear to finish a table"
    );
    Ok(())
}

#[test]
fn multiple_log_blocks_have_a_searchable_index() -> crate::Result {
    let (old, new) = object_ids()?;
    let mut header = super::write::header();
    header.block_size = 128;
    header.max_update_index = 80;
    let mut writer = Writer::new(Vec::new(), header)?;
    for index in (18..=80).rev() {
        let message = format!("native update {index}\n");
        writer.push_log(log(
            BStr::new(b"refs/heads/topic"),
            index,
            Some(update(old.as_ref(), new.as_ref(), message.as_bytes().as_bstr())),
        ))?;
    }
    let bytes = writer.finish()?;
    let root = log_index_root(&bytes)?;
    assert!(root > 0, "multiple compressed log blocks require an index");
    assert_eq!(bytes[root], b'i', "the footer points to a log index block");
    let table = Table::from_bytes(&bytes)?;
    let mut logs = table.logs();
    for index in (18..=80).rev() {
        let record = logs.next_record()?.expect("every log entry survives compression");
        assert_eq!(record.update_index, index, "newer updates precede older ones");
    }
    assert!(logs.next_record()?.is_none(), "the log index is not a log entry");
    for index in [80, 51, 18] {
        logs.seek(BStr::new(b"refs/heads/topic"), index)?;
        assert_eq!(logs.next_record()?.expect("indexed log").update_index, index);
    }
    Ok(())
}

#[test]
fn an_oversized_restarted_log_keeps_the_previous_block_complete() -> crate::Result {
    let (old, new) = object_ids()?;
    let hash_len = gix_testtools::object_hash().len_in_bytes();
    let mut header = super::write::header();
    header.block_size = 128;
    let first = log(BStr::new(b"refs/heads/topic"), 18, None);
    let message = vec![b'x'; if hash_len == 20 { 160 } else { 136 }];
    let value_bytes = first.name.len() + 9 + 2 * hash_len + 6 + 18 + message.len();
    assert!(value_bytes < 256, "the record must reach the restart-size check");
    let mut writer = Writer::new(Vec::new(), header)?;
    writer.push_log(first)?;
    assert!(
        writer
            .push_log(log(
                first.name,
                17,
                Some(update(old.as_ref(), new.as_ref(), message.as_bytes().as_bstr())),
            ))
            .is_err(),
        "a record too large after a restart must not flush the previous block"
    );
    let bytes = writer.finish()?;
    let table = Table::from_bytes(&bytes)?;
    let mut logs = table.logs();
    assert_eq!(logs.next_record()?, Some(first), "the valid log remains readable");
    assert!(
        logs.next_record()?.is_none(),
        "a rejected log must not corrupt the block"
    );
    Ok(())
}

#[test]
fn a_failed_intermediate_log_write_cannot_be_finished() -> crate::Result {
    let (old, new) = object_ids()?;
    let mut header = super::write::header();
    header.block_size = 128;
    header.max_update_index = 80;
    let mut writer = Writer::new(super::write::FailOnce::default(), header)?;
    let mut failed_on_write = false;
    for index in (18..=80).rev() {
        let message = format!("native update {index}\n");
        if let Err(error) = writer.push_log(log(
            BStr::new(b"refs/heads/topic"),
            index,
            Some(update(old.as_ref(), new.as_ref(), message.as_bytes().as_bstr())),
        )) {
            assert!(
                error.to_string().contains("Could not write the reflog block header"),
                "the first full block fails while writing"
            );
            failed_on_write = true;
            break;
        }
    }
    assert!(failed_on_write, "a full log block must reach the failing output");
    assert!(
        writer.finish().is_err(),
        "a failed write cannot produce a complete table"
    );
    Ok(())
}

#[test]
fn git_reads_indexed_native_reflogs() -> crate::Result {
    if *gix_testtools::GIT_VERSION < (2, 45, 0) {
        return Ok(());
    }
    let dir = super::writable_fixture("reftable")?;
    let repo = dir.path().join("repo");
    let reftable = repo.join(".git/reftable");
    let first = super::baseline("files", "first.id")?;
    let second = super::baseline("files", "second.id")?;
    let old = gix_hash::ObjectId::from_hex(first.trim_end())?;
    let new = gix_hash::ObjectId::from_hex(second.trim_end())?;
    let filename = "0x000000000012-0x000000000050-00000001.ref";
    let mut header = super::write::header();
    header.block_size = 128;
    header.max_update_index = 80;
    let mut writer = Writer::new(std::fs::File::create(reftable.join(filename))?, header)?;
    for index in (18..=80).rev() {
        let message = format!("native-{index}\n");
        writer.push_log(log(
            BStr::new(b"refs/heads/topic"),
            index,
            Some(update(old.as_ref(), new.as_ref(), message.as_bytes().as_bstr())),
        ))?;
    }
    writer.finish()?;
    super::write::append_manifest(&reftable, filename)?;
    let visible = gix_testtools::git(&repo, "reflog show --format=%gs refs/heads/topic")?;
    assert!(
        visible.as_bytes().starts_with(b"native-80\nnative-79\n"),
        "Git reads the newest indexed log entries: {visible:?}"
    );
    assert!(
        visible
            .as_bytes()
            .split(|byte| *byte == b'\n')
            .any(|line| line == b"native-18".as_slice()),
        "Git traverses all compressed log blocks"
    );
    Ok(())
}

#[test]
fn git_reads_a_native_reflog_appended_to_a_disposable_stack() -> crate::Result {
    if *gix_testtools::GIT_VERSION < (2, 45, 0) {
        return Ok(());
    }
    let first = super::baseline("files", "first.id")?;
    let second = super::baseline("files", "second.id")?;
    let old = gix_hash::ObjectId::from_hex(first.trim_end())?;
    let new = gix_hash::ObjectId::from_hex(second.trim_end())?;
    let filename = "0x000000000012-0x000000000012-00000001.ref";
    let first_line = format!(
        "{}\tnative\tnative@example.com\tnative update\n",
        second.trim_end().as_bstr()
    );
    for with_references in [false, true] {
        let dir = super::writable_fixture("reftable")?;
        let repo = dir.path().join("repo");
        let reftable = repo.join(".git/reftable");
        let mut header = super::write::header();
        if with_references {
            header.block_size = 128;
        }
        let mut writer = Writer::new(std::fs::File::create(reftable.join(filename))?, header)?;
        if with_references {
            for index in 0..24 {
                let name = format!("refs/heads/native-{index:03}");
                writer.push_reference(ReferenceRef {
                    name: name.as_bytes().as_bstr(),
                    update_index: 18,
                    value: ValueRef::Object {
                        object_id: new.as_ref(),
                        peeled_id: None,
                    },
                })?;
            }
        }
        writer.push_log(log(
            BStr::new(b"refs/heads/topic"),
            18,
            Some(update(old.as_ref(), new.as_ref(), BStr::new(b"native update\n"))),
        ))?;
        writer.finish()?;
        super::write::append_manifest(&reftable, filename)?;
        let visible = gix_testtools::git(&repo, "reflog show --format=%H%x09%gn%x09%ge%x09%gs refs/heads/topic")?;
        assert!(
            visible.as_bytes().starts_with(first_line.as_bytes()),
            "Git reads the native reflog with references={with_references}: {visible:?}"
        );
        if with_references {
            assert_eq!(
                gix_testtools::git(&repo, "rev-parse --verify refs/heads/native-023")?
                    .as_bytes()
                    .as_bstr(),
                second.as_bstr(),
                "Git finds the indexed native reference"
            );
        }
    }
    Ok(())
}
