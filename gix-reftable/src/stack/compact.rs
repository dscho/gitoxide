use std::{
    io::{self, Write},
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};

use bstr::BString;
use gix_error::{OptionExt, Result, ResultExt, bail, cancelled, conflict, message, validation};
use gix_utils::progress::Progress;

use super::{Locked, Snapshot, log, reference};
use crate::{Header, LogRef, ReferenceRef, Table, log::DataRef, reference::ValueRef, write::Writer};

/// Caller-resolved settings for writing a compacted table.
#[derive(Debug, Clone, Copy)]
pub struct CompactionOptions {
    /// Alignment of written reference blocks, or zero for unaligned tables.
    pub block_size: u32,
    /// Reflog expiry limits. The default retains every update.
    pub log_retention: LogRetention,
}

impl Default for CompactionOptions {
    fn default() -> Self {
        CompactionOptions {
            block_size: 4096,
            log_retention: LogRetention::default(),
        }
    }
}

/// Names of contiguous tables selected from one immutable snapshot.
#[derive(Debug)]
pub struct TableRange {
    names: Vec<BString>,
}

impl TableRange {
    fn locate(&self, snapshot: &Snapshot) -> Result<Range<usize>> {
        let start = snapshot
            .tables
            .iter()
            .position(|table| table.name == self.names[0])
            .ok_or_raise(|| conflict("Selected reftables no longer exist in the locked stack"))?;
        let end = start
            .checked_add(self.names.len())
            .filter(|end| *end <= snapshot.tables.len())
            .ok_or_raise(|| conflict("Selected reftables no longer form a complete range"))?;
        if !snapshot.tables[start..end]
            .iter()
            .zip(&self.names)
            .all(|(table, name)| table.name == *name)
        {
            bail!(conflict("Selected reftables changed in the locked stack"));
        }
        Ok(start..end)
    }
}

/// Caller-supplied, inclusive boundaries for keeping reflog updates.
#[derive(Debug, Default, Clone, Copy)]
pub struct LogRetention {
    /// Discard updates whose index is below this value.
    pub min_update_index: Option<u64>,
    /// Discard updates whose timestamp in seconds is below this value.
    pub min_time_seconds: Option<u64>,
}

impl LogRetention {
    fn keeps(self, record: &log::Record) -> bool {
        self.min_update_index.is_none_or(|min| record.update_index >= min)
            && self.min_time_seconds.is_none_or(|min| record.time_seconds >= min)
    }
}

pub(super) fn check_interrupt(should_interrupt: &AtomicBool) -> Result<()> {
    if should_interrupt.load(Ordering::Relaxed) {
        bail!(cancelled("Reftable compaction interrupted"));
    }
    Ok(())
}

impl Snapshot {
    /// Remember the table identities in `range` for a later locked compaction.
    ///
    /// Publication resolves the names again under the manifest lock, so a
    /// concurrent change cannot silently redirect the range to other tables.
    pub fn select_tables(&self, range: Range<usize>) -> Result<TableRange> {
        self.validate_compaction_range(&range)?;
        let mut names = Vec::new();
        names
            .try_reserve_exact(range.len())
            .or_raise(|| message("Could not allocate selected table names"))?;
        names.extend(self.tables[range].iter().map(|table| table.name.clone()));
        Ok(TableRange { names })
    }

    fn validate_compaction_range(&self, range: &Range<usize>) -> Result<()> {
        if range.start >= range.end || range.end > self.tables.len() {
            bail!(validation("Compaction range does not select stack tables"));
        }
        Ok(())
    }

    /// Write the merged records of `range` in oldest-to-newest table order.
    ///
    /// Tombstones are retained unless the range begins at the bottom of the
    /// stack. The returned count is zero for an empty replacement table.
    /// Neither the stack nor its manifest is modified.
    ///
    /// Partial ranges turn expired logs into tombstones, so entries in lower
    /// tables cannot reappear. To expire every old log, compact the whole stack.
    ///
    /// Count written records in `progress` and check `should_interrupt` between
    /// records. Use [`gix_utils::progress::Discard`] when no progress is needed.
    pub fn compact_into<W: Write>(
        &self,
        range: Range<usize>,
        out: W,
        progress: &mut dyn Progress,
        should_interrupt: &AtomicBool,
        options: CompactionOptions,
    ) -> Result<(W, usize)> {
        check_interrupt(should_interrupt)?;
        let header = self.compaction_header(&range, options.block_size)?;
        let tables = self.parsed_range(range.clone())?;
        progress.init(None, gix_utils::progress::count("records"));
        let mut writer = Writer::new(out, header)?;
        let mut count = 0;
        let mut references = reference::Iter::from_tables(tables.clone(), None)?;
        loop {
            check_interrupt(should_interrupt)?;
            let Some(record) = references.next_stored()? else { break };
            if range.start == 0 && record.value.is_none() {
                continue;
            }
            let value = match &record.value {
                None => ValueRef::Deletion,
                Some(reference::Value::Object { object_id, peeled_id }) => ValueRef::Object {
                    object_id: object_id.as_ref(),
                    peeled_id: peeled_id.as_ref().map(AsRef::as_ref),
                },
                Some(reference::Value::Symbolic(target)) => ValueRef::Symbolic(target.as_ref()),
            };
            writer.push_reference(ReferenceRef {
                name: record.name.as_ref(),
                update_index: record.update_index,
                value,
            })?;
            count += 1;
            progress.inc();
        }
        let mut logs = log::Iter::from_tables(tables, None)?;
        loop {
            check_interrupt(should_interrupt)?;
            let Some(record) = logs.next_stored()? else { break };
            let keep = record
                .data
                .as_ref()
                .is_some_and(|data| options.log_retention.keeps(data));
            if range.start == 0 && !keep {
                continue;
            }
            writer.push_log(LogRef {
                name: record.name.as_ref(),
                update_index: record.update_index,
                data: record.data.as_ref().filter(|_| keep).map(|data| DataRef {
                    previous_object_id: data.previous_object_id.as_ref(),
                    new_object_id: data.new_object_id.as_ref(),
                    committer: data.committer.to_ref(),
                    time_seconds: data.time_seconds,
                    timezone_minutes: data.timezone_minutes,
                    message: data.message.as_ref(),
                }),
            })?;
            count += 1;
            progress.inc();
        }
        check_interrupt(should_interrupt)?;
        Ok((writer.finish()?, count))
    }

    pub(super) fn compaction_header(&self, range: &Range<usize>, block_size: u32) -> Result<Header> {
        self.validate_compaction_range(range)?;
        let first = *Table::from_bytes(&self.tables[range.start].data)?.header();
        let last = *Table::from_bytes(&self.tables[range.end - 1].data)?.header();
        Ok(Header {
            version: first.version,
            object_hash: self.object_hash,
            block_size,
            min_update_index: first.min_update_index,
            max_update_index: last.max_update_index,
        })
    }
}

impl Locked {
    /// Replace `selection` with its compacted records under the manifest lock.
    ///
    /// Select tables with [`Snapshot::select_tables()`]; their names are
    /// validated against the locked stack, even if the manifest has shifted.
    /// Retired files remain until separate cleanup.
    ///
    /// The manifest lock is held for the complete rewrite and publication.
    /// Concurrent writers may time out.
    pub fn compact(
        self,
        selection: TableRange,
        progress: &mut dyn Progress,
        should_interrupt: &AtomicBool,
        options: CompactionOptions,
    ) -> Result<Snapshot> {
        self.compact_with(
            selection,
            progress,
            should_interrupt,
            options,
            std::fs::File::sync_all,
            std::fs::File::sync_all,
        )
    }

    fn compact_with(
        self,
        selection: TableRange,
        progress: &mut dyn Progress,
        should_interrupt: &AtomicBool,
        options: CompactionOptions,
        sync_table: impl FnOnce(&std::fs::File) -> io::Result<()>,
        sync_manifest: impl FnOnce(&std::fs::File) -> io::Result<()>,
    ) -> Result<Snapshot> {
        check_interrupt(should_interrupt)?;
        let range = selection.locate(&self.snapshot)?;
        let header = self.snapshot.compaction_header(&range, options.block_size)?;
        let selected = range.clone();
        self.publish_with(
            range,
            header,
            move |snapshot, out| {
                snapshot
                    .compact_into(selected, out, progress, should_interrupt, options)
                    .map(|_| ())
            },
            sync_table,
            sync_manifest,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Version, stack::Stack};

    #[test]
    fn manifest_sync_failures_keep_the_original_tables() -> gix_testtools::TestResult {
        for empty in [false, true] {
            let dir = gix_testtools::tempfile::TempDir::new()?;
            let object_hash = gix_testtools::object_hash();
            let stack = Stack::at(dir.path(), object_hash);
            for index in 1..=2 {
                stack.lock(gix_lock::acquire::Fail::Immediately)?.append(
                    Header {
                        version: if object_hash.len_in_bytes() == 20 {
                            Version::V1
                        } else {
                            Version::V2
                        },
                        object_hash,
                        block_size: 0,
                        min_update_index: index,
                        max_update_index: index,
                    },
                    |writer| {
                        writer.push_reference(ReferenceRef {
                            name: bstr::BStr::new(if !empty && index == 2 { b"refs/b" } else { b"refs/a" }),
                            update_index: index,
                            value: if empty && index == 2 {
                                ValueRef::Deletion
                            } else {
                                ValueRef::Symbolic(bstr::BStr::new(b"refs/target"))
                            },
                        })
                    },
                )?;
            }
            let original = std::fs::read(dir.path().join("tables.list"))?;
            let locked = stack.lock(gix_lock::acquire::Fail::Immediately)?;
            let selection = locked.snapshot().select_tables(0..2)?;
            assert!(
                locked
                    .compact_with(
                        selection,
                        &mut gix_utils::progress::Discard,
                        &AtomicBool::new(false),
                        CompactionOptions {
                            block_size: 0,
                            ..Default::default()
                        },
                        std::fs::File::sync_all,
                        |file| {
                            assert_eq!(
                                file.metadata()?.len() == 0,
                                empty,
                                "the replacement manifest omits empty tables"
                            );
                            Err(std::io::Error::other("injected manifest sync failure"))
                        },
                    )
                    .is_err(),
                "the failed manifest must not be committed"
            );
            assert_eq!(
                std::fs::read(dir.path().join("tables.list"))?,
                original,
                "the published manifest is unchanged"
            );
            let entries = std::fs::read_dir(dir.path())?.collect::<std::io::Result<Vec<_>>>()?;
            assert_eq!(
                entries
                    .iter()
                    .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "ref"))
                    .count(),
                2,
                "the unpublished replacement is removed"
            );
            assert!(
                !dir.path().join("tables.list.lock").exists(),
                "failure releases the manifest lock"
            );
            assert_eq!(
                stack.snapshot()?.table_names().len(),
                2,
                "reloading still sees the original tables"
            );
        }
        Ok(())
    }
}
