use gix_reftable::{
    LogRef, ReferenceRef, Table,
    log::DataRef,
    reference::ValueRef,
    stack::{CompactionOptions, Snapshot, Stack},
};
use gix_testtools::bstr::{BStr, ByteSlice};

mod snapshot {
    use std::ops::Range;

    use bstr::{BStr, BString, ByteSlice};
    use gix_testtools::TestResult;

    use gix_reftable::{
        Header, LogRef, ReferenceRef, Table,
        log::DataRef,
        reference::ValueRef,
        stack::{CompactionOptions, Snapshot, log, reference},
        write::Writer,
    };

    fn table(
        index: u64,
        references: &[(&[u8], Option<u8>)],
        logs: &[(&[u8], u64, bool)],
    ) -> TestResult<(BString, Vec<u8>)> {
        let hash_len = gix_testtools::object_hash().len_in_bytes();
        let old_id = gix_testtools::object_hash().null();
        let new_id = gix_hash::ObjectId::try_from(vec![index as u8; hash_len].as_slice())?;
        let mut writer = Writer::new(
            Vec::new(),
            Header {
                block_size: 0,
                min_update_index: index,
                max_update_index: index,
                ..crate::write::header()
            },
        )?;
        for (name, value) in references {
            let bytes = value.map(|byte| vec![byte; hash_len]);
            let object_id = bytes.as_deref().map(gix_hash::oid::try_from_bytes).transpose()?;
            writer.push_reference(ReferenceRef {
                name: BStr::new(name),
                update_index: index,
                value: object_id.map_or(ValueRef::Deletion, |object_id| ValueRef::Object {
                    object_id,
                    peeled_id: None,
                }),
            })?;
        }
        for (name, update_index, updated) in logs {
            writer.push_log(LogRef {
                name: BStr::new(name),
                update_index: *update_index,
                data: updated.then_some(DataRef {
                    previous_object_id: old_id.as_ref(),
                    new_object_id: new_id.as_ref(),
                    committer: gix_actor::IdentityRef {
                        name: BStr::new(b"author"),
                        email: BStr::new(b"author@example.com"),
                    },
                    time_seconds: index,
                    timezone_minutes: 0,
                    message: BStr::new(b"update\n"),
                }),
            })?;
        }
        Ok((format!("table-{index}.ref").into(), writer.finish()?))
    }

    fn compact(snapshot: &Snapshot, range: Range<usize>) -> TestResult<(Vec<u8>, usize)> {
        Ok(snapshot.compact_into(range, Vec::new(), CompactionOptions { block_size: 0 })?)
    }

    fn references(snapshot: &Snapshot) -> TestResult<Vec<reference::Reference>> {
        let mut iter = snapshot.references()?;
        let mut out = Vec::new();
        while let Some(record) = iter.next_record()? {
            out.push(record);
        }
        Ok(out)
    }

    fn logs(snapshot: &Snapshot, name: &BStr) -> TestResult<Vec<log::Record>> {
        let mut iter = snapshot.logs(name)?;
        let mut out = Vec::new();
        while let Some(record) = iter.next_record()? {
            out.push(record);
        }
        Ok(out)
    }

    #[test]
    fn full_compaction_drops_masked_values_and_tombstones() -> TestResult {
        let old = table(
            1,
            &[(b"refs/a", Some(1)), (b"refs/b", Some(2))],
            &[(b"refs/a", 1, true)],
        )?;
        let new = table(
            2,
            &[(b"refs/a", None), (b"refs/c", Some(3))],
            &[(b"refs/a", 2, true), (b"refs/a", 1, false)],
        )?;
        let before = Snapshot::from_tables(gix_testtools::object_hash(), [old, new])?;
        let (data, count) = compact(&before, 0..2)?;
        assert_eq!(count, 3, "two references and the unmasked log remain");
        let table = Table::from_bytes(&data)?;
        let mut refs = table.references();
        assert_eq!(refs.next_record()?.expect("first reference").name, "refs/b");
        assert_eq!(refs.next_record()?.expect("second reference").name, "refs/c");
        assert!(refs.next_record()?.is_none(), "the deleted reference is absent");
        let mut written_logs = table.logs();
        assert_eq!(written_logs.next_record()?.expect("newer log").update_index, 2);
        assert!(
            written_logs.next_record()?.is_none(),
            "the masked old log does not return"
        );
        let after = Snapshot::from_tables(gix_testtools::object_hash(), [("merged".into(), data)])?;
        assert_eq!(
            references(&before)?,
            references(&after)?,
            "full compaction preserves visible refs"
        );
        assert_eq!(
            logs(&before, BStr::new(b"refs/a"))?,
            logs(&after, BStr::new(b"refs/a"))?,
            "full compaction preserves visible logs"
        );
        Ok(())
    }

    #[test]
    fn partial_compaction_retains_tombstones_against_lower_tables() -> TestResult {
        let old = table(1, &[(b"refs/a", Some(1))], &[(b"refs/a", 1, true)])?;
        let deleting = table(2, &[(b"refs/a", None)], &[(b"refs/a", 1, false)])?;
        let newest = table(3, &[(b"refs/c", Some(3))], &[(b"refs/c", 3, true)])?;
        let before = Snapshot::from_tables(gix_testtools::object_hash(), [old.clone(), deleting, newest])?;
        let (data, count) = compact(&before, 1..3)?;
        assert_eq!(count, 4, "both tombstones and new records remain");
        let table = Table::from_bytes(&data)?;
        assert_eq!(
            table.references().next_record()?.expect("reference tombstone").value,
            ValueRef::Deletion,
            "the deleted reference must still mask the lower table"
        );
        assert!(
            table.logs().next_record()?.expect("log tombstone").data.is_none(),
            "the deleted log must still mask the lower table"
        );
        let after = Snapshot::from_tables(gix_testtools::object_hash(), [old, ("merged".into(), data)])?;
        assert_eq!(
            references(&before)?,
            references(&after)?,
            "partial compaction preserves visible refs"
        );
        for name in [b"refs/a".as_slice(), b"refs/c".as_slice()] {
            assert_eq!(
                logs(&before, name.as_bstr())?,
                logs(&after, name.as_bstr())?,
                "partial compaction preserves each reflog"
            );
        }
        Ok(())
    }

    #[test]
    fn full_compaction_can_yield_an_empty_table() -> TestResult {
        let old = table(1, &[(b"refs/a", Some(1))], &[(b"refs/a", 1, true)])?;
        let deleting = table(2, &[(b"refs/a", None)], &[(b"refs/a", 1, false)])?;
        let before = Snapshot::from_tables(gix_testtools::object_hash(), [old, deleting])?;
        let (data, count) = compact(&before, 0..2)?;
        assert_eq!(count, 0, "nothing is visible after compaction");
        let table = Table::from_bytes(&data)?;
        assert!(table.references().next_record()?.is_none(), "no reference remains");
        assert!(table.logs().next_record()?.is_none(), "no reflog remains");
        Ok(())
    }

    #[test]
    fn invalid_ranges_are_rejected() -> TestResult {
        let only = table(1, &[(b"refs/a", Some(1))], &[])?;
        let snapshot = Snapshot::from_tables(gix_testtools::object_hash(), [only])?;
        for range in [0..0, 1..1, 0..2, 2..3] {
            assert!(
                compact(&snapshot, range.clone()).is_err(),
                "invalid range {range:?} must be rejected"
            );
        }
        Ok(())
    }
}

fn append(stack: &Stack, name: &BStr, value: Option<u8>, log_index: u64) -> crate::Result {
    let locked = stack.lock(gix_lock::acquire::Fail::Immediately)?;
    let index = locked.next_update_index()?;
    let object_id = value
        .map(|byte| gix_hash::ObjectId::try_from(vec![byte; gix_testtools::object_hash().len_in_bytes()].as_slice()))
        .transpose()?;
    let null_id = gix_testtools::object_hash().null();
    locked.append(super::append::next_header(index), |writer| {
        writer.push_reference(ReferenceRef {
            name,
            update_index: index,
            value: object_id.as_ref().map_or(ValueRef::Deletion, |id| ValueRef::Object {
                object_id: id.as_ref(),
                peeled_id: None,
            }),
        })?;
        writer.push_log(LogRef {
            name,
            update_index: log_index,
            data: object_id.as_ref().map(|id| DataRef {
                previous_object_id: null_id.as_ref(),
                new_object_id: id.as_ref(),
                committer: gix_actor::IdentityRef {
                    name: BStr::new(b"author"),
                    email: BStr::new(b"author@example.com"),
                },
                time_seconds: index,
                timezone_minutes: 0,
                message: BStr::new(b"update\n"),
            }),
        })
    })?;
    Ok(())
}

fn compact(stack: &Stack, range: std::ops::Range<usize>) -> crate::Result<Snapshot> {
    let locked = stack.lock(gix_lock::acquire::Fail::Immediately)?;
    let selection = locked.snapshot().select_tables(range)?;
    Ok(locked.compact(selection, CompactionOptions { block_size: 0 })?)
}

#[test]
fn full_compaction_removes_masked_records_and_can_publish_an_empty_manifest() -> crate::Result {
    let dir = gix_testtools::tempfile::TempDir::new()?;
    let stack = Stack::at(dir.path(), gix_testtools::object_hash());
    append(&stack, BStr::new(b"refs/a"), Some(1), 1)?;
    let original = stack.snapshot()?;
    append(&stack, BStr::new(b"refs/a"), None, 1)?;
    let before = stack.snapshot()?;
    let after = compact(&stack, 0..2)?;
    assert!(after.table_names().next().is_none(), "both tables cancel completely");
    assert!(
        std::fs::read(dir.path().join("tables.list"))?.is_empty(),
        "the empty manifest is published"
    );
    assert!(
        original.try_find_reference(BStr::new(b"refs/a"))?.is_some(),
        "old in-memory snapshots survive replacement"
    );
    assert!(before.try_find_reference(BStr::new(b"refs/a"))?.is_none());
    assert!(stack.snapshot()?.try_find_reference(BStr::new(b"refs/a"))?.is_none());
    assert!(
        stack.snapshot()?.logs(BStr::new(b"refs/a"))?.next_record()?.is_none(),
        "the old log must not reappear"
    );
    Ok(())
}

#[test]
fn partial_compaction_keeps_tombstones_below_an_unmodified_table() -> crate::Result {
    let dir = gix_testtools::tempfile::TempDir::new()?;
    let stack = Stack::at(dir.path(), gix_testtools::object_hash());
    append(&stack, BStr::new(b"refs/a"), Some(1), 1)?;
    let lower_name = stack.snapshot()?.table_names().next().expect("base table").to_owned();
    append(&stack, BStr::new(b"refs/a"), None, 1)?;
    append(&stack, BStr::new(b"refs/c"), Some(3), 3)?;
    let before = stack.snapshot()?;
    let after = compact(&stack, 1..3)?;
    assert_eq!(after.table_names().len(), 2, "two upper tables are replaced");
    assert_eq!(after.table_names().next().expect("base table"), lower_name);
    let compacted = after.table_names().last().expect("replacement table");
    let encoded = std::fs::read(dir.path().join(gix_path::from_byte_slice(compacted.as_bytes())?))?;
    let table = Table::from_bytes(&encoded)?;
    assert_eq!(
        table.references().next_record()?.expect("stored deletion").value,
        ValueRef::Deletion,
        "the compacted table still masks refs/a"
    );
    assert!(
        table.logs().next_record()?.expect("stored log deletion").data.is_none(),
        "the compacted table still masks its old log"
    );
    assert!(after.try_find_reference(BStr::new(b"refs/a"))?.is_none());
    assert!(
        after.try_find_reference(BStr::new(b"refs/c"))?.is_some(),
        "unrelated refs survive"
    );
    assert!(
        after.logs(BStr::new(b"refs/a"))?.next_record()?.is_none(),
        "the old log cannot reappear"
    );
    assert!(
        before.try_find_reference(BStr::new(b"refs/c"))?.is_some(),
        "existing readers keep their old snapshot"
    );
    Ok(())
}

#[test]
fn git_reads_and_extends_a_partially_compacted_stack() -> crate::Result {
    if *gix_testtools::GIT_VERSION < (2, 45, 0) {
        return Ok(());
    }
    let dir = super::writable_fixture("reftable")?;
    let repo = dir.path().join("repo");
    let stack = Stack::at(repo.join(".git/reftable"), gix_testtools::object_hash());
    let base_count = stack.snapshot()?.table_names().len();
    let first = super::baseline("files", "first.id")?;
    let object_id = gix_hash::ObjectId::from_hex(first.trim_end())?;
    let locked = stack.lock(gix_lock::acquire::Fail::Immediately)?;
    let index = locked.next_update_index()?;
    locked.append(super::append::next_header(index), |writer| {
        writer.push_reference(ReferenceRef {
            name: BStr::new(b"refs/heads/topic"),
            update_index: index,
            value: ValueRef::Deletion,
        })
    })?;
    let locked = stack.lock(gix_lock::acquire::Fail::Immediately)?;
    let index = locked.next_update_index()?;
    locked.append(super::append::next_header(index), |writer| {
        writer.push_reference(ReferenceRef {
            name: BStr::new(b"refs/heads/written"),
            update_index: index,
            value: ValueRef::Object {
                object_id: object_id.as_ref(),
                peeled_id: None,
            },
        })
    })?;

    let after = compact(&stack, base_count..base_count + 2)?;
    assert_eq!(
        after.table_names().len(),
        base_count + 1,
        "the lower tables are unchanged"
    );
    assert!(
        gix_testtools::git(&repo, "rev-parse --verify refs/heads/topic").is_err(),
        "Git still sees the tombstone masking the lower table"
    );
    assert_eq!(
        gix_testtools::git(&repo, "rev-parse --verify refs/heads/written")?
            .as_bytes()
            .as_bstr(),
        first.as_bstr(),
        "Git reads the replacement table"
    );
    if *gix_testtools::GIT_VERSION >= (2, 52, 0) {
        gix_testtools::git(&repo, "refs verify --strict")?;
    }
    gix_testtools::git(
        &repo,
        &format!("update-ref refs/heads/post-compact {}", first.trim_end().as_bstr()),
    )?;
    assert!(
        stack
            .snapshot()?
            .try_find_reference(BStr::new(b"refs/heads/post-compact"))?
            .is_some(),
        "Git can append to the compacted stack"
    );
    Ok(())
}

#[test]
fn a_range_from_an_older_snapshot_cannot_compact_different_tables() -> crate::Result {
    let dir = gix_testtools::tempfile::TempDir::new()?;
    let stack = Stack::at(dir.path(), gix_testtools::object_hash());
    for index in 1..=5 {
        let name = format!("refs/{index}");
        append(&stack, name.as_bytes().as_bstr(), Some(index as u8), index)?;
    }
    let original = stack.snapshot()?;
    let locked = stack.lock(gix_lock::acquire::Fail::Immediately)?;
    locked.compact(original.select_tables(1..3)?, CompactionOptions::default())?;
    let current = stack.snapshot()?;
    assert_eq!(
        current.table_names().len(),
        4,
        "a pair overlapping the selection was replaced"
    );
    let expected = current.table_names().map(ToOwned::to_owned).collect::<Vec<_>>();

    let locked = stack.lock(gix_lock::acquire::Fail::Immediately)?;
    let error = locked
        .compact(original.select_tables(2..4)?, CompactionOptions::default())
        .err()
        .expect("the stale selection must be rejected");
    assert!(
        error.is_conflict(),
        "2..4 from the old snapshot must not silently replace different tables"
    );
    assert_eq!(
        stack.snapshot()?.table_names().collect::<Vec<_>>(),
        expected,
        "failed validation leaves the manifest untouched"
    );
    assert_eq!(
        original.table_names().len(),
        5,
        "the earlier snapshot retains its original view"
    );
    Ok(())
}

#[test]
fn a_name_bound_range_can_move_when_earlier_tables_are_compacted() -> crate::Result {
    let dir = gix_testtools::tempfile::TempDir::new()?;
    let stack = Stack::at(dir.path(), gix_testtools::object_hash());
    for index in 1..=5 {
        let name = format!("refs/{index}");
        append(&stack, name.as_bytes().as_bstr(), Some(index as u8), index)?;
    }
    let original = stack.snapshot()?;
    let selection = original.select_tables(3..5)?;
    let unchanged = original.table_names().nth(2).expect("middle table").to_owned();
    let locked = stack.lock(gix_lock::acquire::Fail::Immediately)?;
    locked.compact(original.select_tables(0..2)?, CompactionOptions::default())?;

    let locked = stack.lock(gix_lock::acquire::Fail::Immediately)?;
    let after = locked.compact(selection, CompactionOptions::default())?;
    assert_eq!(after.table_names().len(), 3, "both selected tables are merged");
    assert_eq!(
        after.table_names().nth(1).expect("unselected middle table"),
        unchanged,
        "earlier compaction does not redirect the selected range"
    );
    Ok(())
}
