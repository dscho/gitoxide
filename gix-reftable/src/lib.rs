//! Read and write Git's reftable reference storage format.
//!
//! [`Table::from_bytes()`] borrows an encoded table, and [`write::Writer`] streams
//! standalone reference tables. [`stack::Stack`] loads immutable snapshots from
//! `tables.list`. Repository configuration and reference policy remain separate.
#![deny(missing_docs, unsafe_code)]

/// Byte-string types used for reference names and record data.
pub use bstr;

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

/// A decoded reference record borrowing its cursor's key buffer and table data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferenceRef<'a> {
    /// The full reference name, without applying repository namespace policy.
    pub name: &'a bstr::BStr,
    /// The transaction index that last changed this reference.
    pub update_index: u64,
    /// The stored value, including deletion records.
    pub value: reference::ValueRef<'a>,
}

/// A decoded reflog record borrowing its cursor's key and inflated block buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogRef<'a> {
    /// The complete reference name.
    pub name: &'a bstr::BStr,
    /// The transaction index, with newer entries sorting before older entries.
    pub update_index: u64,
    /// The reflog data, or `None` for a tombstone.
    pub data: Option<log::DataRef<'a>>,
}

/// A decoded object-index record borrowing its cursor's buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectRef<'a> {
    /// The abbreviated binary object ID.
    pub hash_prefix: &'a [u8],
    /// Absolute positions of candidate reference blocks.
    ///
    /// An empty slice means references must be scanned instead.
    pub block_offsets: &'a [usize],
}

/// Reflog records and their reusable read cursor.
pub mod log;
/// Reverse object-index records and their reusable read cursor.
pub mod object;
/// Reference records and their reusable read cursor.
pub mod reference;
/// Immutable table stacks and their merged views.
pub mod stack;
/// Native encoding of reftable records.
pub mod write;

mod block;
mod decode;
mod table;
