use gix_reftable::stack::{Options, Stack};
use gix_testtools::bstr::ByteSlice;

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
