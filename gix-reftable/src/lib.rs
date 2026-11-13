//! Read Git's reftable reference storage format from caller-owned bytes.
//!
//! [`Table::from_bytes()`] validates the table envelope before its offsets are used.
//! Repository configuration, reference policy, and stack management are separate
//! concerns and are not handled here.
#![deny(missing_docs, unsafe_code)]

/// The on-disk reftable format version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    /// The original format, using SHA-1 object IDs.
    V1,
    /// The format with an explicit object-hash identifier.
    V2,
}

/// Metadata shared by the header and footer of a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// The format version.
    pub version: Version,
    /// The kind of object IDs stored in this table.
    pub object_hash: gix_hash::Kind,
    /// The alignment of reference and object blocks, or zero for unaligned blocks.
    pub block_size: u32,
    /// The lowest update index described by this table.
    pub min_update_index: u64,
    /// The highest update index described by this table.
    pub max_update_index: u64,
}

/// An immutable view of a reftable backed by caller-owned bytes.
#[derive(Debug, Clone, Copy)]
pub struct Table<'a> {
    data: &'a [u8],
    header: Header,
    footer: table::Footer,
}

mod table;
