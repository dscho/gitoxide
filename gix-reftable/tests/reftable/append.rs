use gix_reftable::{
    Header, LogRef, ReferenceRef,
    log::DataRef,
    reference::ValueRef,
    stack::{Options, Snapshot, Stack},
};
use gix_testtools::bstr::{BStr, ByteSlice};

fn next_header(index: u64) -> Header {
    let mut header = super::write::header();
    header.min_update_index = index;
    header.max_update_index = index;
    header
}

fn stored_names(snapshot: &Snapshot) -> Vec<&BStr> {
    snapshot.table_names().collect()
}

#[test]
fn holding_the_manifest_lock_excludes_other_writers() -> crate::Result {
    let dir = gix_testtools::tempfile::TempDir::new()?;
    let stack = Stack::at_opts(
        dir.path(),
        gix_testtools::object_hash(),
        Options {
            shared_repository_permissions: -0o640,
            ..Default::default()
        },
    );
    let lock_path = dir.path().join("tables.list.lock");
    let locked = stack.lock(gix_lock::acquire::Fail::Immediately)?;
    assert_eq!(locked.next_update_index()?, 1, "the empty stack begins at index one");
    assert!(lock_path.is_file(), "the manifest lock is held");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            lock_path.metadata()?.permissions().mode() & 0o777,
            0o640,
            "locking applies the caller's resolved sharing permissions"
        );
    }
    let contender = Stack::at(dir.path(), gix_testtools::object_hash());
    assert!(
        contender.lock(gix_lock::acquire::Fail::Immediately).is_err(),
        "a competing writer cannot bypass the lock"
    );
    drop(locked);
    assert!(!lock_path.exists(), "dropping an unused lock rolls it back");
    let _again = stack.lock(gix_lock::acquire::Fail::Immediately)?;
    Ok(())
}

#[test]
fn the_locked_snapshot_includes_changes_published_by_git() -> crate::Result {
    if *gix_testtools::GIT_VERSION < (2, 45, 0) {
        return Ok(());
    }
    let dir = super::writable_fixture("reftable")?;
    let repo = dir.path().join("repo");
    let stack = Stack::at(repo.join(".git/reftable"), gix_testtools::object_hash());
    let before = stack.snapshot()?;
    let name = "refs/heads/locked-new";
    assert!(
        before.try_find_reference(name.into())?.is_none(),
        "the old snapshot precedes Git's write"
    );

    let first = super::baseline("files", "first.id")?;
    gix_testtools::git(&repo, &format!("update-ref {name} {}", first.trim_end().as_bstr()))?;
    let locked = stack.lock(gix_lock::acquire::Fail::Immediately)?;
    assert!(
        locked.snapshot().try_find_reference(name.into())?.is_some(),
        "the stack is reloaded only after acquiring tables.list.lock"
    );
    assert!(
        before.try_find_reference(name.into())?.is_none(),
        "earlier snapshots retain their immutable view"
    );
    Ok(())
}

#[test]
fn stale_update_indices_are_rejected_after_a_git_writer() -> crate::Result {
    if *gix_testtools::GIT_VERSION < (2, 45, 0) {
        return Ok(());
    }
    let dir = super::writable_fixture("reftable")?;
    let repo = dir.path().join("repo");
    let stack = Stack::at(repo.join(".git/reftable"), gix_testtools::object_hash());
    let old_lock = stack.lock(gix_lock::acquire::Fail::Immediately)?;
    let stale = next_header(old_lock.next_update_index()?);
    drop(old_lock);

    let first = super::baseline("files", "first.id")?;
    let name = "refs/heads/git-race";
    gix_testtools::git(&repo, &format!("update-ref {name} {}", first.trim_end().as_bstr()))?;
    let locked = stack.lock(gix_lock::acquire::Fail::Immediately)?;
    assert!(
        locked.next_update_index()? > stale.min_update_index,
        "Git advanced the stack"
    );
    let called = std::cell::Cell::new(false);
    let error = locked
        .append(stale, |_| {
            called.set(true);
            Ok(())
        })
        .err()
        .expect("the stale transaction must be rejected");
    assert!(error.is_conflict(), "a stale transaction cannot overlap Git's update");
    assert!(!called.get(), "stale validation stops before writing a table");
    assert!(
        stack.snapshot()?.try_find_reference(name.into())?.is_some(),
        "Git's table remains intact"
    );
    assert!(
        !stack.directory().join("tables.list.lock").exists(),
        "failed validation releases the lock"
    );
    Ok(())
}

#[test]
fn appending_a_complete_table_publishes_it_to_git() -> crate::Result {
    if *gix_testtools::GIT_VERSION < (2, 45, 0) {
        return Ok(());
    }
    let dir = super::writable_fixture("reftable")?;
    let repo = dir.path().join("repo");
    let reftable = repo.join(".git/reftable");
    let stack = Stack::at_opts(
        &reftable,
        gix_testtools::object_hash(),
        Options {
            shared_repository_permissions: -0o640,
            ..Default::default()
        },
    );
    let previous = stack.snapshot()?;
    let first = super::baseline("files", "first.id")?;
    let object_id = gix_hash::ObjectId::from_hex(first.trim_end())?;
    let null_id = gix_testtools::object_hash().null();
    let locked = stack.lock(gix_lock::acquire::Fail::Immediately)?;
    let header = next_header(locked.next_update_index()?);
    let current = locked.append(header, |writer| {
        writer.push_reference(ReferenceRef {
            name: BStr::new(b"refs/heads/native"),
            update_index: header.min_update_index,
            value: ValueRef::Object {
                object_id: object_id.as_ref(),
                peeled_id: None,
            },
        })?;
        writer.push_reference(ReferenceRef {
            name: BStr::new(b"refs/heads/topic"),
            update_index: header.min_update_index,
            value: ValueRef::Deletion,
        })?;
        writer.push_log(LogRef {
            name: BStr::new(b"refs/heads/native"),
            update_index: header.min_update_index,
            data: Some(DataRef {
                previous_object_id: null_id.as_ref(),
                new_object_id: object_id.as_ref(),
                committer: gix_actor::IdentityRef {
                    name: BStr::new(b"native"),
                    email: BStr::new(b"native@example.com"),
                },
                time_seconds: 1_700_000_000,
                timezone_minutes: 0,
                message: BStr::new(b"native append\n"),
            }),
        })
    })?;
    assert_eq!(stored_names(&current).len(), stored_names(&previous).len() + 1);
    assert!(
        previous.try_find_reference("refs/heads/native".into())?.is_none(),
        "readers retain the earlier snapshot"
    );
    assert!(
        current.try_find_reference("refs/heads/native".into())?.is_some(),
        "the new snapshot includes the published table"
    );
    assert!(
        current.try_find_reference("refs/heads/topic".into())?.is_none(),
        "the tombstone hides the older value"
    );
    let names = stored_names(&current);
    let last = names.last().expect("appended table");
    assert!(last.starts_with(b"0x"), "table names use Git's update-index prefix");
    assert!(last.ends_with(b".ref"), "Git recognizes the appended table file");
    let table_path = reftable.join(gix_path::from_byte_slice(last.as_bytes())?);
    assert!(table_path.is_file(), "the published table exists");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let table_mode = std::fs::metadata(&table_path)?.permissions().mode() & 0o777;
        let manifest_mode = std::fs::metadata(reftable.join("tables.list"))?.permissions().mode() & 0o777;
        assert_eq!(table_mode, manifest_mode, "the table is as accessible as the manifest");
    }
    assert_eq!(
        gix_testtools::git(&repo, "rev-parse --verify refs/heads/native")?
            .as_bytes()
            .as_bstr(),
        first.as_bstr(),
        "Git reads the atomically published reference"
    );
    assert!(
        gix_testtools::git(&repo, "rev-parse --verify refs/heads/topic").is_err(),
        "Git honors the published tombstone"
    );
    assert!(
        gix_testtools::git(&repo, "reflog show --format=%gs refs/heads/native")?
            .as_bytes()
            .starts_with(b"native append\n"),
        "Git reads the published native reflog"
    );
    if *gix_testtools::GIT_VERSION >= (2, 52, 0) {
        gix_testtools::git(&repo, "refs verify --strict")?;
    }
    assert!(
        !reftable.join("tables.list.lock").exists(),
        "commit releases the manifest lock"
    );
    Ok(())
}

#[test]
fn successive_appends_keep_all_tables_and_advance_the_index() -> crate::Result {
    let dir = gix_testtools::tempfile::TempDir::new()?;
    let stack = Stack::at(dir.path(), gix_testtools::object_hash());
    for (index, name) in [(1, b"refs/a"), (2, b"refs/b")] {
        let locked = stack.lock(gix_lock::acquire::Fail::Immediately)?;
        let header = next_header(locked.next_update_index()?);
        assert_eq!(
            header.min_update_index, index,
            "each append uses the latest stack index"
        );
        let current = locked.append(header, |writer| {
            writer.push_reference(ReferenceRef {
                name: BStr::new(name),
                update_index: index,
                value: ValueRef::Deletion,
            })
        })?;
        assert_eq!(
            current.table_names().len(),
            index as usize,
            "earlier tables remain listed"
        );
    }
    assert_eq!(stack.snapshot()?.table_names().len(), 2, "a reload sees both tables");
    let locked = stack.lock(gix_lock::acquire::Fail::Immediately)?;
    assert_eq!(
        locked.next_update_index()?,
        3,
        "the next table cannot overlap earlier updates"
    );
    Ok(())
}

#[test]
fn invalid_bounds_and_failed_writes_leave_the_stack_unchanged() -> crate::Result {
    let dir = gix_testtools::tempfile::TempDir::new()?;
    let stack = Stack::at(dir.path(), gix_testtools::object_hash());
    let locked = stack.lock(gix_lock::acquire::Fail::Immediately)?;
    let mut header = next_header(0);
    assert!(
        locked
            .append(header, |writer| {
                writer.push_reference(ReferenceRef {
                    name: BStr::new(b"refs/a"),
                    update_index: 0,
                    value: ValueRef::Deletion,
                })
            })
            .is_err(),
        "the table cannot reuse an earlier update index"
    );
    let locked = stack.lock(gix_lock::acquire::Fail::Immediately)?;
    header = next_header(locked.next_update_index()?);
    assert!(
        locked.append(header, |_| Ok(())).is_err(),
        "empty tables must not be published"
    );
    let locked = stack.lock(gix_lock::acquire::Fail::Immediately)?;
    assert!(
        locked
            .append(header, |writer| {
                let record = ReferenceRef {
                    name: BStr::new(b"refs/a"),
                    update_index: header.min_update_index,
                    value: ValueRef::Deletion,
                };
                writer.push_reference(record)?;
                writer.push_reference(record)
            })
            .is_err(),
        "invalid batches never publish a partial table"
    );
    assert!(
        stack.snapshot()?.table_names().next().is_none(),
        "no table was published"
    );
    assert!(!dir.path().join("tables.list").exists(), "no manifest was published");
    assert!(
        !dir.path().join("tables.list.lock").exists(),
        "the lock was rolled back"
    );
    Ok(())
}

#[test]
fn a_failed_manifest_rename_releases_the_lock_while_reporting_the_error() -> crate::Result {
    let dir = gix_testtools::tempfile::TempDir::new()?;
    let stack = Stack::at(dir.path(), gix_testtools::object_hash());
    let locked = stack.lock(gix_lock::acquire::Fail::Immediately)?;
    let index = locked.next_update_index()?;
    std::fs::create_dir(dir.path().join("tables.list"))?;
    let error = locked
        .append(next_header(index), |writer| {
            writer.push_reference(ReferenceRef {
                name: BStr::new(b"refs/a"),
                update_index: index,
                value: ValueRef::Deletion,
            })
        })
        .err()
        .expect("the manifest cannot replace a directory");
    assert!(
        error.to_string().contains("Could not publish"),
        "the rename failure is reported"
    );
    assert!(
        !dir.path().join("tables.list.lock").exists(),
        "the failed manifest commit releases its lock while the error is retained"
    );
    assert!(
        std::fs::read_dir(dir.path())?
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .all(|entry| entry.path().extension().is_none_or(|ext| ext != "ref")),
        "the unpublished reftable is removed"
    );
    Ok(())
}
