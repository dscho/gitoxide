use std::{
    io::{self, Read, Seek, Write},
    ops::Range,
};

use bstr::{BString, ByteSlice};
use gix_error::{ErrorExt, OptionExt, Result, ResultExt, bail, conflict, message, validation};

use super::{Locked, Snapshot, Stack, TableData};
use crate::{Header, Table, write::Writer};

impl Stack {
    /// Lock `tables.list` and reload the latest snapshot under that lock.
    pub fn lock(&self, mode: gix_lock::acquire::Fail) -> Result<Locked> {
        let manifest = self.directory.join("tables.list");
        let lock = gix_lock::File::acquire_to_update_resource(
            &manifest,
            mode,
            None,
            self.options.shared_repository_permissions,
        )
        .or_raise(|| message!("Could not lock {}", manifest.display()))?;
        let snapshot = self
            .snapshot()
            .or_raise(|| message!("Could not reload stack at {} while locked", self.directory.display()))?;
        Ok(Locked { _lock: lock, snapshot })
    }
}

impl Locked {
    /// Return the latest stack snapshot while holding its manifest lock.
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// Return the first update index not covered by the locked stack.
    pub fn next_update_index(&self) -> Result<u64> {
        let Some(last) = self.snapshot.tables.last() else {
            return Ok(1);
        };
        let table = Table::from_bytes(&last.data)
            .or_raise(|| message!("Could not reopen the last reftable {:?}", last.name))?;
        table
            .header()
            .max_update_index
            .checked_add(1)
            .ok_or_raise(|| message("The stack has no available update index"))
    }

    /// Write a complete table from `write`, then atomically publish it to the stack.
    ///
    /// Inspect [`snapshot()`](Self::snapshot) and validate pending changes while
    /// holding this lock. The callback receives the native writer, not reference
    /// policy. On failure, the manifest is unchanged and the lock is released.
    pub fn append(
        self,
        header: Header,
        write: impl FnOnce(&mut Writer<&mut dyn Write>) -> Result<()>,
    ) -> Result<Snapshot> {
        self.append_with(header, write, std::fs::File::sync_all, std::fs::File::sync_all)
    }

    fn append_with(
        self,
        header: Header,
        write: impl FnOnce(&mut Writer<&mut dyn Write>) -> Result<()>,
        sync_table: impl FnOnce(&std::fs::File) -> io::Result<()>,
        sync_manifest: impl FnOnce(&std::fs::File) -> io::Result<()>,
    ) -> Result<Snapshot> {
        if header.min_update_index < self.next_update_index()? {
            bail!(conflict(
                "The appended table overlaps the locked stack's update indices"
            ));
        }
        let end = self.snapshot.tables.len();
        self.publish_with(
            end..end,
            header,
            |_, out| {
                let mut writer = Writer::new(out, header)?;
                write(&mut writer)?;
                writer.finish()?;
                Ok(())
            },
            sync_table,
            sync_manifest,
        )
    }

    pub(super) fn publish_with(
        mut self,
        range: Range<usize>,
        header: Header,
        write: impl FnOnce(&Snapshot, &mut dyn Write) -> Result<()>,
        sync_table: impl FnOnce(&std::fs::File) -> io::Result<()>,
        sync_manifest: impl FnOnce(&std::fs::File) -> io::Result<()>,
    ) -> Result<Snapshot> {
        if range.start > range.end || range.end > self.snapshot.tables.len() {
            bail!(validation("Publication range is outside the locked stack"));
        }
        if header.object_hash != self.snapshot.object_hash {
            bail!(validation("The new table uses a different object-hash kind"));
        }
        let directory = self._lock.resource_path();
        let directory = directory.parent().expect("tables.list is within a stack directory");
        let mut tempfile = gix_lock::tempfile::new(
            directory,
            gix_lock::tempfile::ContainingDirectory::Exists,
            gix_lock::tempfile::AutoRemove::Tempfile,
        )
        .or_raise(|| message!("Could not create a reftable in {}", directory.display()))?;
        let permissions = self
            ._lock
            .with_mut(|file| file.metadata().map(|metadata| metadata.permissions()))
            .or_raise(|| message!("Could not read permissions for {}", self._lock.lock_path().display()))?;
        tempfile
            .with_mut(|file| file.as_file_mut().set_permissions(permissions))
            .and_then(std::convert::identity)
            .or_raise(|| message!("Could not set permissions for the new reftable"))?;
        write(&self.snapshot, &mut tempfile as &mut dyn Write)?;
        tempfile
            .with_mut(|file| sync_table(file.as_file()))
            .and_then(std::convert::identity)
            .or_raise(|| message("Could not sync the new reftable before publication"))?;
        let bytes = tempfile
            .with_mut(|file| {
                let mut bytes = Vec::new();
                file.as_file_mut().rewind()?;
                file.as_file_mut().read_to_end(&mut bytes)?;
                Ok::<_, io::Error>(bytes)
            })
            .and_then(std::convert::identity)
            .or_raise(|| message("Could not read the newly written reftable"))?;
        let table = Table::from_bytes(&bytes).or_raise(|| message("The newly written reftable is invalid"))?;
        if table.header() != &header {
            bail!("The newly written reftable does not match its header");
        }
        let empty = bytes.len() == header.version.header_len() + header.version.footer_len();
        if empty && range.is_empty() {
            bail!(validation("Cannot append an empty reftable"));
        }
        let (replacement, table_path) = if empty {
            drop(tempfile);
            (None, None)
        } else {
            let unique = gix_utils::rng::usize(..=u32::MAX as usize);
            let name = format!(
                "0x{:012x}-0x{:012x}-{unique:08x}.ref",
                header.min_update_index, header.max_update_index
            );
            let table_path = directory.join(&name);
            match std::fs::symlink_metadata(&table_path) {
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.and_raise(message!("Could not check {}", table_path.display()))),
                Ok(_) => bail!("Reftable {} already exists".conflict(), table_path.display()),
            }
            match tempfile
                .persist(&table_path)
                .or_raise(|| message!("Could not publish reftable {}", table_path.display()))?
            {
                Some(file) => drop(file),
                None => bail!("Reftable {} vanished before publication", table_path.display()),
            }
            (
                Some(TableData {
                    name: BString::from(name),
                    data: bytes,
                }),
                Some(table_path),
            )
        };

        let result = (|| -> Result<()> {
            for table in self.snapshot.tables[..range.start]
                .iter()
                .chain(replacement.iter())
                .chain(self.snapshot.tables[range.end..].iter())
            {
                self._lock
                    .write_all(table.name.as_bytes())
                    .and_then(|()| self._lock.write_all(b"\n"))
                    .or_raise(|| message!("Could not update {}", self._lock.lock_path().display()))?;
            }
            self._lock
                .with_mut(|file| sync_manifest(file))
                .or_raise(|| message!("Could not sync {}", self._lock.lock_path().display()))?;
            self._lock.commit().map_err(|err| {
                err.error.and_raise(message!(
                    "Could not publish {}",
                    directory.join("tables.list").display()
                ))
            })?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.snapshot.tables.splice(range, replacement);
                Ok(self.snapshot)
            }
            Err(err) => {
                if let Some(table_path) = table_path {
                    std::fs::remove_file(&table_path)
                        .or_raise(|| message!("Could not remove unpublished {} after {err}", table_path.display()))?;
                }
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, io};

    use super::*;
    use crate::{ReferenceRef, Version, reference::ValueRef};

    #[test]
    fn sync_failures_roll_back_and_preserve_durability_order() -> gix_testtools::TestResult {
        for fail_table_sync in [true, false] {
            let dir = gix_testtools::tempfile::TempDir::new()?;
            let object_hash = gix_testtools::object_hash();
            let stack = Stack::at(dir.path(), object_hash);
            let locked = stack.lock(gix_lock::acquire::Fail::Immediately)?;
            let header = Header {
                version: if object_hash.len_in_bytes() == 20 {
                    Version::V1
                } else {
                    Version::V2
                },
                object_hash,
                block_size: 0,
                min_update_index: 1,
                max_update_index: 1,
            };
            let table_synced = Cell::new(false);
            let manifest_synced = Cell::new(false);
            let result = locked.append_with(
                header,
                |writer| {
                    writer.push_reference(ReferenceRef {
                        name: bstr::BStr::new(b"refs/a"),
                        update_index: 1,
                        value: ValueRef::Deletion,
                    })
                },
                |file| {
                    assert!(
                        !dir.path().join("tables.list").exists(),
                        "the manifest is not yet published"
                    );
                    if fail_table_sync {
                        Err(io::Error::other("injected table sync failure"))
                    } else {
                        file.sync_all()?;
                        table_synced.set(true);
                        Ok(())
                    }
                },
                |file| {
                    assert!(table_synced.get(), "the table was synced before the manifest");
                    let entries = std::fs::read_dir(dir.path())?.collect::<io::Result<Vec<_>>>()?;
                    assert!(
                        entries
                            .iter()
                            .any(|entry| entry.path().extension().is_some_and(|ext| ext == "ref")),
                        "the table is published before syncing the manifest"
                    );
                    assert!(
                        !dir.path().join("tables.list").exists(),
                        "the manifest is not yet published"
                    );
                    assert!(file.metadata()?.len() > 0, "the lock contains the new table name");
                    manifest_synced.set(true);
                    Err(io::Error::other("injected manifest sync failure"))
                },
            );
            assert!(result.is_err(), "an injected failure must abort publication");
            assert_eq!(
                table_synced.get(),
                !fail_table_sync,
                "only successful table syncs advance"
            );
            assert_eq!(
                manifest_synced.get(),
                !fail_table_sync,
                "manifest sync follows table publication"
            );
            assert!(
                std::fs::read_dir(dir.path())?.next().is_none(),
                "rollback removes both the table and the manifest lock"
            );
            assert!(
                stack.snapshot()?.table_names().next().is_none(),
                "failed publication leaves the stack empty"
            );
        }
        Ok(())
    }
}
