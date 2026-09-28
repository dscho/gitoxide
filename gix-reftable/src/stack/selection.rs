use std::ops::Range;

use gix_error::{OptionExt, Result, ResultExt, bail, message, validation};

use super::Snapshot;

impl Snapshot {
    /// Select a contiguous range whose compaction moves the stack toward a
    /// geometric size progression, or `None` when no compaction is needed.
    ///
    /// `factor` must be at least two. Selection never changes the stack.
    pub fn geometric_range(&self, factor: u8) -> Result<Option<Range<usize>>> {
        let tables = self.parsed_tables()?;
        let mut sizes = Vec::new();
        sizes
            .try_reserve_exact(tables.len())
            .or_raise(|| message("Could not allocate reftable sizes for compaction"))?;
        for table in tables {
            let overhead = table.header().version.header_len() - 1;
            let size = table.as_bytes().len() - overhead;
            sizes.push(u64::try_from(size).or_raise(|| validation("Reftable size does not fit in u64"))?);
        }
        select_range(&sizes, factor)
    }
}

fn select_range(sizes: &[u64], factor: u8) -> Result<Option<Range<usize>>> {
    if factor < 2 {
        bail!(validation("Geometric compaction requires a factor of at least two"));
    }
    let factor = u128::from(factor);
    let Some(end) = (1..sizes.len())
        .rev()
        .find(|&i| u128::from(sizes[i - 1]) < u128::from(sizes[i]) * factor)
        .map(|i| i + 1)
    else {
        return Ok(None);
    };
    let mut start = end - 1;
    let mut bytes = u128::from(sizes[start]);
    for index in (0..start).rev() {
        let current = bytes;
        bytes = bytes
            .checked_add(u128::from(sizes[index]))
            .ok_or_raise(|| validation("Reftable sizes overflow while selecting compaction"))?;
        if u128::from(sizes[index]) < current.saturating_mul(factor) {
            start = index;
        }
    }
    Ok(Some(start..end))
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    #[test]
    fn geometric_segments_include_only_the_range_that_needs_merging() -> gix_error::TestResult {
        let cases: &[(&[u64], Option<Range<usize>>)] = &[
            (&[], None),
            (&[64], None),
            (&[64, 32, 16, 8, 4, 2, 1], None),
            (&[64, 32, 16, 8, 4, 3, 1], Some(0..6)),
            (&[128, 32, 16, 8, 4, 3, 1], Some(1..6)),
            (&[10_000, 3000, 1000, 128, 4, 3, 1], Some(4..6)),
            (&[1, 1, 1, 1], Some(0..4)),
            (&[u64::MAX, u64::MAX], Some(0..2)),
        ];
        for (sizes, expected) in cases {
            assert_eq!(
                super::select_range(sizes, 2)?,
                *expected,
                "unexpected range for {sizes:?}"
            );
        }
        assert!(
            super::select_range(&[1, 1], 1).is_err(),
            "factor 1 cannot bound stack growth"
        );
        Ok(())
    }
}
