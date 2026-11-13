use crate::{Header, Table, Version};
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

    pub(crate) fn objects_start(&self) -> Option<usize> {
        (self.footer.objects != 0).then_some(self.footer.objects)
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
}

mod init;
