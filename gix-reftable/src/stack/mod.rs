use bstr::BString;
use gix_error::{Result, ResultExt, bail, message};

use crate::Table;

/// An immutable collection of tables in oldest-to-newest order.
pub struct Snapshot {
    object_hash: gix_hash::Kind,
    tables: Vec<TableData>,
}

struct TableData {
    name: BString,
    data: Vec<u8>,
}

impl Snapshot {
    /// Validate and own tables listed from oldest to newest.
    pub fn from_tables(
        object_hash: gix_hash::Kind,
        tables: impl IntoIterator<Item = (BString, Vec<u8>)>,
    ) -> Result<Self> {
        let mut out = Vec::new();
        for (name, data) in tables {
            let table = Table::from_bytes(&data).or_raise(|| message!("Could not open reftable {name:?}"))?;
            if table.header().object_hash != object_hash {
                bail!("Reftable {name:?} uses a different object-hash format".validation());
            }
            out.push(TableData { name, data });
        }
        Ok(Snapshot {
            object_hash,
            tables: out,
        })
    }

    /// Return the object-hash kind shared by all tables.
    pub fn object_hash(&self) -> gix_hash::Kind {
        self.object_hash
    }

    /// Return table names in oldest-to-newest order.
    pub fn table_names(&self) -> impl ExactSizeIterator<Item = &bstr::BStr> {
        self.tables.iter().map(|table| table.name.as_ref())
    }

    fn parsed_tables(&self) -> Result<Vec<Table<'_>>> {
        self.tables
            .iter()
            .map(|table| {
                Table::from_bytes(&table.data)
                    .or_raise(|| message!("Could not reopen snapshotted reftable {:?}", table.name))
            })
            .collect()
    }
}

/// Merged reflog views over immutable snapshots.
pub mod log;
/// Merged reference views over immutable snapshots.
pub mod reference;
