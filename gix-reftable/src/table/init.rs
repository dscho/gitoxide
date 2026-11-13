use gix_error::{ErrorExt, Result, ResultExt, bail, corruption, message, unsupported};

use super::Footer;
use crate::{Header, Table, Version};

/// Initialization
impl<'a> Table<'a> {
    /// Validate a table's header, footer, checksum, and section positions.
    ///
    /// The table borrows `data` without allocating a copy. Record blocks are
    /// validated when accessed, not as part of envelope validation.
    pub fn from_bytes(data: &'a [u8]) -> Result<Self> {
        if !data.starts_with(b"REFT") {
            bail!(corruption("Missing reftable signature"));
        }
        let version = match data.get(4) {
            Some(1) => Version::V1,
            Some(2) => Version::V2,
            Some(version) => bail!("Unsupported reftable version {version}".unsupported()),
            None => bail!(corruption("Missing reftable version")),
        };
        let header_len = version.header_len();
        let footer_len = version.footer_len();
        if data.len() < header_len + footer_len {
            bail!(corruption("Table is too short to contain its header and footer"));
        }
        let footer = &data[data.len() - footer_len..];
        if footer[..header_len] != data[..header_len] {
            bail!(corruption("Reftable header and footer do not agree"));
        }
        let checksum = u32::from_be_bytes(
            footer[footer_len - 4..]
                .try_into()
                .expect("the validated footer has a four-byte checksum"),
        );
        if crc32fast::hash(&footer[..footer_len - 4]) != checksum {
            bail!(corruption("Reftable footer checksum mismatch"));
        }

        let hash_id = match version {
            Version::V1 => 1,
            Version::V2 => match &data[24..28] {
                b"sha1" => 1,
                b"s256" => 2,
                _ => bail!(unsupported("Unrecognized reftable object-hash identifier")),
            },
        };
        let object_hash = gix_hash::Kind::try_from(hash_id)
            .map_err(|_| unsupported("This build does not support the table's object-hash kind").raise())?;
        let positions = &footer[header_len..footer_len - 4];
        let objects = u64_at(positions, 8);
        let out = Table {
            data,
            header: Header {
                version,
                object_hash,
                block_size: u32::from_be_bytes([0, data[5], data[6], data[7]]),
                min_update_index: u64_at(data, 8),
                max_update_index: u64_at(data, 16),
            },
            footer: Footer {
                ref_index: offset(u64_at(positions, 0))?,
                objects: offset(objects >> 5)?,
                object_id_len: (objects & 31) as usize,
                object_index: offset(u64_at(positions, 16))?,
                logs: offset(u64_at(positions, 24))?,
                log_index: offset(u64_at(positions, 32))?,
            },
        };
        out.validate_sections()?;
        Ok(out)
    }

    fn validate_sections(&self) -> Result<()> {
        if self.header.min_update_index > self.header.max_update_index {
            bail!(corruption("Reftable update-index bounds are reversed"));
        }
        let header_len = self.header.version.header_len();
        let footer_start = self.data.len() - self.header.version.footer_len();
        let first_kind = (footer_start > header_len).then(|| self.data[header_len]);
        if !matches!(first_kind, None | Some(b'r' | b'g')) {
            bail!(corruption("The first table block must contain references or logs"));
        }
        let mut previous = 0;
        for (position, expected) in [
            (self.footer.ref_index, b'i'),
            (self.footer.objects, b'o'),
            (self.footer.object_index, b'i'),
            (self.footer.logs, b'g'),
            (self.footer.log_index, b'i'),
        ] {
            if position == 0 {
                continue;
            }
            if position < header_len || position >= footer_start {
                bail!("Section position {position} is outside the table body".corrupted());
            }
            if position <= previous {
                bail!(corruption("Reftable sections are not in file order"));
            }
            if self.data[position] != expected {
                bail!("Section at {position} has an unexpected block type".corrupted());
            }
            previous = position;
        }
        if first_kind != Some(b'r')
            && (self.footer.ref_index != 0 || self.footer.objects != 0 || self.footer.object_index != 0)
        {
            bail!(corruption("Reference or object indexes require reference blocks"));
        }
        if self.footer.objects == 0 {
            if self.footer.object_id_len != 0 || self.footer.object_index != 0 {
                bail!(corruption("Absent object blocks require zero object metadata"));
            }
        } else if !(2..=self.header.object_hash.len_in_bytes()).contains(&self.footer.object_id_len) {
            bail!(corruption("Invalid object abbreviation length"));
        }
        if self.footer.log_index != 0 && self.footer.logs == 0 && first_kind != Some(b'g') {
            bail!(corruption("A log index requires log blocks"));
        }
        if first_kind.is_none() && previous != 0 {
            bail!(corruption("An empty table cannot contain section positions"));
        }
        Ok(())
    }
}

fn u64_at(data: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(
        data[offset..offset + 8]
            .try_into()
            .expect("a field in the validated header or footer"),
    )
}

fn offset(value: u64) -> Result<usize> {
    usize::try_from(value).or_raise(|| message!("Table position {value} does not fit this address space").corrupted())
}
