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
