use gix_error::{OptionExt, Result, ResultExt, bail, corruption, message};

use crate::decode;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Block {
    pub size: usize,
    records_start: usize,
    records_end: usize,
    restart_count: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Position {
    offset: usize,
    restart: usize,
}

impl Block {
    pub fn from_bytes(data: &[u8], header_skip: usize) -> Result<Self> {
        let header = data
            .get(header_skip..header_skip + 4)
            .ok_or_raise(|| corruption("Truncated block header"))?;
        if !matches!(header[0], b'r' | b'i' | b'o' | b'g') {
            bail!(corruption("Unrecognized reftable block type"));
        }
        let size = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
        let records_start = header_skip + 4;
        if size > data.len() {
            bail!(corruption("Block length is outside its input buffer"));
        }
        if size < records_start + 2 {
            bail!(corruption("Block is too short to contain records and restart metadata"));
        }
        let restart_count = usize::from(u16::from_be_bytes([data[size - 2], data[size - 1]]));
        if restart_count == 0 {
            bail!(corruption("A record block must contain a restart point"));
        }
        let records_end = (size - 2)
            .checked_sub(3 * restart_count)
            .filter(|end| *end > records_start)
            .ok_or_raise(|| corruption("Restart table overlaps the block header or records are empty"))?;
        let out = Block {
            size,
            records_start,
            records_end,
            restart_count,
        };
        let mut previous_offset = None;
        let mut previous_key: Option<&[u8]> = None;
        for index in 0..restart_count {
            let offset = out.restart_offset(data, index);
            if offset < records_start || offset >= records_end {
                bail!(corruption("Restart point is outside the record data"));
            }
            if index == 0 && offset != records_start {
                bail!(corruption("The first record must be a restart point"));
            }
            if previous_offset.is_some_and(|previous| previous >= offset) {
                bail!(corruption("Restart points are not strictly increasing"));
            }
            let key = out.restart_key(data, index)?;
            if previous_key.is_some_and(|previous| previous >= key) {
                bail!(corruption("Restart keys are not strictly increasing"));
            }
            previous_offset = Some(offset);
            previous_key = Some(key);
        }
        Ok(out)
    }

    pub fn start(self) -> Position {
        Position {
            offset: self.records_start,
            restart: 0,
        }
    }

    pub fn is_done(self, position: Position) -> bool {
        position.offset == self.records_end
    }

    fn restart_offset(self, data: &[u8], index: usize) -> usize {
        let pos = self.records_end + 3 * index;
        u32::from_be_bytes([0, data[pos], data[pos + 1], data[pos + 2]]) as usize
    }

    fn restart_key(self, data: &[u8], index: usize) -> Result<&[u8]> {
        let end = if index + 1 < self.restart_count {
            self.restart_offset(data, index + 1)
        } else {
            self.records_end
        };
        if end > self.records_end {
            bail!(corruption("Restart point is outside the record data"));
        }
        // Stop at the next restart so a malformed key length cannot reach into
        // another restart record.
        let mut input = data
            .get(self.restart_offset(data, index)..end)
            .ok_or_raise(|| corruption("Restart points are not in file order"))?;
        if decode::varint(&mut input)? != 0 {
            bail!(corruption("A restart record cannot have a compressed key"));
        }
        let len = decode::size(decode::varint(&mut input)? >> 3)?;
        let key = decode::take(&mut input, len)?;
        if key.is_empty() {
            bail!(corruption("A record key must not be empty"));
        }
        Ok(key)
    }

    pub fn next<'a, T>(
        self,
        data: &'a [u8],
        position: &mut Position,
        key: &mut Vec<u8>,
        value: impl FnOnce(&mut &'a [u8], u8) -> Result<T>,
    ) -> Result<Option<T>> {
        if position.offset == self.records_end {
            return Ok(None);
        }
        let mut input = &data[position.offset..self.records_end];
        let is_restart =
            position.restart < self.restart_count && self.restart_offset(data, position.restart) == position.offset;
        let prefix = decode::size(decode::varint(&mut input)?)?;
        if prefix > key.len() || (is_restart && prefix != 0) {
            bail!(corruption("Invalid record-key prefix length"));
        }
        let suffix_and_type = decode::varint(&mut input)?;
        let suffix = decode::take(&mut input, decode::size(suffix_and_type >> 3)?)?;
        if prefix == 0 && suffix.is_empty() {
            bail!(corruption("A record key must not be empty"));
        }
        if !key.is_empty() && suffix <= &key[prefix..] {
            bail!(corruption("Record keys are not strictly increasing"));
        }
        key.truncate(prefix);
        key.try_reserve(suffix.len())
            .or_raise(|| message("Could not allocate the record-key buffer"))?;
        key.extend_from_slice(suffix);
        let value = value(&mut input, (suffix_and_type & 7) as u8)
            .or_raise(|| message!("Could not decode block record at {}", position.offset))?;
        let next = self.records_end - input.len();
        let next_restart = position.restart + usize::from(is_restart);
        if next_restart < self.restart_count && self.restart_offset(data, next_restart) < next {
            bail!(corruption("A restart point does not refer to a record boundary"));
        }
        position.offset = next;
        position.restart = next_restart;
        Ok(Some(value))
    }
}
