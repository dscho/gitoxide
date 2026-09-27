use std::io::Write;

use bstr::ByteSlice;
use gix_error::{OptionExt, Result, ResultExt, bail, message, validation};

use super::{IndexEntry, MAX_BLOCK_SIZE, RESTART_INTERVAL, Writer, encode_key, seal_block, varint};
use crate::{LogRef, log::DataRef};

impl<W: Write> Writer<W> {
    /// Encode a reflog entry after the references, in key order.
    ///
    /// Messages are stored verbatim; callers supply the final LF expected by Git.
    pub fn push_log(&mut self, log: LogRef<'_>) -> Result<()> {
        if self.failed {
            bail!(validation("Cannot add records after a failed table write"));
        }
        if log.name.is_empty() || log.update_index > self.header.max_update_index {
            bail!(validation("Invalid reflog name or update index"));
        }
        let key_len = log
            .name
            .len()
            .checked_add(9)
            .ok_or_raise(|| validation("Reflog key is too long"))?;
        let mut key = Vec::new();
        key.try_reserve(key_len)
            .or_raise(|| message("Could not allocate the reflog key"))?;
        key.extend_from_slice(log.name.as_bytes());
        key.push(0);
        key.extend_from_slice(&(!log.update_index).to_be_bytes());
        if self.log_start.is_some() && key <= self.previous_key {
            bail!(validation("Reflog keys must be strictly increasing"));
        }

        let value_len = if let Some(data) = log.data {
            if data.previous_object_id.kind() != self.header.object_hash
                || data.new_object_id.kind() != self.header.object_hash
            {
                bail!(validation("Reflog object IDs use a different hash kind"));
            }
            [
                data.committer.name.len(),
                data.committer.email.len(),
                data.message.len(),
            ]
            .into_iter()
            .try_fold(2 * self.header.object_hash.len_in_bytes(), usize::checked_add)
            .ok_or_raise(|| validation("Reflog value is too long"))?
        } else {
            0
        };
        let value_bytes = key_len
            .checked_add(value_len)
            .ok_or_raise(|| validation("Reflog value is too long"))?;
        let limit = self.log_block_size();
        if value_bytes >= limit {
            bail!(validation("Reflog record exceeds the table's log block size"));
        }
        let capacity = value_bytes
            .checked_add(40)
            .ok_or_raise(|| validation("Reflog value is too long"))?;
        let first = self.log_start.is_none();
        let restart = first || self.record_count.is_multiple_of(RESTART_INTERVAL);
        self.record.clear();
        self.record
            .try_reserve(capacity)
            .or_raise(|| message("Could not allocate the reflog record"))?;
        encode_key(
            &mut self.record,
            &key,
            &self.previous_key,
            restart,
            u64::from(log.data.is_some()),
        )?;
        if let Some(data) = log.data {
            encode_data(&mut self.record, data)?;
        }
        if first {
            let header_len = if self.offset == 0 {
                self.header.version.header_len()
            } else {
                0
            };
            if self
                .record
                .len()
                .checked_add(header_len + 9)
                .is_none_or(|size| size > limit)
            {
                bail!(validation("Reflog record exceeds the table's log block size"));
            }
            let record = std::mem::take(&mut self.record);
            if let Err(err) = self.start_logs() {
                self.failed = true;
                return Err(err);
            }
            self.record = record;
            self.failed = true;
        }
        if self.append_record(&key, restart, limit)? {
            self.failed = false;
            return Ok(());
        }
        if first || self.record_count == 0 {
            bail!(validation("Reflog block exceeds the table's log block size"));
        }

        self.record.clear();
        encode_key(&mut self.record, &key, &[], true, u64::from(log.data.is_some()))?;
        if let Some(data) = log.data {
            encode_data(&mut self.record, data)?;
        }
        if self.record.len().checked_add(9).is_none_or(|size| size > limit) {
            bail!(validation("Reflog record exceeds the table's log block size"));
        }
        let entry = self.flush_log_block()?;
        self.log_index.push(entry);
        self.failed = true;
        if !self.append_record(&key, true, limit)? {
            bail!(validation("Reflog record could not start a new block"));
        }
        self.failed = false;
        Ok(())
    }

    fn log_block_size(&self) -> usize {
        self.block_size().saturating_mul(2).min(MAX_BLOCK_SIZE)
    }

    fn start_logs(&mut self) -> Result<()> {
        if self.record_count != 0 {
            let indexed = !self.index.is_empty();
            let entry = self.flush_block(true)?;
            if indexed {
                self.index.push(entry);
                let level = std::mem::take(&mut self.index);
                self.ref_index_root = self.write_index(level, true)?;
            }
        }
        self.log_start = Some(self.offset);
        self.block.truncate(if self.offset == 0 {
            self.header.version.header_len()
        } else {
            0
        });
        self.block.extend_from_slice(b"g\0\0\0");
        self.previous_key.clear();
        self.restarts.clear();
        self.record_count = 0;
        Ok(())
    }

    pub(super) fn flush_log_block(&mut self) -> Result<IndexEntry> {
        self.failed = true;
        let mut key = Vec::new();
        key.try_reserve(self.previous_key.len())
            .or_raise(|| message("Could not allocate the reflog-index key"))?;
        key.extend_from_slice(&self.previous_key);
        let header_skip = if self.offset == 0 {
            self.header.version.header_len()
        } else {
            0
        };
        seal_block(&mut self.block, &self.restarts, header_skip)?;
        let mut encoder = gix_zlib::stream::deflate::Write::new(Vec::new(), gix_zlib::Compression::DEFAULT);
        encoder
            .write_all(&self.block[header_skip + 4..])
            .or_raise(|| message("Could not deflate the reflog block"))?;
        encoder
            .flush()
            .or_raise(|| message("Could not finish deflating the reflog block"))?;
        let compressed = encoder.into_inner();
        let physical_len = (header_skip + 4)
            .checked_add(compressed.len())
            .ok_or_raise(|| validation("Compressed reflog block is too large"))?;
        let next_offset = self
            .offset
            .checked_add(physical_len as u64)
            .ok_or_raise(|| validation("Reftable offset exceeds the format limit"))?;
        self.out
            .write_all(&self.block[..header_skip + 4])
            .or_raise(|| message("Could not write the reflog block header"))?;
        self.out
            .write_all(&compressed)
            .or_raise(|| message("Could not write the compressed reflog block"))?;
        let entry = IndexEntry {
            key,
            offset: self.offset,
        };
        self.offset = next_offset;
        self.block.clear();
        self.block.extend_from_slice(b"g\0\0\0");
        self.restarts.clear();
        self.record_count = 0;
        self.failed = false;
        Ok(entry)
    }
}

fn encode_data(out: &mut Vec<u8>, data: DataRef<'_>) -> Result<()> {
    out.extend_from_slice(data.previous_object_id.as_bytes());
    out.extend_from_slice(data.new_object_id.as_bytes());
    for field in [data.committer.name.as_bytes(), data.committer.email.as_bytes()] {
        string(out, field)?;
    }
    varint(data.time_seconds, out);
    out.extend_from_slice(&data.timezone_minutes.to_be_bytes());
    string(out, data.message.as_bytes())
}

fn string(out: &mut Vec<u8>, value: &[u8]) -> Result<()> {
    varint(
        u64::try_from(value.len()).or_raise(|| validation("Reflog string is too long"))?,
        out,
    );
    out.extend_from_slice(value);
    Ok(())
}
