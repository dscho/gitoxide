use bstr::{BStr, BString, ByteSlice};
use gix_error::Result;

use crate::{reference::ValueRef, stack::Snapshot};

/// A merged reference with storage tombstones suppressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// The complete byte-oriented reference name.
    pub name: BString,
    /// The update index from the selected table record.
    pub update_index: u64,
    /// The stored value.
    pub value: Value,
}

/// An owned direct or symbolic reference value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// An object ID and its optional peeled target.
    Object {
        /// The object directly named by the reference.
        object_id: gix_hash::ObjectId,
        /// The peeled target, if stored.
        peeled_id: Option<gix_hash::ObjectId>,
    },
    /// The uninterpreted target of a symbolic reference.
    Symbolic(BString),
}

pub(super) struct Current {
    pub name: BString,
    pub update_index: u64,
    pub value: Option<Value>,
}

struct SubIter<'a> {
    iter: crate::reference::Iter<'a>,
    current: Option<Current>,
}

/// A merged reference cursor holding one current record per table.
pub struct Iter<'a> {
    subiters: Vec<SubIter<'a>>,
    prefix: Option<BString>,
    /// The selected subiterator is advanced lazily so a later decode error
    /// cannot swallow a record that was already decoded successfully.
    pending: Option<usize>,
    done: bool,
}

impl Snapshot {
    /// Iterate all visible references in byte order.
    pub fn references(&self) -> Result<Iter<'_>> {
        Iter::new(self, None)
    }

    /// Iterate visible references whose names start with `prefix`.
    pub fn references_prefixed(&self, prefix: &BStr) -> Result<Iter<'_>> {
        Iter::new(self, Some(prefix.to_owned()))
    }

    /// Find an exact visible reference.
    pub fn try_find_reference(&self, name: &BStr) -> Result<Option<Reference>> {
        let mut iter = self.references_prefixed(name)?;
        Ok(iter.next_record()?.filter(|reference| reference.name.as_bstr() == name))
    }
}

impl<'a> Iter<'a> {
    fn new(snapshot: &'a Snapshot, prefix: Option<BString>) -> Result<Self> {
        Self::from_tables(snapshot.parsed_tables()?, prefix)
    }

    pub(super) fn from_tables(tables: Vec<crate::Table<'a>>, prefix: Option<BString>) -> Result<Self> {
        let mut subiters = Vec::with_capacity(tables.len());
        for table in tables {
            let mut iter = table.references();
            if let Some(prefix) = prefix.as_ref() {
                iter.seek(prefix.as_ref())?;
            }
            let current = next_owned(&mut iter)?;
            subiters.push(SubIter { iter, current });
        }
        Ok(Iter {
            subiters,
            prefix,
            pending: None,
            done: false,
        })
    }

    /// Return the next visible reference, suppressing overwritten records and tombstones.
    pub fn next_record(&mut self) -> Result<Option<Reference>> {
        while let Some(current) = self.next_stored()? {
            if let Some(value) = current.value {
                return Ok(Some(Reference {
                    name: current.name,
                    update_index: current.update_index,
                    value,
                }));
            }
        }
        Ok(None)
    }

    pub(super) fn next_stored(&mut self) -> Result<Option<Current>> {
        let result = self.advance();
        if result.is_err() {
            self.done = true;
        }
        result
    }

    fn advance(&mut self) -> Result<Option<Current>> {
        if self.done {
            return Ok(None);
        }
        if let Some(index) = self.pending.take() {
            let sub = &mut self.subiters[index];
            sub.current = next_owned(&mut sub.iter)?;
        }
        let Some((selected, current)) = self
            .subiters
            .iter()
            .enumerate()
            .filter_map(|(index, sub)| sub.current.as_ref().map(|record| (index, record)))
            .min_by(|(left_index, left), (right_index, right)| {
                left.name.cmp(&right.name).then_with(|| right_index.cmp(left_index))
            })
        else {
            self.done = true;
            return Ok(None);
        };
        if self
            .prefix
            .as_ref()
            .is_some_and(|prefix| !current.name.starts_with(prefix.as_slice()))
        {
            self.done = true;
            return Ok(None);
        }
        let current = self.subiters[selected]
            .current
            .take()
            .expect("the selected subiterator has a record");
        self.pending = Some(selected);
        for (index, sub) in self.subiters.iter_mut().enumerate() {
            if index != selected && sub.current.as_ref().is_some_and(|record| record.name == current.name) {
                sub.current = next_owned(&mut sub.iter)?;
            }
        }
        Ok(Some(current))
    }
}

fn next_owned(iter: &mut crate::reference::Iter<'_>) -> Result<Option<Current>> {
    Ok(iter.next_record()?.map(|record| Current {
        name: record.name.to_owned(),
        update_index: record.update_index,
        value: match record.value {
            ValueRef::Deletion => None,
            ValueRef::Object { object_id, peeled_id } => Some(Value::Object {
                object_id: object_id.to_owned(),
                peeled_id: peeled_id.map(ToOwned::to_owned),
            }),
            ValueRef::Symbolic(target) => Some(Value::Symbolic(target.to_owned())),
        },
    }))
}
