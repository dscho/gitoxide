use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    io,
    path::{Path, PathBuf},
};

use bstr::ByteSlice;
use gix_error::{Result, ResultExt, message};

use super::Stack;

/// The result of removing table files absent from the locked manifest.
#[derive(Debug)]
#[must_use = "Inspect deferred deletions; they may require a later retry"]
pub struct Outcome {
    /// The number of obsolete table files deleted.
    pub removed: usize,
    /// Files that could not be deleted, including those held by other readers or writers.
    pub deferred: Vec<Failure>,
}

/// A table that remains unlisted but could not be removed yet.
#[derive(Debug)]
pub struct Failure {
    /// The obsolete table path.
    pub path: PathBuf,
    /// The filesystem or table-lock error that prevented deletion.
    pub error: io::Error,
}

impl Stack {
    /// Remove unlisted `.ref` files only while holding the current manifest lock.
    ///
    /// Call this after compaction or later to retry deferred deletions. Listed
    /// tables are never removed. Individual failures are returned in
    /// [`Outcome::deferred`] without invalidating a successful publication.
    pub fn cleanup_stale_tables(&self, mode: gix_lock::acquire::Fail) -> Result<Outcome> {
        self.cleanup_with(mode, |path| std::fs::remove_file(path))
    }

    fn cleanup_with(
        &self,
        mode: gix_lock::acquire::Fail,
        mut remove: impl FnMut(&Path) -> io::Result<()>,
    ) -> Result<Outcome> {
        let locked = self.lock(mode)?;
        let listed = locked
            .snapshot()
            .table_names()
            .map(|name| {
                gix_path::from_byte_slice(name.as_bytes())
                    .map(|path| path.as_os_str().to_owned())
                    .or_raise(|| message!("Invalid reftable filename {name:?}"))
            })
            .collect::<Result<BTreeSet<OsString>>>()?;
        let entries = std::fs::read_dir(&self.directory)
            .or_raise(|| message!("Could not list stack directory {}", self.directory.display()))?
            .collect::<io::Result<Vec<_>>>()
            .or_raise(|| message!("Could not read stack directory {}", self.directory.display()))?;
        let mut outcome = Outcome {
            removed: 0,
            deferred: Vec::new(),
        };
        for entry in entries {
            let name = entry.file_name();
            if Path::new(&name).extension() != Some(OsStr::new("ref")) || listed.contains(&name) {
                continue;
            }
            let path = entry.path();
            let guard = match gix_lock::Marker::acquire_to_hold_resource(
                &path,
                gix_lock::acquire::Fail::Immediately,
                None,
                self.options.shared_repository_permissions,
            ) {
                Ok(guard) => guard,
                Err(err) => {
                    outcome.deferred.push(Failure {
                        path,
                        error: io::Error::other(err),
                    });
                    continue;
                }
            };
            let result = remove(&path);
            drop(guard);
            match result {
                Ok(()) => outcome.removed += 1,
                Err(error) => {
                    outcome.deferred.push(Failure { path, error });
                }
            }
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[test]
    fn deferred_deletions_report_errors_and_can_be_retried() -> gix_testtools::TestResult {
        let dir = gix_testtools::tempfile::TempDir::new()?;
        let stale = dir.path().join("stale.ref");
        let temp = dir.path().join("pending.temp");
        std::fs::write(&stale, b"unlisted")?;
        std::fs::write(&temp, b"not a table")?;
        let stack = Stack::at(dir.path(), gix_testtools::object_hash());
        let result = stack.cleanup_with(gix_lock::acquire::Fail::Immediately, |_| {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "in use"))
        })?;
        assert_eq!(result.removed, 0);
        assert_eq!(result.deferred.len(), 1, "a busy file is explicitly reported");
        assert_eq!(result.deferred[0].path, stale);
        assert_eq!(result.deferred[0].error.kind(), io::ErrorKind::PermissionDenied);
        assert!(stale.is_file(), "a failed deletion leaves the file for retry");
        assert!(
            !dir.path().join("stale.ref.lock").exists(),
            "the table lock is released"
        );

        let held = gix_lock::Marker::acquire_to_hold_resource(&stale, gix_lock::acquire::Fail::Immediately, None, 0)?;
        let result = stack.cleanup_stale_tables(gix_lock::acquire::Fail::Immediately)?;
        assert_eq!(
            result.deferred.len(),
            1,
            "another compactor's table lock defers deletion"
        );
        assert!(stale.exists(), "cleanup never bypasses another table lock");
        drop(held);

        let result = stack.cleanup_stale_tables(gix_lock::acquire::Fail::Immediately)?;
        assert_eq!(result.removed, 1, "retry removes the stale table");
        assert!(result.deferred.is_empty());
        assert!(temp.is_file(), "unrelated temporary files are not candidates");
        Ok(())
    }
}
