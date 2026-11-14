use bstr::ByteSlice;
use gix_error::{OptionExt, Result, bail, corruption};

use crate::{ReferenceRef, Table, block, decode};

/// The stored value of a reference record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueRef<'a> {
    /// A tombstone hiding an older value when tables are merged.
    Deletion,
    /// An object ID and, for an annotated tag, its peeled target.
    Object {
        /// The object directly named by this reference.
        object_id: &'a gix_hash::oid,
        /// The peeled object, if recorded.
        peeled_id: Option<&'a gix_hash::oid>,
    },
    /// The uninterpreted target name of a symbolic reference.
    Symbolic(&'a bstr::BStr),
}

/// A reference cursor with a reusable buffer for prefix-compressed names.
///
/// Use [`next_record()`](Self::next_record) to borrow each decoded record until
/// the next call. This avoids allocating a new name for every reference.
pub struct Iter<'a> {
    table: Table<'a>,
    block: Option<(block::Block, block::Position)>,
    block_offset: usize,
    next_block: usize,
    key: Vec<u8>,
    seek_key: Vec<u8>,
    done: bool,
}

impl<'a> Table<'a> {
    /// Iterate reference records in byte order, including symbolic references
    /// and deletion records, without applying repository policy.
    pub fn references(&self) -> Iter<'a> {
        Iter {
            table: *self,
            block: None,
            block_offset: 0,
            next_block: 0,
            key: Vec::new(),
            seek_key: Vec::new(),
            done: self.first_block_kind() != Some(b'r'),
        }
    }
}

impl<'a> Iter<'a> {
    /// Position this cursor so [`next_record()`](Self::next_record) returns the
    /// first reference whose name is greater than or equal to `name`.
    ///
    /// The reference index and block restart points are used when present.
    pub fn seek(&mut self, name: &bstr::BStr) -> Result<()> {
        self.key.clear();
        self.seek_key.clear();
        self.block = None;
        self.done = false;
        self.next_block = match self.table.reference_block(name.as_bytes())? {
            Some(offset) => offset,
            None => {
                self.done = true;
                return Ok(());
            }
        };
        while !self.done {
            self.load_next_block()?;
            if self.done {
                return Ok(());
            }
            let (block, position) = self.block.as_mut().expect("a block was loaded");
            let data = self.table.block_data(self.block_offset)?;
            block.seek(data, position, &mut self.key, name.as_bytes())?;
            while !block.is_done(*position) {
                let previous_position = *position;
                self.seek_key.clone_from(&self.key);
                let header = self.table.header;
                block.next(data, position, &mut self.key, |input, value_type| {
                    decode_reference(input, value_type, header)
                })?;
                if self.key.as_slice() >= name.as_bytes() {
                    *position = previous_position;
                    std::mem::swap(&mut self.key, &mut self.seek_key);
                    return Ok(());
                }
            }
            self.block = None;
            if self.table.reference_index_start().is_some() {
                self.done = true;
            }
        }
        Ok(())
    }

    /// Read the next reference, borrowing this cursor's key buffer.
    ///
    /// Returns `None` at the end. After an error, subsequent calls return `None`.
    pub fn next_record(&mut self) -> Result<Option<ReferenceRef<'_>>> {
        let decoded = match self.advance() {
            Ok(decoded) => decoded,
            Err(err) => {
                self.done = true;
                return Err(err);
            }
        };
        Ok(decoded.map(|(update_index, value)| ReferenceRef {
            name: self.key.as_bstr(),
            update_index,
            value,
        }))
    }

    fn advance(&mut self) -> Result<Option<(u64, ValueRef<'a>)>> {
        while !self.done {
            if self.block.is_none() {
                self.load_next_block()?;
                if self.done {
                    return Ok(None);
                }
            }
            let (block, position) = self.block.as_mut().expect("a block was loaded");
            let data = self.table.block_data(self.block_offset)?;
            let header = self.table.header;
            let decoded = block.next(data, position, &mut self.key, |input, value_type| {
                decode_reference(input, value_type, header)
            })?;
            if decoded.is_some() {
                return Ok(decoded);
            }
            self.block = None;
        }
        Ok(None)
    }

    fn load_next_block(&mut self) -> Result<()> {
        if self.next_block == self.table.body_end() {
            self.done = true;
            return Ok(());
        }
        let data = self.table.block_data(self.next_block)?;
        let header_skip = self.table.header_skip(self.next_block);
        match data.get(header_skip) {
            Some(b'r') => {}
            Some(b'i')
                if self
                    .table
                    .reference_index_start()
                    .is_some_and(|root| self.next_block <= root) =>
            {
                self.done = true;
                return Ok(());
            }
            Some(b'o') if self.table.objects_start() == Some(self.next_block) => {
                self.done = true;
                return Ok(());
            }
            Some(b'g') if self.table.logs_start() == Some(self.next_block) => {
                self.done = true;
                return Ok(());
            }
            _ => bail!(corruption("Invalid block after reference records")),
        }
        let block = block::Block::from_bytes(data, header_skip)?;
        if self.table.header.block_size != 0 && block.size > self.table.header.block_size as usize {
            bail!(corruption("Reference block exceeds the table's block size"));
        }
        self.block_offset = self.next_block;
        self.next_block = self.table.next_uncompressed_block(self.block_offset, block.size)?;
        self.block = Some((block, block.start()));
        Ok(())
    }
}

impl Table<'_> {
    fn reference_block(&self, want: &[u8]) -> Result<Option<usize>> {
        let Some(mut offset) = self.reference_index_start() else {
            return Ok((self.first_block_kind() == Some(b'r')).then_some(0));
        };
        let root_end = self.reference_index_end();
        let mut root = true;
        loop {
            let end = if root { root_end } else { self.body_end() };
            let Some(next) = self.index_entry(offset, end, want)? else {
                return Ok(None);
            };
            if next >= offset {
                bail!(corruption("A reference index must point to an earlier block"));
            }
            let data = self.block_data(next)?;
            match data.get(self.header_skip(next)) {
                Some(b'r') => return Ok(Some(next)),
                Some(b'i') => {
                    offset = next;
                    root = false;
                }
                _ => bail!(corruption("A reference index points to an unexpected block")),
            }
        }
    }

    fn index_entry(&self, mut offset: usize, end: usize, want: &[u8]) -> Result<Option<usize>> {
        let mut key = Vec::new();
        while offset < end {
            let data = self.block_data(offset)?;
            let block = block::Block::from_bytes(data, self.header_skip(offset))?;
            if block.kind(data) != b'i' {
                bail!(corruption("Reference index contains a non-index block"));
            }
            let mut position = block.start();
            block.seek(data, &mut position, &mut key, want)?;
            while let Some(index_offset) = block.next(data, &mut position, &mut key, |input, value_type| {
                if value_type != 0 {
                    bail!(corruption("Index record has a reserved value type"));
                }
                let offset = decode::varint(input)?;
                decode::size(offset)
            })? {
                if key.as_slice() >= want {
                    return Ok(Some(index_offset));
                }
            }
            offset = self.next_uncompressed_block(offset, block.size)?;
        }
        Ok(None)
    }
}

fn decode_reference<'a>(input: &mut &'a [u8], value_type: u8, header: crate::Header) -> Result<(u64, ValueRef<'a>)> {
    let update_index = header
        .min_update_index
        .checked_add(decode::varint(input)?)
        .filter(|index| *index <= header.max_update_index)
        .ok_or_raise(|| corruption("Reference update index is outside the table's bounds"))?;
    let value = match value_type {
        0 => ValueRef::Deletion,
        1 | 2 => ValueRef::Object {
            object_id: decode::object_id(input, header.object_hash)?,
            peeled_id: if value_type == 2 {
                Some(decode::object_id(input, header.object_hash)?)
            } else {
                None
            },
        },
        3 => ValueRef::Symbolic(decode::string(input)?.as_bstr()),
        _ => bail!("Unsupported reference value type {value_type}".corrupted()),
    };
    Ok((update_index, value))
}
