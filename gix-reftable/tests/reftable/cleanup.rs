use gix_reftable::stack::Stack;
use gix_testtools::bstr::{BStr, ByteSlice};

#[test]
fn cleanup_only_removes_unlisted_tables_and_keeps_old_snapshots() -> crate::Result {
    let dir = gix_testtools::tempfile::TempDir::new()?;
    let stack = Stack::at(dir.path(), gix_testtools::object_hash());
    super::compact::append(&stack, BStr::new(b"refs/a"), Some(1), 1)?;
    super::compact::append(&stack, BStr::new(b"refs/b"), Some(2), 2)?;
    let old = stack.snapshot()?;
    let old_names = old.table_names().map(ToOwned::to_owned).collect::<Vec<_>>();
    let current = super::compact::compact(&stack, 0..2)?;
    assert_eq!(current.table_names().len(), 1, "compaction has replaced both tables");
    for name in &old_names {
        assert!(
            dir.path().join(gix_path::from_byte_slice(name.as_bytes())?).is_file(),
            "retired files survive manifest publication"
        );
    }

    let cleaned = stack.cleanup_stale_tables(gix_lock::acquire::Fail::Immediately)?;
    assert_eq!(cleaned.removed, 2, "both retired tables are removed");
    assert!(cleaned.deferred.is_empty(), "no deletion is blocked");
    for name in old_names {
        assert!(
            !dir.path().join(gix_path::from_byte_slice(name.as_bytes())?).exists(),
            "only unlisted files are deleted"
        );
    }
    assert!(
        old.try_find_reference(BStr::new(b"refs/a"))?.is_some(),
        "immutable readers still own their table bytes"
    );
    assert!(
        stack.snapshot()?.try_find_reference(BStr::new(b"refs/b"))?.is_some(),
        "the listed replacement remains accessible"
    );
    assert!(
        !dir.path().join("tables.list.lock").exists(),
        "cleanup releases its lock"
    );
    Ok(())
}

#[test]
fn cleanup_does_not_race_an_active_manifest_writer() -> crate::Result {
    let dir = gix_testtools::tempfile::TempDir::new()?;
    let stack = Stack::at(dir.path(), gix_testtools::object_hash());
    let locked = stack.lock(gix_lock::acquire::Fail::Immediately)?;
    assert!(
        stack
            .cleanup_stale_tables(gix_lock::acquire::Fail::Immediately)
            .is_err(),
        "cleanup cannot scan while an append holds tables.list.lock"
    );
    drop(locked);
    let result = stack.cleanup_stale_tables(gix_lock::acquire::Fail::Immediately)?;
    assert_eq!(result.removed, 0);
    assert!(result.deferred.is_empty());
    Ok(())
}
