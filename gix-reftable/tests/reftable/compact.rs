mod snapshot {
    use std::ops::Range;

    use bstr::{BStr, BString, ByteSlice};
    use gix_testtools::TestResult;

    use gix_reftable::{
        Header, LogRef, ReferenceRef, Table,
        log::DataRef,
        reference::ValueRef,
        stack::{CompactionOptions, Snapshot, log, reference},
        write::Writer,
    };

    fn table(
        index: u64,
        references: &[(&[u8], Option<u8>)],
        logs: &[(&[u8], u64, bool)],
    ) -> TestResult<(BString, Vec<u8>)> {
        let hash_len = gix_testtools::object_hash().len_in_bytes();
        let old_id = gix_testtools::object_hash().null();
        let new_id = gix_hash::ObjectId::try_from(vec![index as u8; hash_len].as_slice())?;
        let mut writer = Writer::new(
            Vec::new(),
            Header {
                block_size: 0,
                min_update_index: index,
                max_update_index: index,
                ..crate::write::header()
            },
        )?;
        for (name, value) in references {
            let bytes = value.map(|byte| vec![byte; hash_len]);
            let object_id = bytes.as_deref().map(gix_hash::oid::try_from_bytes).transpose()?;
            writer.push_reference(ReferenceRef {
                name: BStr::new(name),
                update_index: index,
                value: object_id.map_or(ValueRef::Deletion, |object_id| ValueRef::Object {
                    object_id,
                    peeled_id: None,
                }),
            })?;
        }
        for (name, update_index, updated) in logs {
            writer.push_log(LogRef {
                name: BStr::new(name),
                update_index: *update_index,
                data: updated.then_some(DataRef {
                    previous_object_id: old_id.as_ref(),
                    new_object_id: new_id.as_ref(),
                    committer: gix_actor::IdentityRef {
                        name: BStr::new(b"author"),
                        email: BStr::new(b"author@example.com"),
                    },
                    time_seconds: index,
                    timezone_minutes: 0,
                    message: BStr::new(b"update\n"),
                }),
            })?;
        }
        Ok((format!("table-{index}.ref").into(), writer.finish()?))
    }

    fn compact(snapshot: &Snapshot, range: Range<usize>) -> TestResult<(Vec<u8>, usize)> {
        Ok(snapshot.compact_into(range, Vec::new(), CompactionOptions { block_size: 0 })?)
    }

    fn references(snapshot: &Snapshot) -> TestResult<Vec<reference::Reference>> {
        let mut iter = snapshot.references()?;
        let mut out = Vec::new();
        while let Some(record) = iter.next_record()? {
            out.push(record);
        }
        Ok(out)
    }

    fn logs(snapshot: &Snapshot, name: &BStr) -> TestResult<Vec<log::Record>> {
        let mut iter = snapshot.logs(name)?;
        let mut out = Vec::new();
        while let Some(record) = iter.next_record()? {
            out.push(record);
        }
        Ok(out)
    }

    #[test]
    fn full_compaction_drops_masked_values_and_tombstones() -> TestResult {
        let old = table(
            1,
            &[(b"refs/a", Some(1)), (b"refs/b", Some(2))],
            &[(b"refs/a", 1, true)],
        )?;
        let new = table(
            2,
            &[(b"refs/a", None), (b"refs/c", Some(3))],
            &[(b"refs/a", 2, true), (b"refs/a", 1, false)],
        )?;
        let before = Snapshot::from_tables(gix_testtools::object_hash(), [old, new])?;
        let (data, count) = compact(&before, 0..2)?;
        assert_eq!(count, 3, "two references and the unmasked log remain");
        let table = Table::from_bytes(&data)?;
        let mut refs = table.references();
        assert_eq!(refs.next_record()?.expect("first reference").name, "refs/b");
        assert_eq!(refs.next_record()?.expect("second reference").name, "refs/c");
        assert!(refs.next_record()?.is_none(), "the deleted reference is absent");
        let mut written_logs = table.logs();
        assert_eq!(written_logs.next_record()?.expect("newer log").update_index, 2);
        assert!(
            written_logs.next_record()?.is_none(),
            "the masked old log does not return"
        );
        let after = Snapshot::from_tables(gix_testtools::object_hash(), [("merged".into(), data)])?;
        assert_eq!(
            references(&before)?,
            references(&after)?,
            "full compaction preserves visible refs"
        );
        assert_eq!(
            logs(&before, BStr::new(b"refs/a"))?,
            logs(&after, BStr::new(b"refs/a"))?,
            "full compaction preserves visible logs"
        );
        Ok(())
    }

    #[test]
    fn partial_compaction_retains_tombstones_against_lower_tables() -> TestResult {
        let old = table(1, &[(b"refs/a", Some(1))], &[(b"refs/a", 1, true)])?;
        let deleting = table(2, &[(b"refs/a", None)], &[(b"refs/a", 1, false)])?;
        let newest = table(3, &[(b"refs/c", Some(3))], &[(b"refs/c", 3, true)])?;
        let before = Snapshot::from_tables(gix_testtools::object_hash(), [old.clone(), deleting, newest])?;
        let (data, count) = compact(&before, 1..3)?;
        assert_eq!(count, 4, "both tombstones and new records remain");
        let table = Table::from_bytes(&data)?;
        assert_eq!(
            table.references().next_record()?.expect("reference tombstone").value,
            ValueRef::Deletion,
            "the deleted reference must still mask the lower table"
        );
        assert!(
            table.logs().next_record()?.expect("log tombstone").data.is_none(),
            "the deleted log must still mask the lower table"
        );
        let after = Snapshot::from_tables(gix_testtools::object_hash(), [old, ("merged".into(), data)])?;
        assert_eq!(
            references(&before)?,
            references(&after)?,
            "partial compaction preserves visible refs"
        );
        for name in [b"refs/a".as_slice(), b"refs/c".as_slice()] {
            assert_eq!(
                logs(&before, name.as_bstr())?,
                logs(&after, name.as_bstr())?,
                "partial compaction preserves each reflog"
            );
        }
        Ok(())
    }

    #[test]
    fn full_compaction_can_yield_an_empty_table() -> TestResult {
        let old = table(1, &[(b"refs/a", Some(1))], &[(b"refs/a", 1, true)])?;
        let deleting = table(2, &[(b"refs/a", None)], &[(b"refs/a", 1, false)])?;
        let before = Snapshot::from_tables(gix_testtools::object_hash(), [old, deleting])?;
        let (data, count) = compact(&before, 0..2)?;
        assert_eq!(count, 0, "nothing is visible after compaction");
        let table = Table::from_bytes(&data)?;
        assert!(table.references().next_record()?.is_none(), "no reference remains");
        assert!(table.logs().next_record()?.is_none(), "no reflog remains");
        Ok(())
    }

    #[test]
    fn invalid_ranges_are_rejected() -> TestResult {
        let only = table(1, &[(b"refs/a", Some(1))], &[])?;
        let snapshot = Snapshot::from_tables(gix_testtools::object_hash(), [only])?;
        for range in [0..0, 1..1, 0..2, 2..3] {
            assert!(
                compact(&snapshot, range.clone()).is_err(),
                "invalid range {range:?} must be rejected"
            );
        }
        Ok(())
    }
}
