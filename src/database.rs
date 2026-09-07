use std::collections::{BTreeMap, BTreeSet};
use std::marker::PhantomData;
use std::path::Path;

use thiserror::Error;

use crate::Timestamp;
use crate::sqlite::Sqlite;

pub type EventId = u64;
pub type State = BTreeMap<String, Value>;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Value::Bool(value)
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Value::Int(value)
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Value::Float(value)
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Value::Str(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Value::Str(value.to_owned())
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("cannot commit an empty collection of writes")]
    EmptyCommit,
    #[error("no event {id}: the log holds {len} events")]
    OutOfRange { id: EventId, len: u64 },
    #[error("unreadable row in the log")]
    Corrupt,
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Assertion {
    pub key: String,
    pub valid_from: Timestamp,
    pub value: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub id: EventId,
    pub committed_at: Timestamp,
    pub assertions: Vec<Assertion>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordedAssertion {
    pub event_id: EventId,
    pub committed_at: Timestamp,
    pub assertion: Assertion,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Delta {
    Added(Value),
    Removed(Value),
    Changed { before: Value, after: Value },
}

enum ValidFrom {
    Commit,
    At(Timestamp),
}

pub struct Write {
    key: String,
    valid_from: ValidFrom,
    value: Option<Value>,
}

impl Write {
    pub fn set(key: impl Into<String>, value: impl Into<Value>) -> Self {
        Write {
            key: key.into(),
            valid_from: ValidFrom::Commit,
            value: Some(value.into()),
        }
    }

    pub fn set_at(key: impl Into<String>, value: impl Into<Value>, valid_from: Timestamp) -> Self {
        Write {
            key: key.into(),
            valid_from: ValidFrom::At(valid_from),
            value: Some(value.into()),
        }
    }

    pub fn delete(key: impl Into<String>) -> Self {
        Write {
            key: key.into(),
            valid_from: ValidFrom::Commit,
            value: None,
        }
    }

    pub fn delete_at(key: impl Into<String>, valid_from: Timestamp) -> Self {
        Write {
            key: key.into(),
            valid_from: ValidFrom::At(valid_from),
            value: None,
        }
    }
}

pub(crate) struct ResolvedWrite {
    pub(crate) key: String,
    pub(crate) valid_from: i64,
    pub(crate) value: Option<Value>,
}

pub(crate) fn resolve_writes(
    writes: Vec<Write>,
    committed_at: Timestamp,
) -> impl Iterator<Item = ResolvedWrite> {
    let commit_micros = committed_at.as_microsecond();
    let unique: BTreeMap<_, _> = writes
        .into_iter()
        .map(|write| {
            let valid_from = match write.valid_from {
                ValidFrom::Commit => commit_micros,
                ValidFrom::At(valid_from) => valid_from.as_microsecond(),
            };
            ((write.key, valid_from), write.value)
        })
        .collect();

    unique
        .into_iter()
        .map(|((key, valid_from), value)| ResolvedWrite {
            key,
            valid_from,
            value,
        })
}

pub struct ReadOnly;
pub struct ReadWrite;

pub struct Handle<Access> {
    sqlite: Sqlite,
    access: PhantomData<Access>,
}

pub type Database = Handle<ReadWrite>;
pub type Reader = Handle<ReadOnly>;

impl Handle<ReadOnly> {
    pub fn inspect(path: impl AsRef<Path>) -> Result<Self, Error> {
        Ok(Handle::new(Sqlite::inspect(path)?))
    }
}

impl Handle<ReadWrite> {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Ok(Handle::new(Sqlite::open(path)?))
    }

    pub fn memory() -> Result<Self, Error> {
        Ok(Handle::new(Sqlite::memory()?))
    }

    /// Records writes as one atomic event, returning its ID.
    ///
    /// Accepts arrays, vectors, or iterators of [`Write`]. The input is collected
    /// before acquiring the database's write lock. Empty input returns
    /// [`Error::EmptyCommit`] without starting a transaction.
    ///
    /// Writes without an explicit valid time use the event's commit timestamp.
    /// For repeated keys at the same valid time, after truncation to
    /// microseconds, the last supplied write wins.
    ///
    /// ```
    /// use two_tdb::{Database, Write};
    ///
    /// let mut db = Database::memory()?;
    /// let event = db.commit([
    ///     Write::set("name", "Ada"),
    ///     Write::set("active", true),
    ///     Write::delete("pending"),
    /// ])?;
    /// assert_eq!(db.event(event)?.assertions.len(), 3);
    /// # Ok::<(), two_tdb::Error>(())
    /// ```
    pub fn commit(&mut self, writes: impl IntoIterator<Item = Write>) -> Result<EventId, Error> {
        self.sqlite.commit(writes.into_iter().collect())
    }
}

impl<Access> Handle<Access> {
    fn new(sqlite: Sqlite) -> Self {
        Handle {
            sqlite,
            access: PhantomData,
        }
    }

    pub fn len(&self) -> Result<u64, Error> {
        self.sqlite.len()
    }

    pub fn is_empty(&self) -> Result<bool, Error> {
        Ok(self.len()? == 0)
    }

    pub fn at(&self, id: EventId) -> Result<Snapshot<'_>, Error> {
        let valid_through = self.sqlite.event_timestamp(id)?;
        Ok(Snapshot::new(
            &self.sqlite,
            TransactionCutoff::Through(id),
            ValidCutoff::Through(valid_through),
        ))
    }

    pub fn known_at(&self, time: Timestamp) -> Result<Snapshot<'_>, Error> {
        let transaction = self.sqlite.event_id_at(time)?.into();
        Ok(Snapshot::new(
            &self.sqlite,
            transaction,
            ValidCutoff::Through(time),
        ))
    }

    pub fn latest(&self) -> Result<Snapshot<'_>, Error> {
        let transaction = self.sqlite.latest_event_id()?.into();
        Ok(Snapshot::new(
            &self.sqlite,
            transaction,
            ValidCutoff::Through(Timestamp::now()),
        ))
    }

    pub fn event(&self, id: EventId) -> Result<Event, Error> {
        self.sqlite.event(id)
    }

    pub fn keys(&self) -> Result<Vec<String>, Error> {
        self.sqlite.keys()
    }

    pub fn history(&self, key: &str) -> Result<Vec<RecordedAssertion>, Error> {
        self.sqlite.history(key)
    }

    pub fn close(self) -> Result<(), Error> {
        self.sqlite.close()
    }

    pub fn bisect<Predicate>(&self, mut predicate: Predicate) -> Result<Option<EventId>, Error>
    where
        Predicate: FnMut(Snapshot<'_>) -> Result<bool, Error>,
    {
        let mut first = 1;
        let mut last = self.len()?;
        let mut found = None;

        while first <= last {
            let middle = first + (last - first) / 2;
            if predicate(self.at(middle)?)? {
                found = Some(middle);
                if middle == 1 {
                    break;
                }
                last = middle - 1;
            } else {
                first = middle + 1;
            }
        }

        Ok(found)
    }
}

#[derive(Clone, Copy)]
pub(crate) enum TransactionCutoff {
    BeforeFirst,
    Through(EventId),
}

impl From<Option<EventId>> for TransactionCutoff {
    fn from(event: Option<EventId>) -> Self {
        match event {
            Some(id) => Self::Through(id),
            None => Self::BeforeFirst,
        }
    }
}

impl TransactionCutoff {
    pub(crate) fn event_id(self) -> Option<EventId> {
        match self {
            Self::BeforeFirst => None,
            Self::Through(id) => Some(id),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ValidCutoff {
    Through(Timestamp),
    Unbounded,
}

impl ValidCutoff {
    pub(crate) fn timestamp(self) -> Option<Timestamp> {
        match self {
            Self::Through(time) => Some(time),
            Self::Unbounded => None,
        }
    }

    pub(crate) fn micros(self) -> Option<i64> {
        self.timestamp().map(|time| time.as_microsecond())
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Coordinate {
    pub(crate) transaction: TransactionCutoff,
    pub(crate) valid: ValidCutoff,
}

#[derive(Clone, Copy)]
pub struct Snapshot<'db> {
    sqlite: &'db Sqlite,
    coordinate: Coordinate,
}

impl<'db> Snapshot<'db> {
    fn new(sqlite: &'db Sqlite, transaction: TransactionCutoff, valid: ValidCutoff) -> Self {
        Snapshot {
            sqlite,
            coordinate: Coordinate { transaction, valid },
        }
    }

    pub fn event_id(&self) -> Option<EventId> {
        self.coordinate.transaction.event_id()
    }

    pub fn valid_through(&self) -> Option<Timestamp> {
        self.coordinate.valid.timestamp()
    }

    pub fn valid_at(self, time: Timestamp) -> Self {
        Snapshot::new(
            self.sqlite,
            self.coordinate.transaction,
            ValidCutoff::Through(time),
        )
    }

    pub fn valid_unbounded(self) -> Self {
        Snapshot::new(
            self.sqlite,
            self.coordinate.transaction,
            ValidCutoff::Unbounded,
        )
    }

    pub fn get(&self, key: &str) -> Result<Option<Value>, Error> {
        self.sqlite.get(self.coordinate, key)
    }

    pub fn blame(&self, key: &str) -> Result<Option<RecordedAssertion>, Error> {
        self.sqlite.blame(self.coordinate, key)
    }

    pub fn changepoints(&self) -> Result<Vec<Timestamp>, Error> {
        self.sqlite.changepoints(self.coordinate.transaction)
    }

    pub fn state(&self) -> Result<State, Error> {
        self.sqlite.state(self.coordinate)
    }

    pub fn diff(&self, other: &Snapshot<'_>) -> Result<BTreeMap<String, Delta>, Error> {
        let before = self.state()?;
        let after = other.state()?;
        let keys: BTreeSet<_> = before.keys().chain(after.keys()).collect();

        Ok(keys
            .into_iter()
            .filter_map(|key| match (before.get(key), after.get(key)) {
                (None, Some(after)) => Some((key.clone(), Delta::Added(after.clone()))),
                (Some(before), None) => Some((key.clone(), Delta::Removed(before.clone()))),
                (Some(before), Some(after)) if before != after => Some((
                    key.clone(),
                    Delta::Changed {
                        before: before.clone(),
                        after: after.clone(),
                    },
                )),
                _ => None,
            })
            .collect())
    }
}
