use std::io::Write;

use bstr::ByteSlice;
use gix_error::{OptionExt, Result, ResultExt, bail, message, unsupported, validation};

use crate::{Header, ReferenceRef, Version, reference::ValueRef};

const MAX_BLOCK_SIZE: usize = 0x00ff_ffff;
const RESTART_INTERVAL: usize = 16;

/// A table encoder that accepts references in increasing byte order.
pub struct Writer<W> {
    out: W,
    header: Header,
    block: Vec<u8>,
    record: Vec<u8>,
    previous_key: Vec<u8>,
    restarts: Vec<usize>,
    reference_count: usize,
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
        Ok(Writer {
            out,
            header,
            block,
            record: Vec::new(),
            previous_key: Vec::new(),
            restarts: Vec::new(),
            reference_count: 0,
        })
    }

    /// Encode the next stored reference, including deletion and symbolic values.
    pub fn push_reference(&mut self, reference: ReferenceRef<'_>) -> Result<()> {
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
        let restart = self.reference_count.is_multiple_of(RESTART_INTERVAL);
        let prefix = if restart {
            0
        } else {
            key.iter()
                .zip(&self.previous_key)
                .take_while(|(left, right)| left == right)
                .count()
        };
        let suffix = &key[prefix..];
        let suffix_and_type = u64::try_from(suffix.len())
            .ok()
            .and_then(|length| length.checked_mul(8))
            .and_then(|length| length.checked_add(value_type))
            .ok_or_raise(|| validation("Reference name is too long to encode"))?;
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

        self.record.clear();
        self.record
            .try_reserve(capacity)
            .or_raise(|| message("Could not allocate the reference record buffer"))?;
        varint(
            u64::try_from(prefix).or_raise(|| validation("Reference prefix is too long"))?,
            &mut self.record,
        );
        varint(suffix_and_type, &mut self.record);
        self.record.extend_from_slice(suffix);
        varint(delta, &mut self.record);
        match reference.value {
            ValueRef::Deletion => {}
            ValueRef::Object { object_id, peeled_id } => {
                self.record.extend_from_slice(object_id.as_bytes());
                if let Some(peeled_id) = peeled_id {
                    self.record.extend_from_slice(peeled_id.as_bytes());
                }
            }
            ValueRef::Symbolic(target) => {
                varint(
                    u64::try_from(target.len()).or_raise(|| validation("Symbolic target is too long"))?,
                    &mut self.record,
                );
                self.record.extend_from_slice(target.as_bytes());
            }
        }

        let restart_count = self.restarts.len() + usize::from(restart);
        if restart_count > u16::MAX as usize {
            bail!(validation("Too many restart points in a reference block"));
        }
        let block_size = self
            .block
            .len()
            .checked_add(self.record.len())
            .and_then(|length| length.checked_add(restart_count * 3 + 2))
            .filter(|length| *length <= self.block_size())
            .ok_or_raise(|| validation("Reference block exceeds the table's block size"))?;
        self.block
            .try_reserve(self.record.len())
            .or_raise(|| message("Could not allocate the reftable buffer"))?;
        self.previous_key
            .try_reserve(key.len().saturating_sub(self.previous_key.len()))
            .or_raise(|| message("Could not allocate the reference-key buffer"))?;
        if restart {
            self.restarts.push(self.block.len());
        }
        self.block.extend_from_slice(&self.record);
        self.previous_key.clear();
        self.previous_key.extend_from_slice(key);
        self.reference_count += 1;
        debug_assert_eq!(self.block.len() + self.restarts.len() * 3 + 2, block_size);
        Ok(())
    }

    /// Finish the table and return the output after writing its footer.
    pub fn finish(mut self) -> Result<W> {
        let header_len = self.header.version.header_len();
        let restart_bytes = if self.reference_count == 0 {
            0
        } else {
            self.restarts.len() * 3 + 2
        };
        self.block
            .try_reserve(restart_bytes)
            .or_raise(|| message("Could not allocate the reference-block restarts"))?;
        if self.reference_count == 0 {
            self.block.truncate(header_len);
        } else {
            let block_size = self.block.len() + restart_bytes;
            let block_size = u32::try_from(block_size).or_raise(|| validation("Reference block is too large"))?;
            self.block[header_len + 1..header_len + 4].copy_from_slice(&block_size.to_be_bytes()[1..]);
            for offset in &self.restarts {
                let offset = u32::try_from(*offset).or_raise(|| validation("Restart offset is too large"))?;
                self.block.extend_from_slice(&offset.to_be_bytes()[1..]);
            }
            self.block
                .extend_from_slice(&(self.restarts.len() as u16).to_be_bytes());
        }
        let mut footer = [0; 72];
        footer[..header_len].copy_from_slice(&self.block[..header_len]);
        let footer_end = header_len + 40;
        let checksum = crc32fast::hash(&footer[..footer_end]);
        footer[footer_end..footer_end + 4].copy_from_slice(&checksum.to_be_bytes());
        self.out
            .write_all(&self.block)
            .or_raise(|| message("Could not write the reftable block"))?;
        self.out
            .write_all(&footer[..self.header.version.footer_len()])
            .or_raise(|| message("Could not write the reftable footer"))?;
        Ok(self.out)
    }

    fn block_size(&self) -> usize {
        if self.header.block_size == 0 {
            MAX_BLOCK_SIZE
        } else {
            self.header.block_size as usize
        }
    }
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
