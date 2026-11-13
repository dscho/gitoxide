use gix_error::{OptionExt, Result, ResultExt, message};

use super::{Locked, Snapshot, Stack};
use crate::Table;

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
}
