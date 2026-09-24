use bstr::{BStr, BString};
use gix_error::Result;

use crate::stack::Snapshot;

/// A visible merged reflog record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The complete reference name.
    pub name: BString,
    /// The update index.
    pub update_index: u64,
    /// The value before the update.
    pub previous_object_id: gix_hash::ObjectId,
    /// The value after the update.
    pub new_object_id: gix_hash::ObjectId,
    /// The committer identity.
    pub committer: gix_actor::Identity,
    /// Seconds since the Unix epoch.
    pub time_seconds: u64,
    /// The signed timezone offset in minutes.
    pub timezone_minutes: i16,
    /// The uninterpreted reflog message.
    pub message: BString,
}

struct Current {
    update_index: u64,
    data: Option<Record>,
}

struct SubIter<'a> {
    iter: crate::log::Iter<'a>,
    current: Option<Current>,
}

/// A merged cursor over one reference's visible reflog entries.
pub struct Iter<'a> {
    subiters: Vec<SubIter<'a>>,
    name: BString,
    /// The selected subiterator is advanced lazily so a later decode error
    /// cannot swallow a record that was already decoded successfully.
    pending: Option<usize>,
    done: bool,
}

impl Snapshot {
    /// Iterate visible reflog entries for `name`, newest update index first.
    pub fn logs(&self, name: &BStr) -> Result<Iter<'_>> {
        let tables = self.parsed_tables()?;
        let mut subiters = Vec::with_capacity(tables.len());
        for table in tables {
            let mut iter = table.logs();
            iter.seek(name, u64::MAX)?;
            let current = next_owned(&mut iter, name)?;
            subiters.push(SubIter { iter, current });
        }
        Ok(Iter {
            subiters,
            name: name.to_owned(),
            pending: None,
            done: false,
        })
    }
}

impl Iter<'_> {
    /// Return the next visible log entry, suppressing overwritten records and tombstones.
    pub fn next_record(&mut self) -> Result<Option<Record>> {
        let result = self.advance();
        if result.is_err() {
            self.done = true;
        }
        result
    }

    fn advance(&mut self) -> Result<Option<Record>> {
        while !self.done {
            if let Some(index) = self.pending.take() {
                let sub = &mut self.subiters[index];
                sub.current = next_owned(&mut sub.iter, self.name.as_ref())?;
            }
            let Some((selected, _current)) = self
                .subiters
                .iter()
                .enumerate()
                .filter_map(|(index, sub)| sub.current.as_ref().map(|record| (index, record)))
                .max_by(|(left_index, left), (right_index, right)| {
                    left.update_index
                        .cmp(&right.update_index)
                        .then_with(|| left_index.cmp(right_index))
                })
            else {
                self.done = true;
                return Ok(None);
            };
            let current = self.subiters[selected]
                .current
                .take()
                .expect("the selected subiterator has a record");
            self.pending = Some(selected);
            for (index, sub) in self.subiters.iter_mut().enumerate() {
                if index != selected
                    && sub
                        .current
                        .as_ref()
                        .is_some_and(|record| record.update_index == current.update_index)
                {
                    sub.current = next_owned(&mut sub.iter, self.name.as_ref())?;
                }
            }
            if let Some(data) = current.data {
                return Ok(Some(data));
            }
        }
        Ok(None)
    }
}

fn next_owned(iter: &mut crate::log::Iter<'_>, name: &BStr) -> Result<Option<Current>> {
    let Some(record) = iter.next_record()? else {
        return Ok(None);
    };
    if record.name != name {
        return Ok(None);
    }
    Ok(Some(Current {
        update_index: record.update_index,
        data: record.data.map(|data| Record {
            name: record.name.to_owned(),
            update_index: record.update_index,
            previous_object_id: data.previous_object_id.to_owned(),
            new_object_id: data.new_object_id.to_owned(),
            committer: data.committer.to_owned(),
            time_seconds: data.time_seconds,
            timezone_minutes: data.timezone_minutes,
            message: data.message.to_owned(),
        }),
    }))
}
