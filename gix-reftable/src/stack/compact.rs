use std::{io::Write, ops::Range};

use gix_error::{Result, bail, validation};

use super::{Snapshot, log, reference};
use crate::{Header, LogRef, ReferenceRef, log::DataRef, reference::ValueRef, write::Writer};

/// Caller-resolved settings for writing a compacted table.
#[derive(Debug, Clone, Copy)]
pub struct CompactionOptions {
    /// Alignment of written reference blocks, or zero for unaligned tables.
    pub block_size: u32,
}

impl Default for CompactionOptions {
    fn default() -> Self {
        CompactionOptions { block_size: 4096 }
    }
}

impl Snapshot {
    /// Write the merged records of `range` in oldest-to-newest table order.
    ///
    /// Tombstones are retained unless the range begins at the bottom of the
    /// stack. The returned count is zero for an empty replacement table.
    /// Neither the stack nor its manifest is modified.
    pub fn compact_into<W: Write>(
        &self,
        range: Range<usize>,
        out: W,
        options: CompactionOptions,
    ) -> Result<(W, usize)> {
        if range.start >= range.end || range.end > self.tables.len() {
            bail!(validation("Compaction range does not select stack tables"));
        }
        let tables = self.parsed_range(range.clone())?;
        let first = tables.first().expect("the range contains at least one table").header();
        let last = tables.last().expect("the range contains at least one table").header();
        let mut writer = Writer::new(
            out,
            Header {
                version: first.version,
                object_hash: self.object_hash,
                block_size: options.block_size,
                min_update_index: first.min_update_index,
                max_update_index: last.max_update_index,
            },
        )?;
        let mut count = 0;
        let mut references = reference::Iter::from_tables(tables.clone(), None)?;
        while let Some(record) = references.next_stored()? {
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
        }
        let mut logs = log::Iter::from_tables(tables, None)?;
        while let Some(record) = logs.next_stored()? {
            if range.start == 0 && record.data.is_none() {
                continue;
            }
            writer.push_log(LogRef {
                name: record.name.as_ref(),
                update_index: record.update_index,
                data: record.data.as_ref().map(|data| DataRef {
                    previous_object_id: data.previous_object_id.as_ref(),
                    new_object_id: data.new_object_id.as_ref(),
                    committer: data.committer.to_ref(),
                    time_seconds: data.time_seconds,
                    timezone_minutes: data.timezone_minutes,
                    message: data.message.as_ref(),
                }),
            })?;
            count += 1;
        }
        Ok((writer.finish()?, count))
    }
}
