use crate::{Header, Table, Version};

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
}

mod init;
