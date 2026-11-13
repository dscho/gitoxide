use std::path::{Path, PathBuf};

use bstr::BString;
use gix_error::{Result, ResultExt, bail, message};

use crate::Table;

/// An immutable collection of tables in oldest-to-newest order.
pub struct Snapshot {
    object_hash: gix_hash::Kind,
    tables: Vec<TableData>,
}

/// A directory-backed reftable stack.
pub struct Stack {
    directory: PathBuf,
    object_hash: gix_hash::Kind,
    options: Options,
}

/// Options controlling stack snapshot loading.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Maximum attempts to recover when concurrent publication removes a listed table.
    pub max_reload_attempts: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options { max_reload_attempts: 3 }
    }
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

impl Stack {
    /// Create a stack handle at `directory` for tables using `object_hash`.
    pub fn at(directory: impl Into<PathBuf>, object_hash: gix_hash::Kind) -> Self {
        Stack::at_opts(directory, object_hash, Options::default())
    }

    /// Create a stack handle with explicit loading `options`.
    pub fn at_opts(directory: impl Into<PathBuf>, object_hash: gix_hash::Kind, options: Options) -> Self {
        Stack {
            directory: directory.into(),
            object_hash,
            options,
        }
    }

    /// Return the directory containing `tables.list` and immutable table files.
    pub fn directory(&self) -> &Path {
        &self.directory
    }
}

/// Merged reflog views over immutable snapshots.
pub mod log;
/// Merged reference views over immutable snapshots.
pub mod reference;

mod init;
