use gix_reftable::stack::{CompactionOptions, MaintenanceOptions, Stack};
use gix_testtools::bstr::ByteSlice;

#[test]
fn repeated_geometric_maintenance_bounds_stack_growth() -> crate::Result {
    let dir = gix_testtools::tempfile::TempDir::new()?;
    let stack = Stack::at(dir.path(), gix_testtools::object_hash());
    for index in 1..=32 {
        let name = format!("refs/heads/{index:02}");
        super::compact::append(&stack, name.as_bytes().as_bstr(), Some(index as u8), index)?;
        let mut compacted = 0;
        while let Some(snapshot) =
            stack.compact_if_needed(gix_lock::acquire::Fail::Immediately, MaintenanceOptions::default())?
        {
            assert!(
                snapshot.table_names().len() < index as usize,
                "compaction decreases the stack depth"
            );
            compacted += 1;
            assert!(compacted < index, "maintenance must converge");
        }
        let snapshot = stack.snapshot()?;
        assert!(
            snapshot.geometric_range(2)?.is_none(),
            "no violation remains after maintenance"
        );
        assert!(snapshot.table_names().len() <= 8, "32 appends must not leave 32 tables");
        let mut refs = snapshot.references()?;
        let mut count = 0;
        while refs.next_record()?.is_some() {
            count += 1;
        }
        assert_eq!(count, index, "maintenance preserves every reference");
    }
    let current = stack.snapshot()?;
    let cleaned = stack.cleanup_stale_tables(gix_lock::acquire::Fail::Immediately)?;
    assert!(cleaned.deferred.is_empty(), "inactive retired files are reclaimable");
    let listed = current.table_names().len();
    let present = std::fs::read_dir(dir.path())?
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "ref"))
        .count();
    assert_eq!(present, listed, "cleanup leaves only listed tables on disk");
    Ok(())
}

#[test]
fn invalid_maintenance_options_report_errors_even_without_tables() -> crate::Result {
    let dir = gix_testtools::tempfile::TempDir::new()?;
    let stack = Stack::at(dir.path(), gix_testtools::object_hash());
    for options in [
        MaintenanceOptions {
            factor: 1,
            ..MaintenanceOptions::default()
        },
        MaintenanceOptions {
            compaction: CompactionOptions { block_size: 1 << 24 },
            ..MaintenanceOptions::default()
        },
    ] {
        assert!(
            stack
                .compact_if_needed(gix_lock::acquire::Fail::Immediately, options)
                .is_err(),
            "invalid options do not look like a no-op: {options:?}"
        );
    }
    assert!(
        !dir.path().join("tables.list.lock").exists(),
        "failures release any manifest lock"
    );
    Ok(())
}
