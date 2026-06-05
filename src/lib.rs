//! A small append-only, time-travelling key-value store.
//!
//! Writes are [`Event`]s appended to a log; state is folded from them. Each
//! version shares structure with the last, so the store keeps them all cheaply.
//!
//! ```
//! use time_travel_db_rs::{in_memory, Value};
//!
//! let mut db = in_memory();
//! db.batch().set("votes", 1).commit().unwrap();
//! db.batch().set("votes", 2).commit().unwrap();
//!
//! assert_eq!(db.at(0).unwrap().get("votes"), Some(&Value::Int(1)));
//! assert_eq!(db.latest().get("votes"), Some(&Value::Int(2)));
//! ```

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use imbl::ordmap::DiffItem;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use imbl::OrdMap;
pub use jiff::Timestamp;

/// An event's position in the log.
pub type Seq = usize;

/// A scalar the store can hold.
///
/// No null variant by design: absence is a missing key, removal is
/// [`Change::Delete`]. That is what frees `null` to mean "deleted" on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Float(v)
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Str(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Str(v.to_owned())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Change {
    Set(Value),
    /// Serialized as JSON `null`.
    Delete,
}

impl Change {
    fn value(&self) -> Option<&Value> {
        match self {
            Change::Set(value) => Some(value),
            Change::Delete => None,
        }
    }
}

/// One atomic write: per-key [`Change`]s at a moment in time.
///
/// No sequence number is stored; an entry's position in the log is its [`Seq`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub changes: BTreeMap<String, Change>,
    pub ts: Timestamp,
}

pub type State = OrdMap<String, Value>;

fn fold(state: &State, event: &Event) -> State {
    let mut next = state.clone();
    for (key, change) in &event.changes {
        match change {
            Change::Set(value) => {
                next.insert(key.clone(), value.clone());
            }
            Change::Delete => {
                next.remove(key);
            }
        }
    }
    next
}

/// The keys that differ between two states, as `(key, before, after)`.
pub fn diff<'a>(
    a: &'a State,
    b: &'a State,
) -> impl Iterator<Item = (&'a str, Option<&'a Value>, Option<&'a Value>)> {
    a.diff(b).map(|item| match item {
        DiffItem::Add(key, after) => (key.as_str(), None, Some(after)),
        DiffItem::Remove(key, before) => (key.as_str(), Some(before), None),
        DiffItem::Update {
            old: (key, before),
            new: (_, after),
        } => (key.as_str(), Some(before), Some(after)),
    })
}

/// Each touch of a single key, as `(seq, value, ts)`. `value` is `None` at a delete.
pub fn history<'a>(
    events: impl IntoIterator<Item = &'a Event> + 'a,
    key: &'a str,
) -> impl Iterator<Item = (Seq, Option<&'a Value>, Timestamp)> {
    events
        .into_iter()
        .enumerate()
        .filter_map(move |(seq, event)| {
            event
                .changes
                .get(key)
                .map(|change| (seq, change.value(), event.ts))
        })
}

#[derive(Debug, Error)]
pub enum OpenError {
    #[error("database at {} is locked by another process", .path.display())]
    Locked { path: PathBuf },
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Debug, Error)]
pub enum CommitError {
    #[error("timestamp {at} is before the last event at {last}")]
    Backwards { at: Timestamp, last: Timestamp },
    #[error("could not persist the event")]
    Io(#[from] io::Error),
}

#[derive(Debug, Error)]
#[error("no event at seq {seq}: the log holds {len}")]
pub struct OutOfRange {
    pub seq: Seq,
    pub len: usize,
}

/// The modes that accept writes. Sealed in practice: only this crate's types
/// implement it, and a [`Db`] is only built through the constructors.
#[doc(hidden)]
pub trait Writable {
    fn persist(&mut self, event: &Event) -> io::Result<()>;
}

pub struct ReadOnly;

pub struct InMemory;

pub struct Durable {
    file: File,
}

impl Writable for InMemory {
    fn persist(&mut self, _event: &Event) -> io::Result<()> {
        Ok(())
    }
}

impl Writable for Durable {
    fn persist(&mut self, event: &Event) -> io::Result<()> {
        self.file.write_all(&encode(event))?;
        self.file.sync_data()
    }
}

/// An append-only, time-travelling key-value store.
///
/// The mode parameter is [`ReadOnly`], [`InMemory`], or [`Durable`]: all read,
/// only the writable ones write.
pub struct Db<P> {
    // Append-only and non-decreasing in `ts` (maintained by `resolve_ts`),
    // which is what lets `as_of` binary-search.
    events: Vec<Event>,
    // states[k] is the state after the first k events: states[0] is empty,
    // states[events.len()] is the latest.
    states: Vec<State>,
    backend: P,
}

impl<P> Db<P> {
    fn from_events(events: Vec<Event>, backend: P) -> Self {
        let mut states = Vec::with_capacity(events.len() + 1);
        states.push(State::new());
        for event in &events {
            let next = fold(states.last().unwrap(), event);
            states.push(next);
        }
        Db {
            events,
            states,
            backend,
        }
    }

    /// The state immediately after event `seq`.
    pub fn at(&self, seq: Seq) -> Result<&State, OutOfRange> {
        self.states.get(seq + 1).ok_or(OutOfRange {
            seq,
            len: self.events.len(),
        })
    }

    pub fn latest(&self) -> &State {
        self.states.last().unwrap()
    }

    pub fn as_of(&self, t: Timestamp) -> &State {
        let applied = self.events.partition_point(|event| event.ts <= t);
        &self.states[applied]
    }

    /// The full event log, oldest first.
    pub fn events(&self) -> &[Event] {
        &self.events
    }
}

fn resolve_ts(at: Option<Timestamp>, last: Option<Timestamp>) -> Result<Timestamp, CommitError> {
    match (at, last) {
        (Some(at), Some(last)) if at < last => Err(CommitError::Backwards { at, last }),
        (Some(at), _) => Ok(at),
        // No explicit time: now, but never before the previous event.
        (None, None) => Ok(Timestamp::now()),
        (None, Some(last)) => Ok(Timestamp::now().max(last)),
    }
}

impl<P: Writable> Db<P> {
    /// Begin a batch that commits atomically as one event.
    pub fn batch(&mut self) -> Batch<'_, P> {
        Batch {
            db: self,
            changes: BTreeMap::new(),
            at: None,
        }
    }

    fn commit(
        &mut self,
        changes: BTreeMap<String, Change>,
        at: Option<Timestamp>,
    ) -> Result<Seq, CommitError> {
        let last = self.events.last().map(|event| event.ts);
        let ts = resolve_ts(at, last)?;
        let event = Event { changes, ts };

        // Persist before touching memory, so a failed write leaves the
        // in-memory state exactly matching what is on disk.
        self.backend.persist(&event)?;

        let seq = self.events.len();
        let next = fold(self.states.last().unwrap(), &event);
        self.events.push(event);
        self.states.push(next);
        Ok(seq)
    }
}

impl Db<Durable> {
    /// Release the lock, surfacing any error. Dropping the store also releases
    /// it, but silently.
    pub fn close(self) -> io::Result<()> {
        self.backend.file.unlock()
    }
}

/// Changes that commit atomically as a single event. Built by [`Db::batch`].
pub struct Batch<'db, P: Writable> {
    db: &'db mut Db<P>,
    changes: BTreeMap<String, Change>,
    at: Option<Timestamp>,
}

impl<P: Writable> Batch<'_, P> {
    pub fn set(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.changes.insert(key.into(), Change::Set(value.into()));
        self
    }

    pub fn delete(mut self, key: impl Into<String>) -> Self {
        self.changes.insert(key.into(), Change::Delete);
        self
    }

    /// Stamp the event with an explicit time; it may not precede the last.
    pub fn at(mut self, ts: Timestamp) -> Self {
        self.at = Some(ts);
        self
    }

    pub fn commit(self) -> Result<Seq, CommitError> {
        self.db.commit(self.changes, self.at)
    }
}

fn encode(event: &Event) -> Vec<u8> {
    let mut line = serde_json::to_vec(event).expect("an event always serializes");
    line.push(b'\n');
    line
}

/// Parse events until the first torn or unparsable line. Also returns the
/// byte length of the clean prefix, for the caller to truncate to.
fn parse_log(mut reader: impl BufRead) -> io::Result<(Vec<Event>, u64)> {
    let mut events = Vec::new();
    let mut consumed = 0u64;
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 || !line.ends_with('\n') {
            break;
        }
        match serde_json::from_str::<Event>(line.trim_end()) {
            Ok(event) => {
                events.push(event);
                consumed += read as u64;
            }
            Err(_) => break,
        }
    }
    Ok((events, consumed))
}

fn read_events(file: &mut File) -> io::Result<(Vec<Event>, u64)> {
    file.seek(SeekFrom::Start(0))?;
    parse_log(BufReader::new(file))
}

/// Open or create a durable store at `path`, replaying and repairing its log.
pub fn connect(path: impl AsRef<Path>) -> Result<Db<Durable>, OpenError> {
    let path = path.as_ref();
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(path)?;
    file.try_lock().map_err(|err| match err {
        TryLockError::WouldBlock => OpenError::Locked {
            path: path.to_owned(),
        },
        TryLockError::Error(err) => OpenError::Io(err),
    })?;

    let (events, consumed) = read_events(&mut file)?;
    file.set_len(consumed)?;

    Ok(Db::from_events(events, Durable { file }))
}

/// Open a store on disk read-only, without locking or repairing it.
///
/// The returned store is not writable:
///
/// ```compile_fail
/// let mut db = time_travel_db_rs::inspect("log.json").unwrap();
/// db.batch();
/// ```
pub fn inspect(path: impl AsRef<Path>) -> Result<Db<ReadOnly>, OpenError> {
    let mut file = OpenOptions::new().read(true).open(path)?;
    let (events, _) = read_events(&mut file)?;
    Ok(Db::from_events(events, ReadOnly))
}

pub fn in_memory() -> Db<InMemory> {
    Db::from_events(Vec::new(), InMemory)
}

#[cfg(test)]
mod tests;
