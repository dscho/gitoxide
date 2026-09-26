use std::{
    collections::BTreeSet,
    io,
    path::{Component, Path},
};

use bstr::BString;
use gix_error::{ErrorExt, Result, ResultExt, bail, corruption, message, validation};

use crate::stack::Stack;

impl Stack {
    /// Load a coherent immutable snapshot of the current `tables.list`.
    ///
    /// If a listed table disappears while it is opened and a concurrent publisher
    /// changed the manifest, loading is retried up to the configured bound.
    pub fn snapshot(&self) -> Result<super::Snapshot> {
        self.snapshot_with(&mut |path: &Path| std::fs::read(path))
    }

    fn snapshot_with(&self, read: &mut impl FnMut(&Path) -> io::Result<Vec<u8>>) -> Result<super::Snapshot> {
        if self.options.max_reload_attempts == 0 {
            bail!(validation("Stack loading requires at least one reload attempt"));
        }
        let manifest_path = self.directory.join("tables.list");
        for _ in 0..self.options.max_reload_attempts {
            let names = read_manifest(&manifest_path, read)?;
            let mut tables = Vec::new();
            tables
                .try_reserve_exact(names.len())
                .or_raise(|| message("Could not allocate the stack table list"))?;
            let mut missing = None;
            for name in &names {
                let path = self.directory.join(bstr_to_path(name.as_ref())?);
                match read(&path) {
                    Ok(data) => tables.push((name.clone(), data)),
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {
                        missing = Some((name, err));
                        break;
                    }
                    Err(err) => return Err(err.and_raise(message!("Could not read reftable {:?}", name))),
                }
            }
            if let Some((name, err)) = missing {
                if read_manifest(&manifest_path, read)? != names {
                    continue;
                }
                return Err(err.and_raise(message!("Reftable {name:?} listed in tables.list is missing").corrupted()));
            }
            return super::Snapshot::from_tables(self.object_hash, tables)
                .or_raise(|| message!("Could not load stack at {}", self.directory.display()));
        }
        Err(message!(
            "Reftable manifest at {} changed during all {} reload attempts",
            manifest_path.display(),
            self.options.max_reload_attempts
        )
        .retryable_error())
    }
}

fn read_manifest(path: &Path, read: &mut impl FnMut(&Path) -> io::Result<Vec<u8>>) -> Result<Vec<BString>> {
    let data = match read(path) {
        Ok(data) => data,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.and_raise(message!("Could not read {}", path.display()))),
    };
    if data.is_empty() {
        return Ok(Vec::new());
    }
    if data.last() != Some(&b'\n') {
        bail!(corruption("tables.list is missing its final newline"));
    }
    let mut names = Vec::new();
    let mut seen = BTreeSet::new();
    for line in data[..data.len() - 1].split(|byte| *byte == b'\n') {
        if line.is_empty() {
            bail!(corruption("tables.list contains an empty table name"));
        }
        let path = bstr_to_path(line)?;
        if path.components().count() != 1 || !matches!(path.components().next(), Some(Component::Normal(_))) {
            bail!("Invalid reftable name {line:?} in tables.list".corrupted());
        }
        if !seen.insert(line) {
            bail!("Duplicate reftable name {line:?} in tables.list".corrupted());
        }
        names.push(BString::from(line));
    }
    Ok(names)
}

fn bstr_to_path(name: &[u8]) -> Result<&Path> {
    gix_path::from_byte_slice(name).or_raise(|| {
        message!(
            "Reftable name {:?} is not a valid path on this platform",
            BString::from(name)
        )
    })
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        path::{Path, PathBuf},
    };

    use super::super::{Options, Stack};
    use bstr::ByteSlice;

    fn empty_table() -> Vec<u8> {
        let mut header = b"REFT\x01".to_vec();
        header.extend_from_slice(&[0; 19]);
        let mut data = header.clone();
        data.extend_from_slice(&header);
        data.extend_from_slice(&[0; 40]);
        let checksum = crc32fast::hash(&data[24..]);
        data.extend_from_slice(&checksum.to_be_bytes());
        data
    }

    #[test]
    fn complete_generations_do_not_require_manifest_stability() -> gix_error::TestResult {
        let root = PathBuf::from("/stack");
        let mut manifest_reads = 0;
        let mut read = |path: &Path| -> io::Result<Vec<u8>> {
            match path.file_name().and_then(|name| name.to_str()) {
                Some("tables.list") => {
                    manifest_reads += 1;
                    Ok(if manifest_reads == 1 {
                        b"old.ref\n".to_vec()
                    } else {
                        b"new.ref\n".to_vec()
                    })
                }
                Some("old.ref" | "new.ref") => Ok(empty_table()),
                _ => unreachable!("known test path"),
            }
        };
        let snapshot = Stack::at(root, gix_hash::Kind::Sha1).snapshot_with(&mut read)?;
        assert_eq!(snapshot.table_names().collect::<Vec<_>>(), ["old.ref"]);
        assert_eq!(manifest_reads, 1, "fully loaded generations are coherent snapshots");
        Ok(())
    }

    #[test]
    fn changed_manifests_retry_missing_tables() -> gix_error::TestResult {
        let root = PathBuf::from("/stack");
        let mut manifest_reads = 0;
        let mut read = |path: &Path| -> io::Result<Vec<u8>> {
            match path.file_name().and_then(|name| name.to_str()) {
                Some("tables.list") => {
                    manifest_reads += 1;
                    Ok(if manifest_reads == 1 {
                        b"old.ref\n".to_vec()
                    } else {
                        b"new.ref\n".to_vec()
                    })
                }
                Some("old.ref") => Err(io::ErrorKind::NotFound.into()),
                Some("new.ref") => Ok(empty_table()),
                _ => unreachable!("known test path"),
            }
        };
        let snapshot = Stack::at(root, gix_hash::Kind::Sha1).snapshot_with(&mut read)?;
        assert_eq!(snapshot.table_names().collect::<Vec<_>>(), ["new.ref"]);
        assert_eq!(
            manifest_reads, 3,
            "only a missing table requires checking for a newer manifest"
        );
        Ok(())
    }

    #[test]
    fn stable_missing_tables_are_errors() {
        let root = PathBuf::from("/stack");
        let mut read = |path: &Path| -> io::Result<Vec<u8>> {
            match path.file_name().and_then(|name| name.to_str()) {
                Some("tables.list") => Ok(b"missing.ref\n".to_vec()),
                Some("missing.ref") => Err(io::ErrorKind::NotFound.into()),
                _ => unreachable!("known test path"),
            }
        };
        assert!(Stack::at(root, gix_hash::Kind::Sha1).snapshot_with(&mut read).is_err());
    }

    #[test]
    fn manifests_reject_paths_empty_names_and_duplicates() {
        for manifest in [
            b"../a.ref\n".as_slice(),
            b"/a.ref\n",
            b"a.ref",
            b"a.ref\nb.ref",
            b"a.ref\n\nb.ref\n",
            b"a.ref\na.ref\n",
        ] {
            let mut read = |path: &Path| -> io::Result<Vec<u8>> {
                assert!(
                    path.ends_with("tables.list"),
                    "invalid manifests must fail before opening a table"
                );
                Ok(manifest.to_vec())
            };
            assert!(
                Stack::at("/stack", gix_hash::Kind::Sha1)
                    .snapshot_with(&mut read)
                    .is_err()
            );
        }
    }

    #[test]
    fn reload_attempts_are_bounded() {
        let mut generation = 0;
        let mut read = |path: &Path| -> io::Result<Vec<u8>> {
            if path.ends_with("tables.list") {
                generation += 1;
                Ok(format!("{generation}.ref\n").into_bytes())
            } else {
                Err(io::ErrorKind::NotFound.into())
            }
        };
        assert!(
            Stack::at_opts("/stack", gix_hash::Kind::Sha1, Options { max_reload_attempts: 2 },)
                .snapshot_with(&mut read)
                .is_err_and(|err| err.is_retryable()),
            "manifest churn can be retried without changing the request"
        );
        assert_eq!(generation, 4, "two attempts read the manifest twice");
    }

    #[cfg(unix)]
    #[test]
    fn manifest_names_are_bytes_on_unix() -> gix_error::TestResult {
        use std::os::unix::ffi::OsStrExt;

        let table = empty_table();
        let mut read = |path: &Path| -> io::Result<Vec<u8>> {
            match path.file_name().map(std::ffi::OsStr::as_bytes) {
                Some(b"tables.list") => Ok(b"\xff.ref\n".to_vec()),
                Some(b"\xff.ref") => Ok(table.clone()),
                _ => unreachable!("known test path"),
            }
        };
        let snapshot = Stack::at("/stack", gix_hash::Kind::Sha1).snapshot_with(&mut read)?;
        assert_eq!(
            snapshot.table_names().next().expect("one table").as_bytes(),
            b"\xff.ref"
        );
        Ok(())
    }
}
