use gix_error::Result;
use gix_reftable::stack::{
    Snapshot,
    reference::{Iter, Value},
};
use gix_testtools::bstr::{BString, ByteSlice};

fn ref_record(name: &[u8], value: Option<u8>) -> Vec<u8> {
    let mut record = vec![0];
    super::log::varint((name.len() as u64) << 3 | u64::from(value.is_some()), &mut record);
    record.extend_from_slice(name);
    record.push(0);
    if let Some(byte) = value {
        record.extend(std::iter::repeat_n(byte, gix_testtools::object_hash().len_in_bytes()));
    }
    record
}

fn refs_table(records: &[(&[u8], Option<u8>)]) -> Vec<u8> {
    let mut encoded = Vec::new();
    let mut restarts = Vec::new();
    for (name, value) in records {
        restarts.push(encoded.len());
        encoded.extend(ref_record(name, *value));
    }
    super::reference::table_with_records(&encoded, &restarts)
}

fn snapshot(tables: impl IntoIterator<Item = (BString, Vec<u8>)>) -> Result<Snapshot> {
    Snapshot::from_tables(gix_testtools::object_hash(), tables)
}

fn reference_names(mut references: Iter<'_>) -> Result<Vec<BString>> {
    let mut names = Vec::new();
    while let Some(reference) = references.next_record()? {
        names.push(reference.name);
    }
    Ok(names)
}

fn logs_table(records: &[(&[u8], u64, bool)]) -> crate::Result<Vec<u8>> {
    let mut encoded = Vec::new();
    let mut restarts = Vec::new();
    for (name, index, update) in records {
        restarts.push(encoded.len());
        encoded.extend(super::log::record(name, *index, *update));
    }
    super::log::log_table(&encoded, &restarts)
}

#[test]
fn references_prefer_newer_tables_and_hide_tombstones() -> crate::Result {
    let old = refs_table(&[(b"refs/a", Some(1)), (b"refs/b", Some(1))]);
    let new = refs_table(&[(b"refs/a", None), (b"refs/c", Some(2))]);
    let snapshot = snapshot([("old".into(), old), ("new".into(), new)])?;
    assert_eq!(snapshot.table_names().collect::<Vec<_>>(), ["old", "new"]);
    let names = reference_names(snapshot.references()?)?;
    assert_eq!(
        names,
        ["refs/b", "refs/c"],
        "newer tombstones suppress older values while unrelated refs remain ordered"
    );
    assert!(
        snapshot.try_find_reference("refs/a".into())?.is_none(),
        "lookup hides tombstones"
    );
    assert_eq!(
        snapshot.try_find_reference("refs/c".into())?.expect("new value").value,
        Value::Object {
            object_id: gix_hash::ObjectId::from_bytes_or_panic(&vec![2; gix_testtools::object_hash().len_in_bytes()]),
            peeled_id: None,
        }
    );
    Ok(())
}

#[test]
fn prefixed_reference_iteration_stops_at_the_prefix_boundary() -> crate::Result {
    let table = refs_table(&[
        (b"refs/heads/a", Some(1)),
        (b"refs/heads/b", Some(2)),
        (b"refs/tags/a", Some(3)),
    ]);
    let snapshot = snapshot([("one".into(), table)])?;
    let references = snapshot.references_prefixed("refs/heads/".into())?;
    let names = reference_names(references)?;
    assert_eq!(names, ["refs/heads/a", "refs/heads/b"]);
    Ok(())
}

#[test]
fn logs_merge_by_key_and_hide_newer_tombstones() -> crate::Result {
    let old = logs_table(&[(b"refs/a", 9, true), (b"refs/a", 8, true)])?;
    let new = logs_table(&[(b"refs/a", 10, true), (b"refs/a", 9, false)])?;
    let snapshot = snapshot([("old".into(), old), ("new".into(), new)])?;
    let mut logs = snapshot.logs("refs/a".into())?;
    let mut indices = Vec::new();
    while let Some(record) = logs.next_record()? {
        indices.push(record.update_index);
    }
    assert_eq!(
        indices,
        [10, 8],
        "newer tombstone suppresses the duplicate update index"
    );
    Ok(())
}

#[test]
fn snapshots_reject_hash_mismatches_and_corrupt_tables() {
    let other = if gix_testtools::object_hash().len_in_bytes() == 20 {
        super::envelope::empty_table(2, b"s256")
    } else {
        super::envelope::empty_table(1, b"sha1")
    };
    assert!(
        Snapshot::from_tables(gix_testtools::object_hash(), [("other".into(), other)]).is_err(),
        "all stack tables use one object-hash format"
    );
    assert!(
        Snapshot::from_tables(
            gix_testtools::object_hash(),
            [("broken".into(), b"not a table".to_vec())]
        )
        .is_err(),
        "corrupt tables fail snapshot creation"
    );
}

#[test]
fn merged_cursor_errors_are_terminal_without_exposing_stale_records() -> crate::Result {
    let old = refs_table(&[(b"refs/a", Some(1)), (b"refs/b", Some(1))]);
    let first = ref_record(b"refs/a", Some(2));
    let mut second = ref_record(b"refs/b", Some(2));
    second.pop();
    let mut records = first.clone();
    records.extend(second);
    let newer = super::reference::table_with_records(&records, &[0, first.len()]);
    let snapshot = snapshot([("old".into(), old), ("newer".into(), newer)])?;
    assert!(
        snapshot.try_find_reference("refs/a".into())?.is_some(),
        "exact lookup does not decode the selected record's malformed successor"
    );
    let mut references = snapshot.references()?;
    assert_eq!(
        references
            .next_record()?
            .expect("the selected record is returned before reading its successor")
            .name,
        "refs/a"
    );
    assert!(
        references.next_record().is_err(),
        "the malformed successor in the newer table is reported"
    );
    assert!(
        references.next_record()?.is_none(),
        "a terminal error cannot expose an older table's stale record"
    );
    Ok(())
}

#[test]
fn merged_log_errors_are_terminal_without_exposing_stale_records() -> crate::Result {
    let old = logs_table(&[(b"refs/a", 9, true), (b"refs/a", 8, true)])?;
    let first = super::log::record(b"refs/a", 9, true);
    let mut second = super::log::record(b"refs/a", 8, true);
    second.pop();
    let mut encoded = first.clone();
    encoded.extend(second);
    let newer = super::log::log_table(&encoded, &[0, first.len()])?;
    let snapshot = snapshot([("old".into(), old), ("newer".into(), newer)])?;
    let mut logs = snapshot.logs("refs/a".into())?;
    assert_eq!(logs.next_record()?.expect("valid newest record").update_index, 9);
    assert!(logs.next_record().is_err(), "the malformed newer successor is reported");
    assert!(
        logs.next_record()?.is_none(),
        "a terminal error cannot expose the older duplicate"
    );
    Ok(())
}

#[test]
fn directory_stacks_load_git_tables_in_manifest_order() -> crate::Result {
    let reftable = super::fixture()?.join("reftable/repo/.git/reftable");
    let expected_names = std::fs::read_to_string(reftable.join("tables.list"))?
        .lines()
        .map(BString::from)
        .collect::<Vec<_>>();
    let stack = gix_reftable::stack::Stack::at(&reftable, gix_testtools::object_hash());
    let snapshot = stack.snapshot()?;
    assert_eq!(stack.directory(), reftable);
    assert_eq!(snapshot.table_names().collect::<Vec<_>>(), expected_names);

    let expected = super::baseline("files", "refs")?;
    let references = snapshot.references_prefixed("refs/".into())?;
    let names = reference_names(references)?;
    assert_eq!(
        names,
        expected.lines().collect::<Vec<_>>(),
        "the merged native view agrees with Git"
    );
    Ok(())
}

#[test]
fn old_snapshot_cursors_survive_later_stack_reloads() -> crate::Result {
    let dir = gix_testtools::tempfile::TempDir::new()?;
    std::fs::write(dir.path().join("old.ref"), refs_table(&[(b"refs/a", Some(1))]))?;
    std::fs::write(dir.path().join("tables.list"), b"old.ref\n")?;
    let stack = gix_reftable::stack::Stack::at(dir.path(), gix_testtools::object_hash());
    let old_snapshot = stack.snapshot()?;
    let mut old_references = old_snapshot.references()?;

    std::fs::write(dir.path().join("new.ref"), refs_table(&[(b"refs/a", Some(2))]))?;
    std::fs::write(dir.path().join("tables.list"), b"old.ref\nnew.ref\n")?;
    let new_snapshot = stack.snapshot()?;

    let old = old_references
        .next_record()?
        .expect("the old snapshot retains its table bytes");
    let new = new_snapshot
        .try_find_reference("refs/a".into())?
        .expect("the reloaded snapshot sees the appended table");
    assert_ne!(old.value, new.value, "new snapshots do not mutate existing cursors");
    Ok(())
}
