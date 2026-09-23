use gix_error::{OptionExt, Result, ResultExt, bail, corruption, message};

use crate::{ObjectRef, Table, block, decode};

/// A reverse object-index cursor with reusable key and offset buffers.
pub struct Iter<'a> {
    table: Table<'a>,
    next_block: usize,
    end: usize,
    block: Option<(block::Block, block::Position)>,
    block_offset: usize,
    key: Vec<u8>,
    offsets: Vec<usize>,
    done: bool,
}

impl<'a> Table<'a> {
    /// Iterate optional reverse object-index records in abbreviated hash order.
    pub fn objects(&self) -> Iter<'a> {
        let start = self.objects_start();
        Iter {
            table: *self,
            next_block: start.unwrap_or_else(|| self.objects_end()),
            end: self.objects_end(),
            block: None,
            block_offset: 0,
            key: Vec::new(),
            offsets: Vec::new(),
            done: start.is_none(),
        }
    }
}

impl Iter<'_> {
    /// Read the next reverse-index record, borrowing this cursor's buffers.
    ///
    /// Returns `None` at the end. After an error, subsequent calls return `None`.
    pub fn next_record(&mut self) -> Result<Option<ObjectRef<'_>>> {
        if self.done {
            return Ok(None);
        }
        let decoded = match self.advance() {
            Ok(decoded) => decoded,
            Err(err) => {
                self.done = true;
                return Err(err);
            }
        };
        Ok(decoded.map(|()| ObjectRef {
            hash_prefix: &self.key,
            block_offsets: &self.offsets,
        }))
    }

    fn advance(&mut self) -> Result<Option<()>> {
        while !self.done {
            if self.block.is_none() {
                if self.next_block == self.end {
                    self.done = true;
                    return Ok(None);
                }
                let data = self.table.block_data(self.next_block)?;
                match data.first() {
                    Some(b'o') => {}
                    Some(b'i')
                        if self
                            .table
                            .object_index_start()
                            .is_some_and(|root| self.next_block <= root) =>
                    {
                        self.done = true;
                        return Ok(None);
                    }
                    Some(b'g') if self.table.logs_start() == Some(self.next_block) => {
                        self.done = true;
                        return Ok(None);
                    }
                    _ => bail!(corruption("Unexpected block in the object section")),
                }
                let block = block::Block::from_bytes(data, 0)?;
                if self.table.header.block_size != 0 && block.size > self.table.header.block_size as usize {
                    bail!(corruption("Object block exceeds the table's block size"));
                }
                self.block_offset = self.next_block;
                self.next_block = self.table.next_uncompressed_block(self.block_offset, block.size)?;
                self.block = Some((block, block.start()));
            }
            let (block, position) = self.block.as_mut().expect("an object block was loaded");
            let data = self.table.block_data(self.block_offset)?;
            self.offsets.clear();
            let offsets = &mut self.offsets;
            let body_end = self.table.body_end();
            let decoded = block.next(data, position, &mut self.key, |input, count| {
                decode_offsets(input, count, offsets, body_end)
            })?;
            if decoded.is_some() {
                if self.key.len() != self.table.object_id_len() {
                    bail!(corruption("Object abbreviation has the wrong length"));
                }
                return Ok(decoded);
            }
            self.block = None;
        }
        Ok(None)
    }
}

fn decode_offsets(input: &mut &[u8], inline_count: u8, out: &mut Vec<usize>, body_end: usize) -> Result<()> {
    let count = if inline_count == 0 {
        decode::size(decode::varint(input)?)?
    } else {
        usize::from(inline_count)
    };
    if count == 0 {
        return Ok(());
    }
    if count > input.len() {
        bail!(corruption("Object record declares more offsets than can be encoded"));
    }
    out.try_reserve(count)
        .or_raise(|| message("Could not allocate the object-offset buffer"))?;
    let first = decode::size(decode::varint(input)?)?;
    if first >= body_end {
        bail!(corruption("Object record points outside the table body"));
    }
    out.push(first);
    for _ in 1..count {
        let delta = decode::size(decode::varint(input)?)?;
        let offset = out
            .last()
            .expect("the first offset was added")
            .checked_add(delta)
            .filter(|offset| *offset < body_end)
            .ok_or_raise(|| corruption("Object block offsets overflow or leave the table body"))?;
        if offset <= *out.last().expect("the first offset was added") {
            bail!(corruption("Object block offsets are not strictly increasing"));
        }
        out.push(offset);
    }
    Ok(())
}
