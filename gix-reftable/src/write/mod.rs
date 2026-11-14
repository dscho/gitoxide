use std::io::Write;

use bstr::ByteSlice;
use gix_error::{OptionExt, Result, ResultExt, bail, message, unsupported, validation};

use crate::{Header, ReferenceRef, Version, reference::ValueRef};

const MAX_BLOCK_SIZE: usize = 0x00ff_ffff;
const DEFAULT_BLOCK_SIZE: usize = 4096;
const RESTART_INTERVAL: usize = 16;

mod log;

struct IndexEntry {
    key: Vec<u8>,
    offset: u64,
}

/// A table encoder accepting sorted references followed by sorted reflogs.
pub struct Writer<W> {
    out: W,
    header: Header,
    failed: bool,
    log_start: Option<u64>,
    ref_index_root: u64,
    header_bytes: [u8; 28],
    offset: u64,
    block: Vec<u8>,
    record: Vec<u8>,
    previous_key: Vec<u8>,
    restarts: Vec<usize>,
    record_count: usize,
    index: Vec<IndexEntry>,
    log_index: Vec<IndexEntry>,
}

impl<W: Write> Writer<W> {
    /// Start a table in `out` with explicit object-hash, block-size, and update-index bounds.
    pub fn new(out: W, header: Header) -> Result<Self> {
        if header.min_update_index > header.max_update_index {
            bail!(validation("Reftable update-index bounds are reversed"));
        }
        if header.block_size as usize > MAX_BLOCK_SIZE {
            bail!(validation("Reftable block size exceeds the format limit"));
        }
        let hash_id = header.object_hash as u8;
        if header.version == Version::V1 && hash_id != 1 {
            bail!(validation("Version 1 tables require SHA-1"));
        }
        let mut block = Vec::with_capacity(header.version.header_len() + 4);
        block.extend_from_slice(b"REFT");
        block.push(if header.version == Version::V1 { 1 } else { 2 });
        block.extend_from_slice(&header.block_size.to_be_bytes()[1..]);
        block.extend_from_slice(&header.min_update_index.to_be_bytes());
        block.extend_from_slice(&header.max_update_index.to_be_bytes());
        if header.version == Version::V2 {
            match hash_id {
                1 => block.extend_from_slice(b"sha1"),
                2 => block.extend_from_slice(b"s256"),
                _ => bail!(unsupported("This build cannot write the table's object-hash kind")),
            }
        }
        block.extend_from_slice(b"r\0\0\0");
        let mut header_bytes = [0; 28];
        header_bytes[..header.version.header_len()].copy_from_slice(&block[..header.version.header_len()]);
        Ok(Writer {
            out,
            header,
            failed: false,
            log_start: None,
            ref_index_root: 0,
            header_bytes,
            offset: 0,
            block,
            record: Vec::new(),
            previous_key: Vec::new(),
            restarts: Vec::new(),
            record_count: 0,
            index: Vec::new(),
            log_index: Vec::new(),
        })
    }

    /// Encode the next stored reference, including deletion and symbolic values.
    pub fn push_reference(&mut self, reference: ReferenceRef<'_>) -> Result<()> {
        if self.failed {
            bail!(validation("Cannot add records after a failed table write"));
        }
        if self.log_start.is_some() {
            bail!(validation("References cannot follow reflogs in a table"));
        }
        let key = reference.name.as_bytes();
        if key.is_empty() || (!self.previous_key.is_empty() && key <= self.previous_key.as_slice()) {
            bail!(validation("Reference names must be nonempty and strictly increasing"));
        }
        let delta = reference
            .update_index
            .checked_sub(self.header.min_update_index)
            .filter(|_| reference.update_index <= self.header.max_update_index)
            .ok_or_raise(|| validation("Reference update index is outside the table's bounds"))?;
        let (value_type, value_len) = match reference.value {
            ValueRef::Deletion => (0, 0),
            ValueRef::Object { object_id, peeled_id } => {
                if object_id.kind() != self.header.object_hash
                    || peeled_id.is_some_and(|id| id.kind() != self.header.object_hash)
                {
                    bail!(validation("Reference object IDs use a different hash kind"));
                }
                (
                    if peeled_id.is_some() { 2 } else { 1 },
                    self.header.object_hash.len_in_bytes() * (1 + usize::from(peeled_id.is_some())),
                )
            }
            ValueRef::Symbolic(target) => (3, target.len()),
        };
        let value_bytes = key
            .len()
            .checked_add(value_len)
            .ok_or_raise(|| validation("Reference value is too long to encode"))?;
        if value_bytes >= self.block_size() {
            bail!(validation("Reference record exceeds the table's block size"));
        }
        let capacity = value_bytes
            .checked_add(40)
            .ok_or_raise(|| validation("Reference value is too long to encode"))?;

        let restart = self.record_count.is_multiple_of(RESTART_INTERVAL);
        self.record.clear();
        self.record
            .try_reserve(capacity)
            .or_raise(|| message("Could not allocate the reference record buffer"))?;
        encode_key(&mut self.record, key, &self.previous_key, restart, value_type)?;
        varint(delta, &mut self.record);
        encode_reference_value(&mut self.record, reference.value)?;
        if self.append_record(key, restart, self.block_size())? {
            return Ok(());
        }
        if self.record_count == 0 {
            bail!(validation("Reference record exceeds the table's block size"));
        }

        self.record.clear();
        encode_key(&mut self.record, key, &[], true, value_type)?;
        varint(delta, &mut self.record);
        encode_reference_value(&mut self.record, reference.value)?;
        if self
            .record
            .len()
            .checked_add(9)
            .is_none_or(|size| size > self.block_size())
        {
            bail!(validation("Reference record exceeds the table's block size"));
        }
        let entry = self.flush_block(true)?;
        self.index.push(entry);
        self.failed = true;
        if !self.append_record(key, true, self.block_size())? {
            bail!(validation("Reference record could not start a new block"));
        }
        self.failed = false;
        Ok(())
    }

    /// Finish the table and return the output after writing its footer.
    pub fn finish(mut self) -> Result<W> {
        if self.failed {
            bail!(validation("Cannot finish a table after a failed write"));
        }
        let header_len = self.header.version.header_len();
        let (root, log_root) = if self.log_start.is_some() {
            let indexed = !self.log_index.is_empty();
            let entry = self.flush_log_block()?;
            if indexed {
                self.log_index.push(entry);
                let level = std::mem::take(&mut self.log_index);
                (self.ref_index_root, self.write_index(level, false)?)
            } else {
                (self.ref_index_root, 0)
            }
        } else if self.record_count == 0 {
            self.block.truncate(header_len);
            self.out
                .write_all(&self.block)
                .or_raise(|| message("Could not write the reftable block"))?;
            (0, 0)
        } else {
            let indexed = !self.index.is_empty();
            let entry = self.flush_block(indexed)?;
            if indexed {
                self.index.push(entry);
            }
            let root = if indexed {
                let level = std::mem::take(&mut self.index);
                self.write_index(level, true)?
            } else {
                0
            };
            (root, 0)
        };
        let mut footer = [0; 72];
        footer[..header_len].copy_from_slice(&self.header_bytes[..header_len]);
        footer[header_len..header_len + 8].copy_from_slice(&root.to_be_bytes());
        if let Some(start) = self.log_start {
            footer[header_len + 24..header_len + 32].copy_from_slice(&start.to_be_bytes());
        }
        footer[header_len + 32..header_len + 40].copy_from_slice(&log_root.to_be_bytes());
        let footer_end = header_len + 40;
        let checksum = crc32fast::hash(&footer[..footer_end]);
        footer[footer_end..footer_end + 4].copy_from_slice(&checksum.to_be_bytes());
        self.out
            .write_all(&footer[..self.header.version.footer_len()])
            .or_raise(|| message("Could not write the reftable footer"))?;
        Ok(self.out)
    }

    fn block_size(&self) -> usize {
        if self.header.block_size == 0 {
            DEFAULT_BLOCK_SIZE
        } else {
            self.header.block_size as usize
        }
    }

    fn append_record(&mut self, key: &[u8], restart: bool, limit: usize) -> Result<bool> {
        let restart_count = self.restarts.len() + usize::from(restart);
        if restart_count > u16::MAX as usize
            || self
                .block
                .len()
                .checked_add(self.record.len())
                .and_then(|size| size.checked_add(restart_count * 3 + 2))
                .is_none_or(|size| size > limit)
        {
            return Ok(false);
        }
        self.block
            .try_reserve(self.record.len())
            .or_raise(|| message("Could not allocate the reftable block"))?;
        self.previous_key
            .try_reserve(key.len().saturating_sub(self.previous_key.len()))
            .or_raise(|| message("Could not allocate the record-key buffer"))?;
        if restart {
            self.restarts.push(self.block.len());
        }
        self.block.extend_from_slice(&self.record);
        self.previous_key.clear();
        self.previous_key.extend_from_slice(key);
        self.record_count += 1;
        Ok(true)
    }

    fn flush_block(&mut self, aligned: bool) -> Result<IndexEntry> {
        self.failed = true;
        let mut key = Vec::new();
        key.try_reserve(self.previous_key.len())
            .or_raise(|| message("Could not allocate the index key"))?;
        key.extend_from_slice(&self.previous_key);
        let header_skip = if self.offset == 0 {
            self.header.version.header_len()
        } else {
            0
        };
        let kind = self.block[header_skip];
        let size = seal_block(&mut self.block, &self.restarts, header_skip)?;
        if aligned && self.header.block_size != 0 {
            let padding = self
                .block_size()
                .checked_sub(size)
                .ok_or_raise(|| validation("Block exceeds the table's block size"))?;
            self.block
                .try_reserve(padding)
                .or_raise(|| message("Could not allocate the aligned block"))?;
            self.block.resize(self.block_size(), 0);
        }
        let next_offset = self
            .offset
            .checked_add(self.block.len() as u64)
            .ok_or_raise(|| validation("Reftable offset exceeds the format limit"))?;
        self.out.write_all(&self.block).or_raise(|| {
            if kind == b'i' {
                message("Could not write the reftable index")
            } else {
                message("Could not write the reference block")
            }
        })?;
        let entry = IndexEntry {
            key,
            offset: self.offset,
        };
        self.offset = next_offset;
        self.block.clear();
        self.block.extend_from_slice(&[kind, 0, 0, 0]);
        self.restarts.clear();
        self.record_count = 0;
        self.failed = false;
        Ok(entry)
    }

    fn write_index(&mut self, mut level: Vec<IndexEntry>, aligned: bool) -> Result<u64> {
        loop {
            self.block.clear();
            self.block.extend_from_slice(b"i\0\0\0");
            self.previous_key.clear();
            self.restarts.clear();
            self.record_count = 0;
            let mut next = Vec::new();
            for entry in &level {
                let capacity = entry
                    .key
                    .len()
                    .checked_add(40)
                    .ok_or_raise(|| validation("Index key is too long"))?;
                loop {
                    let restart = self.record_count.is_multiple_of(RESTART_INTERVAL);
                    self.record.clear();
                    self.record
                        .try_reserve(capacity)
                        .or_raise(|| message("Could not allocate the index record"))?;
                    encode_key(&mut self.record, &entry.key, &self.previous_key, restart, 0)?;
                    varint(entry.offset, &mut self.record);
                    if self.append_record(&entry.key, restart, self.block_size())? {
                        break;
                    }
                    if self.record_count == 0 {
                        bail!(validation("Index record exceeds the table's block size"));
                    }
                    next.push(self.flush_block(aligned)?);
                }
            }
            next.push(self.flush_block(aligned)?);
            if next.len() == 1 {
                return Ok(next[0].offset);
            }
            if next.len() >= level.len() {
                bail!(validation("Reftable index cannot fit a root block"));
            }
            level = next;
        }
    }
}

fn encode_reference_value(record: &mut Vec<u8>, value: ValueRef<'_>) -> Result<()> {
    match value {
        ValueRef::Deletion => {}
        ValueRef::Object { object_id, peeled_id } => {
            record.extend_from_slice(object_id.as_bytes());
            if let Some(peeled_id) = peeled_id {
                record.extend_from_slice(peeled_id.as_bytes());
            }
        }
        ValueRef::Symbolic(target) => {
            varint(
                u64::try_from(target.len()).or_raise(|| validation("Symbolic target is too long"))?,
                record,
            );
            record.extend_from_slice(target.as_bytes());
        }
    }
    Ok(())
}

fn seal_block(block: &mut Vec<u8>, restarts: &[usize], header_skip: usize) -> Result<usize> {
    let count = u16::try_from(restarts.len()).or_raise(|| validation("Too many restart points"))?;
    let size = block
        .len()
        .checked_add(restarts.len() * 3 + 2)
        .filter(|size| *size <= MAX_BLOCK_SIZE)
        .ok_or_raise(|| validation("Reftable block exceeds the format's size limit"))?;
    block
        .try_reserve(restarts.len() * 3 + 2)
        .or_raise(|| message("Could not allocate the block restarts"))?;
    let size_bytes = u32::try_from(size)
        .or_raise(|| validation("Reftable block is too large"))?
        .to_be_bytes();
    block[header_skip + 1..header_skip + 4].copy_from_slice(&size_bytes[1..]);
    for offset in restarts {
        let bytes = u32::try_from(*offset)
            .or_raise(|| validation("Restart offset is too large"))?
            .to_be_bytes();
        block.extend_from_slice(&bytes[1..]);
    }
    block.extend_from_slice(&count.to_be_bytes());
    Ok(size)
}

fn encode_key(record: &mut Vec<u8>, key: &[u8], previous: &[u8], restart: bool, value_type: u64) -> Result<()> {
    let prefix = if restart {
        0
    } else {
        key.iter()
            .zip(previous)
            .take_while(|(left, right)| left == right)
            .count()
    };
    let suffix = &key[prefix..];
    let suffix_and_type = u64::try_from(suffix.len())
        .ok()
        .and_then(|length| length.checked_mul(8))
        .and_then(|length| length.checked_add(value_type))
        .ok_or_raise(|| validation("Record key is too long to encode"))?;
    varint(
        u64::try_from(prefix).or_raise(|| validation("Record prefix is too long"))?,
        record,
    );
    varint(suffix_and_type, record);
    record.extend_from_slice(suffix);
    Ok(())
}

fn varint(mut value: u64, out: &mut Vec<u8>) {
    let mut bytes = [0; 10];
    let mut pos = bytes.len() - 1;
    bytes[pos] = (value & 127) as u8;
    while value > 127 {
        value = (value >> 7) - 1;
        pos -= 1;
        bytes[pos] = (value & 127) as u8 | 128;
    }
    out.extend_from_slice(&bytes[pos..]);
}
