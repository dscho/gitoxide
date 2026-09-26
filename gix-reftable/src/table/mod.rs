use crate::{Header, Table, Version, block, decode};
use gix_error::{OptionExt, Result, bail, corruption, message};

#[derive(Debug, Clone, Copy)]
pub(crate) struct Footer {
    ref_index: usize,
    objects: usize,
    object_id_len: usize,
    object_index: usize,
    logs: usize,
    log_index: usize,
}

impl Version {
    pub(crate) const fn header_len(self) -> usize {
        match self {
            Version::V1 => 24,
            Version::V2 => 28,
        }
    }

    pub(crate) const fn footer_len(self) -> usize {
        self.header_len() + 44
    }
}

/// Access
impl<'a> Table<'a> {
    /// Return the validated table metadata.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Return the original encoded table.
    pub fn as_bytes(&self) -> &'a [u8] {
        self.data
    }

    pub(crate) fn body_end(&self) -> usize {
        self.data.len() - self.header.version.footer_len()
    }

    pub(crate) fn logs_start(&self) -> Option<usize> {
        if self.footer.logs != 0 {
            Some(self.footer.logs)
        } else if self.first_block_kind() == Some(b'g') {
            Some(0)
        } else {
            None
        }
    }

    pub(crate) fn reference_index_start(&self) -> Option<usize> {
        (self.footer.ref_index != 0).then_some(self.footer.ref_index)
    }

    pub(crate) fn reference_index_end(&self) -> usize {
        [self.footer.objects, self.footer.logs, self.body_end()]
            .into_iter()
            .filter(|offset| *offset != 0 && *offset > self.footer.ref_index)
            .min()
            .expect("the footer always follows the reference index")
    }

    pub(crate) fn objects_start(&self) -> Option<usize> {
        (self.footer.objects != 0).then_some(self.footer.objects)
    }

    pub(crate) fn objects_end(&self) -> usize {
        [self.footer.logs, self.body_end()]
            .into_iter()
            .filter(|offset| *offset != 0 && *offset > self.footer.objects)
            .min()
            .expect("the logs or footer always follow the object section")
    }

    pub(crate) fn object_index_start(&self) -> Option<usize> {
        (self.footer.object_index != 0).then_some(self.footer.object_index)
    }

    pub(crate) fn object_index_end(&self) -> usize {
        [self.footer.logs, self.body_end()]
            .into_iter()
            .filter(|offset| *offset != 0 && *offset > self.footer.object_index)
            .min()
            .expect("the footer always follows the object index")
    }

    pub(crate) fn object_id_len(&self) -> usize {
        self.footer.object_id_len
    }

    pub(crate) fn logs_end(&self) -> usize {
        self.body_end()
    }

    pub(crate) fn log_index_start(&self) -> Option<usize> {
        (self.footer.log_index != 0).then_some(self.footer.log_index)
    }

    pub(crate) fn header_skip(&self, block_offset: usize) -> usize {
        if block_offset == 0 {
            self.header.version.header_len()
        } else {
            0
        }
    }

    pub(crate) fn first_block_kind(&self) -> Option<u8> {
        (self.body_end() > self.header.version.header_len()).then(|| self.data[self.header.version.header_len()])
    }

    pub(crate) fn block_data(&self, offset: usize) -> Result<&'a [u8]> {
        self.data
            .get(offset..self.body_end())
            .ok_or_raise(|| message!("Block position {offset} is outside the table body").corrupted())
    }

    pub(crate) fn next_uncompressed_block(&self, offset: usize, size: usize) -> Result<usize> {
        let end = offset
            .checked_add(size)
            .filter(|end| *end <= self.body_end())
            .ok_or_raise(|| corruption("Block extends beyond the table body"))?;
        let alignment = self.header.block_size as usize;
        if alignment == 0 || size >= alignment || end == self.body_end() || self.data[end] != 0 {
            return Ok(end);
        }
        let aligned_end = offset
            .checked_add(alignment)
            .filter(|end| *end <= self.body_end())
            .ok_or_raise(|| corruption("Block padding extends beyond the table body"))?;
        if self.data[end..aligned_end].iter().any(|byte| *byte != 0) {
            bail!(corruption("Block padding contains nonzero bytes"));
        }
        Ok(aligned_end)
    }

    pub(crate) fn indexed_block(
        &self,
        mut offset: usize,
        root_end: usize,
        want: &[u8],
        expected_leaf: u8,
    ) -> Result<Option<usize>> {
        let mut root = true;
        loop {
            let mut index_offset = offset;
            let pointed_to = loop {
                let (found, next) = self.index_entry(index_offset, want)?;
                if let Some(found) = found {
                    break found;
                }
                if !root || next >= root_end {
                    return Ok(None);
                }
                index_offset = next;
            };
            if pointed_to >= offset {
                bail!(corruption("An index must point to an earlier block"));
            }
            let data = self.block_data(pointed_to)?;
            match data.get(self.header_skip(pointed_to)) {
                Some(kind) if *kind == expected_leaf => return Ok(Some(pointed_to)),
                Some(b'i') => {
                    offset = pointed_to;
                    root = false;
                }
                _ => bail!(corruption("An index points to an unexpected block")),
            }
        }
    }

    fn index_entry(&self, offset: usize, want: &[u8]) -> Result<(Option<usize>, usize)> {
        let data = self.block_data(offset)?;
        let block = block::Block::from_bytes(data, self.header_skip(offset))?;
        if block.kind(data) != b'i' {
            bail!(corruption("Index section contains a non-index block"));
        }
        let mut position = block.start();
        let mut key = Vec::new();
        block.seek(data, &mut position, &mut key, want)?;
        let mut found = None;
        while let Some(index_offset) = block.next(data, &mut position, &mut key, |input, value_type| {
            if value_type != 0 {
                bail!(corruption("Index record has a reserved value type"));
            }
            let offset = decode::varint(input)?;
            decode::size(offset)
        })? {
            if key.as_slice() >= want {
                found = Some(index_offset);
                break;
            }
        }
        Ok((found, self.next_uncompressed_block(offset, block.size)?))
    }
}

mod init;
