//! An append-only, time-travelling key-value store on SQLite. See README.md.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::types::Value as Sql;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use thiserror::Error;

pub use jiff::Timestamp;

pub type Seq = usize;

#[derive(Debug, Clone, PartialEq)]
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

#[derive(Debug, Error)]
pub enum Error {
    #[error("timestamp {at} is before the last event at {last}")]
    Backwards { at: Timestamp, last: Timestamp },
    #[error("no event at seq {seq}: the log holds {len}")]
    OutOfRange { seq: Seq, len: usize },
    #[error("a batch must contain at least one change")]
    Empty,
    #[error("unreadable row in the log")]
    Corrupt,
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

fn encode(value: Option<&Value>) -> (Option<i64>, Sql) {
    match value {
        None => (None, Sql::Null),
        Some(Value::Bool(b)) => (Some(0), Sql::Integer(*b as i64)),
        Some(Value::Int(i)) => (Some(1), Sql::Integer(*i)),
        Some(Value::Float(f)) => (Some(2), Sql::Real(*f)),
        Some(Value::Str(s)) => (Some(3), Sql::Text(s.clone())),
    }
}

fn decode(kind: Option<i64>, value: Sql) -> Result<Option<Value>, Error> {
    match (kind, value) {
        (None, Sql::Null) => Ok(None),
        (Some(0), Sql::Integer(i)) => Ok(Some(Value::Bool(i != 0))),
        (Some(1), Sql::Integer(i)) => Ok(Some(Value::Int(i))),
        (Some(2), Sql::Real(f)) => Ok(Some(Value::Float(f))),
        (Some(3), Sql::Text(s)) => Ok(Some(Value::Str(s))),
        _ => Err(Error::Corrupt),
    }
}

fn to_nanos(ts: Timestamp) -> i64 {
    i64::try_from(ts.as_nanosecond()).expect("timestamp fits in i64 nanoseconds")
}

fn from_nanos(nanos: i64) -> Timestamp {
    Timestamp::from_nanosecond(nanos as i128).expect("i64 nanoseconds is a valid timestamp")
}

fn resolve_ts(at: Option<Timestamp>, last: Option<Timestamp>) -> Result<Timestamp, Error> {
    match (at, last) {
        (Some(at), Some(last)) if at < last => Err(Error::Backwards { at, last }),
        (Some(at), _) => Ok(at),
        (None, None) => Ok(Timestamp::now()),
        (None, Some(last)) => Ok(Timestamp::now().max(last)),
    }
}

pub trait Writable {}

pub struct ReadOnly;

pub struct InMemory;

pub struct Durable;

impl Writable for InMemory {}

impl Writable for Durable {}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub changes: BTreeMap<String, Option<Value>>,
    pub ts: Timestamp,
}

pub struct Db<M> {
    conn: Connection,
    _mode: M,
}

impl<M> Db<M> {
    fn open(conn: Connection, mode: M) -> Result<Self, Error> {
        conn.execute_batch("PRAGMA busy_timeout = 5000")?;
        Ok(Db { conn, _mode: mode })
    }

    pub fn len(&self) -> Result<usize, Error> {
        let len: i64 = self.conn.query_row(
            "SELECT coalesce(max(seq) + 1, 0) FROM changes",
            [],
            |row| row.get(0),
        )?;
        Ok(len as usize)
    }

    pub fn is_empty(&self) -> Result<bool, Error> {
        Ok(self.len()? == 0)
    }

    pub fn at(&self, seq: Seq) -> Result<Snapshot<'_>, Error> {
        let len = self.len()?;
        if seq >= len {
            return Err(Error::OutOfRange { seq, len });
        }
        Ok(Snapshot {
            conn: &self.conn,
            applied: seq + 1,
        })
    }

    pub fn latest(&self) -> Result<Snapshot<'_>, Error> {
        Ok(Snapshot {
            conn: &self.conn,
            applied: self.len()?,
        })
    }

    pub fn as_of(&self, t: Timestamp) -> Result<Snapshot<'_>, Error> {
        let applied: i64 = self.conn.query_row(
            "WITH RECURSIVE search (lo, hi) AS (
                 SELECT 0, (SELECT coalesce(max(seq) + 1, 0) FROM changes)
                 UNION ALL
                 SELECT
                     CASE WHEN (SELECT ts FROM changes WHERE seq = (lo + hi) / 2 LIMIT 1) <= ?1
                          THEN (lo + hi) / 2 + 1 ELSE lo END,
                     CASE WHEN (SELECT ts FROM changes WHERE seq = (lo + hi) / 2 LIMIT 1) <= ?1
                          THEN hi ELSE (lo + hi) / 2 END
                 FROM search WHERE lo < hi
             )
             SELECT lo FROM search WHERE lo = hi",
            [to_nanos(t)],
            |row| row.get(0),
        )?;
        Ok(Snapshot {
            conn: &self.conn,
            applied: applied as usize,
        })
    }

    pub fn event(&self, seq: Seq) -> Result<Event, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT key, kind, value, ts FROM changes WHERE seq = ?1 ORDER BY key",
        )?;
        let rows = stmt.query_map([seq as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Sql>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut changes = BTreeMap::new();
        let mut ts = None;
        for row in rows {
            let (key, kind, value, nanos) = row?;
            changes.insert(key, decode(kind, value)?);
            ts = Some(from_nanos(nanos));
        }
        match ts {
            Some(ts) => Ok(Event { changes, ts }),
            None => Err(Error::OutOfRange {
                seq,
                len: self.len()?,
            }),
        }
    }

    pub fn history(&self, key: &str) -> Result<Vec<(Seq, Option<Value>, Timestamp)>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, kind, value, ts FROM changes WHERE key = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map([key], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Sql>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (seq, kind, value, ts) = row?;
            Ok((seq as Seq, decode(kind, value)?, from_nanos(ts)))
        })
        .collect()
    }

    pub fn close(self) -> Result<(), Error> {
        self.conn.close().map_err(|(_, err)| Error::Sqlite(err))
    }
}

impl<M: Writable> Db<M> {
    fn create(conn: Connection, mode: M) -> Result<Self, Error> {
        let db = Self::open(conn, mode)?;
        db.conn.execute_batch(SCHEMA)?;
        Ok(db)
    }

    pub fn batch(&mut self) -> Batch<'_, M> {
        Batch {
            db: self,
            changes: BTreeMap::new(),
            at: None,
        }
    }

    fn commit(
        &mut self,
        changes: BTreeMap<String, Option<Value>>,
        at: Option<Timestamp>,
    ) -> Result<Seq, Error> {
        if changes.is_empty() {
            return Err(Error::Empty);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let last: Option<i64> = tx
            .query_row("SELECT ts FROM changes ORDER BY seq DESC LIMIT 1", [], |row| {
                row.get(0)
            })
            .optional()?;
        let ts = resolve_ts(at, last.map(from_nanos))?;
        let seq: i64 = tx.query_row(
            "SELECT coalesce(max(seq) + 1, 0) FROM changes",
            [],
            |row| row.get(0),
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO changes (seq, key, kind, value, ts) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for (key, value) in &changes {
                let (kind, value) = encode(value.as_ref());
                stmt.execute((seq, key, kind, value, to_nanos(ts)))?;
            }
        }
        tx.commit()?;
        Ok(seq as Seq)
    }
}

pub struct Batch<'db, M: Writable> {
    db: &'db mut Db<M>,
    changes: BTreeMap<String, Option<Value>>,
    at: Option<Timestamp>,
}

impl<M: Writable> Batch<'_, M> {
    pub fn set(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.changes.insert(key.into(), Some(value.into()));
        self
    }

    pub fn delete(mut self, key: impl Into<String>) -> Self {
        self.changes.insert(key.into(), None);
        self
    }

    pub fn at(mut self, ts: Timestamp) -> Self {
        self.at = Some(ts);
        self
    }

    pub fn commit(self) -> Result<Seq, Error> {
        self.db.commit(self.changes, self.at)
    }
}

pub struct Snapshot<'db> {
    conn: &'db Connection,
    applied: usize,
}

impl Snapshot<'_> {
    pub fn get(&self, key: &str) -> Result<Option<Value>, Error> {
        let row = self
            .conn
            .query_row(
                "SELECT kind, value FROM changes
                 WHERE key = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT 1",
                (key, self.applied as i64),
                |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Sql>(1)?)),
            )
            .optional()?;
        row.map_or(Ok(None), |(kind, value)| decode(kind, value))
    }

    pub fn entries(&self) -> Result<Vec<(String, Value)>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT c.key, c.kind, c.value
             FROM firsts f
             JOIN changes c ON c.key = f.key
                 AND c.seq = (SELECT max(seq) FROM changes WHERE key = f.key AND seq < ?1)
             WHERE f.first < ?1 AND c.value IS NOT NULL
             ORDER BY c.key",
        )?;
        let rows = stmt.query_map([self.applied as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Sql>(2)?,
            ))
        })?;
        rows.map(|row| {
            let (key, kind, value) = row?;
            Ok((key, decode(kind, value)?.ok_or(Error::Corrupt)?))
        })
        .collect()
    }
}

pub type DiffEntry = (String, Option<Value>, Option<Value>);

pub fn diff(a: &Snapshot<'_>, b: &Snapshot<'_>) -> Result<Vec<DiffEntry>, Error> {
    let mut stmt = a.conn.prepare(
        "WITH sides AS (
             SELECT f.key AS key,
                 (SELECT max(seq) FROM changes WHERE key = f.key AND seq < ?1) AS sa,
                 (SELECT max(seq) FROM changes WHERE key = f.key AND seq < ?2) AS sb
             FROM firsts f WHERE f.first < max(?1, ?2)
         )
         SELECT s.key, a.kind, a.value, b.kind, b.value
         FROM sides s
         LEFT JOIN changes a ON a.key = s.key AND a.seq = s.sa
         LEFT JOIN changes b ON b.key = s.key AND b.seq = s.sb
         WHERE a.kind IS NOT b.kind OR a.value IS NOT b.value
         ORDER BY s.key",
    )?;
    let rows = stmt.query_map([a.applied as i64, b.applied as i64], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, Sql>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, Sql>(4)?,
        ))
    })?;
    rows.map(|row| {
        let (key, ka, va, kb, vb) = row?;
        Ok((key, decode(ka, va)?, decode(kb, vb)?))
    })
    .collect()
}

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS changes (
        seq   INTEGER NOT NULL,
        key   TEXT NOT NULL,
        kind  INTEGER,
        value ANY,
        ts    INTEGER NOT NULL,
        PRIMARY KEY (key, seq)
    ) STRICT, WITHOUT ROWID;
    CREATE INDEX IF NOT EXISTS changes_by_seq ON changes (seq, key);
    CREATE VIEW IF NOT EXISTS firsts (key, first) AS
    WITH RECURSIVE scan (key) AS (
        SELECT min(key) FROM changes
        UNION ALL
        SELECT (SELECT min(key) FROM changes WHERE key > scan.key)
        FROM scan WHERE scan.key IS NOT NULL
    )
    SELECT key, (SELECT min(seq) FROM changes WHERE key = scan.key)
    FROM scan WHERE key IS NOT NULL;
    CREATE VIEW IF NOT EXISTS latest (key, kind, value, seq, ts) AS
    SELECT c.key, c.kind, c.value, c.seq, c.ts
    FROM firsts f
    JOIN changes c ON c.key = f.key
        AND c.seq = (SELECT max(seq) FROM changes WHERE key = f.key)
    WHERE c.value IS NOT NULL;";

pub fn connect(path: impl AsRef<Path>) -> Result<Db<Durable>, Error> {
    Db::create(Connection::open(path)?, Durable)
}

pub fn inspect(path: impl AsRef<Path>) -> Result<Db<ReadOnly>, Error> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    Db::open(conn, ReadOnly)
}

pub fn in_memory() -> Result<Db<InMemory>, Error> {
    Db::create(Connection::open_in_memory()?, InMemory)
}
