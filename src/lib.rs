use std::collections::BTreeMap;
use std::marker::PhantomData;
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

fn decode_value(kind: i64, value: Sql) -> Result<Value, Error> {
    match (kind, value) {
        (0, Sql::Integer(i)) => Ok(Value::Bool(i != 0)),
        (1, Sql::Integer(i)) => Ok(Value::Int(i)),
        (2, Sql::Real(f)) => Ok(Value::Float(f)),
        (3, Sql::Text(s)) => Ok(Value::Str(s)),
        _ => Err(Error::Corrupt),
    }
}

fn decode(kind: Option<i64>, value: Sql) -> Result<Option<Value>, Error> {
    match (kind, value) {
        (None, Sql::Null) => Ok(None),
        (None, _) => Err(Error::Corrupt),
        (Some(kind), value) => decode_value(kind, value).map(Some),
    }
}

fn from_micros(micros: i64) -> Result<Timestamp, Error> {
    Timestamp::from_microsecond(micros).map_err(|_| Error::Corrupt)
}

/// the assertion in force at a coordinate
/// `valid` and `applied` name the parameter placeholders
/// the key correlates with the enclosing `keys k`
fn winner(valid: &str, applied: &str) -> String {
    format!(
        "(SELECT w.key, w.valid, w.seq FROM changes w
           WHERE w.key = k.key AND w.valid <= {valid} AND w.seq < {applied}
           ORDER BY w.valid DESC, w.seq DESC LIMIT 1)"
    )
}

const UNBOUNDED: i64 = i64::MAX;

mod sealed { pub trait Sealed {} }

pub trait DbMode : sealed::Sealed {}
pub trait Writable : DbMode {}

pub struct ReadOnly;
pub struct InMemory;
pub struct Durable;

impl sealed::Sealed for ReadOnly {}
impl sealed::Sealed for InMemory {}
impl sealed::Sealed for Durable {}

impl DbMode for ReadOnly {}
impl DbMode for InMemory {}
impl DbMode for Durable {}

impl Writable for InMemory {}
impl Writable for Durable {}

#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub key: String,
    pub valid: Timestamp,
    pub value: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub changes: Vec<Change>,
    pub ts: Timestamp,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Assertion {
    pub seq: Seq,
    pub valid: Timestamp,
    pub value: Option<Value>,
    pub ts: Timestamp,
}

pub struct Db<M: DbMode> {
    conn: Connection,
    _mode: PhantomData<M>,
}

impl<M: DbMode> Db<M> {
    fn open(conn: Connection) -> Result<Self, Error> {
        conn.execute_batch("PRAGMA busy_timeout = 5000; PRAGMA foreign_keys = ON")?;
        Ok(Db { conn, _mode: PhantomData })
    }

    pub fn len(&self) -> Result<usize, Error> {
        let len: i64 =
            self.conn
                .query_row("SELECT coalesce(max(seq) + 1, 0) FROM events", [], |row| {
                    row.get(0)
                })?;
        Ok(len as usize)
    }

    pub fn is_empty(&self) -> Result<bool, Error> {
        Ok(self.len()? == 0)
    }

    fn event_ts(&self, seq: Seq) -> Result<i64, Error> {
        let ts: Option<i64> = self
            .conn
            .query_row(
                "SELECT ts FROM events WHERE seq = ?1",
                [seq as i64],
                |row| row.get(0),
            )
            .optional()?;
        match ts {
            Some(ts) => Ok(ts),
            None => Err(Error::OutOfRange {
                seq,
                len: self.len()?,
            }),
        }
    }

    pub fn at(&self, seq: Seq) -> Result<Snapshot<'_>, Error> {
        Ok(Snapshot {
            conn: &self.conn,
            applied: seq + 1,
            valid: self.event_ts(seq)?,
        })
    }

    pub fn known_at(&self, t: Timestamp) -> Result<Snapshot<'_>, Error> {
        let seq: Option<i64> = self
            .conn
            .query_row(
                "SELECT seq FROM events WHERE ts <= ?1
                 ORDER BY ts DESC, seq DESC LIMIT 1",
                [t.as_microsecond()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(Snapshot {
            conn: &self.conn,
            applied: seq.map_or(0, |seq| seq as usize + 1),
            valid: t.as_microsecond(),
        })
    }

    pub fn latest(&self) -> Result<Snapshot<'_>, Error> {
        Ok(Snapshot {
            conn: &self.conn,
            applied: self.len()?,
            valid: Timestamp::now().as_microsecond(),
        })
    }

    pub fn event(&self, seq: Seq) -> Result<Event, Error> {
        let ts = from_micros(self.event_ts(seq)?)?;
        let mut stmt = self.conn.prepare(
            "SELECT key, valid, kind, value FROM changes
             WHERE seq = ?1 ORDER BY key, valid",
        )?;
        let rows = stmt.query_map([seq as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Sql>(3)?,
            ))
        })?;
        let changes = rows
            .map(|row| {
                let (key, valid, kind, value) = row?;
                Ok(Change {
                    key,
                    valid: from_micros(valid)?,
                    value: decode(kind, value)?,
                })
            })
            .collect::<Result<_, Error>>()?;
        Ok(Event { changes, ts })
    }

    pub fn history(&self, key: &str) -> Result<Vec<Assertion>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT c.seq, c.valid, c.kind, c.value, e.ts
             FROM changes c JOIN events e ON e.seq = c.seq
             WHERE c.key = ?1 ORDER BY c.seq, c.valid",
        )?;
        let rows = stmt.query_map([key], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Sql>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        rows.map(|row| {
            let (seq, valid, kind, value, ts) = row?;
            Ok(Assertion {
                seq: seq as Seq,
                valid: from_micros(valid)?,
                value: decode(kind, value)?,
                ts: from_micros(ts)?,
            })
        })
        .collect()
    }

    /// the first event after which `pred` holds, by bisection
    /// assumes `pred` flips once from false to true along the log
    /// (the git-bisect contract); probes see `at(seq)`, so pin the
    /// valid time inside the predicate (`s.valid_at(v)`) to hold it
    /// fixed while knowledge varies
    pub fn when<F>(&self, mut pred: F) -> Result<Option<Seq>, Error>
    where
        F: FnMut(Snapshot<'_>) -> Result<bool, Error>,
    {
        let len = self.len()?;
        let (mut lo, mut hi) = (0, len);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if pred(self.at(mid)?)? {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        Ok((lo < len).then_some(lo))
    }

    pub fn close(self) -> Result<(), Error> {
        self.conn.close().map_err(|(_, err)| Error::Sqlite(err))
    }
}

impl<M: Writable> Db<M> {
    fn create(conn: Connection) -> Result<Self, Error> {
        let db = Self::open(conn)?;
        db.conn.execute_batch(SCHEMA)?;
        Ok(db)
    }

    pub fn batch(&mut self) -> Batch<'_, M> {
        Batch {
            db: self,
            changes: BTreeMap::new(),
        }
    }

    fn commit(
        &mut self,
        changes: BTreeMap<(String, Option<i64>), Option<Value>>,
    ) -> Result<Seq, Error> {
        if changes.is_empty() {
            return Err(Error::Empty);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ts = Timestamp::now().as_microsecond();
        let seq: i64 = tx.query_row(
            "INSERT INTO events (seq, ts)
             SELECT coalesce(max(seq) + 1, 0), ?1 FROM events
             RETURNING seq",
            [ts],
            |row| row.get(0),
        )?;
        let resolved: BTreeMap<(String, i64), Option<Value>> = changes
            .into_iter()
            .map(|((key, valid), value)| ((key, valid.unwrap_or(ts)), value))
            .collect();
        {
            let mut stmt = tx.prepare(
                "INSERT INTO changes (seq, key, valid, kind, value)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for ((key, valid), value) in &resolved {
                let (kind, value) = encode(value.as_ref());
                stmt.execute((seq, key, valid, kind, value))?;
            }
        }
        tx.commit()?;
        Ok(seq as Seq)
    }
}

pub struct Batch<'db, M: Writable> {
    db: &'db mut Db<M>,
    changes: BTreeMap<(String, Option<i64>), Option<Value>>,
}

impl<M: Writable> Batch<'_, M> {
    pub fn set(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.changes.insert((key.into(), None), Some(value.into()));
        self
    }

    pub fn set_from(
        mut self,
        key: impl Into<String>,
        value: impl Into<Value>,
        valid: Timestamp,
    ) -> Self {
        self.changes
            .insert((key.into(), Some(valid.as_microsecond())), Some(value.into()));
        self
    }

    pub fn delete(mut self, key: impl Into<String>) -> Self {
        self.changes.insert((key.into(), None), None);
        self
    }

    pub fn delete_from(mut self, key: impl Into<String>, valid: Timestamp) -> Self {
        self.changes
            .insert((key.into(), Some(valid.as_microsecond())), None);
        self
    }

    pub fn commit(self) -> Result<Seq, Error> {
        self.db.commit(self.changes)
    }
}

#[derive(Clone, Copy)]
pub struct Snapshot<'db> {
    conn: &'db Connection,
    applied: usize,
    valid: i64,
}

impl Snapshot<'_> {
    pub fn valid_at(self, v: Timestamp) -> Self {
        Snapshot {
            valid: v.as_microsecond(),
            ..self
        }
    }

    pub fn valid_unbounded(self) -> Self {
        Snapshot {
            valid: UNBOUNDED,
            ..self
        }
    }

    pub fn get(&self, key: &str) -> Result<Option<Value>, Error> {
        let row = self
            .conn
            .query_row(
                "SELECT kind, value FROM changes
                 WHERE key = ?1 AND valid <= ?2 AND seq < ?3
                 ORDER BY valid DESC, seq DESC LIMIT 1",
                (key, self.valid, self.applied as i64),
                |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Sql>(1)?)),
            )
            .optional()?;
        row.map_or(Ok(None), |(kind, value)| decode(kind, value))
    }

    pub fn entries(&self) -> Result<Vec<(String, Value)>, Error> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT c.key, c.kind, c.value
             FROM keys k
             JOIN changes c ON (c.key, c.valid, c.seq) = {}
             WHERE c.kind IS NOT NULL
             ORDER BY c.key",
            winner("?1", "?2")
        ))?;
        let rows = stmt.query_map((self.valid, self.applied as i64), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Sql>(2)?,
            ))
        })?;
        rows.map(|row| {
            let (key, kind, value) = row?;
            Ok((key, decode_value(kind, value)?))
        })
        .collect()
    }
}

pub type DiffEntry = (String, Option<Value>, Option<Value>);

pub fn diff(a: &Snapshot<'_>, b: &Snapshot<'_>) -> Result<Vec<DiffEntry>, Error> {
    let mut stmt = a.conn.prepare(&format!(
        "SELECT k.key, a.kind, a.value, b.kind, b.value
         FROM keys k
         LEFT JOIN changes a ON (a.key, a.valid, a.seq) = {}
         LEFT JOIN changes b ON (b.key, b.valid, b.seq) = {}
         WHERE a.kind IS NOT b.kind OR a.value IS NOT b.value
         ORDER BY k.key",
        winner("?1", "?2"),
        winner("?3", "?4")
    ))?;
    let rows = stmt.query_map(
        (a.valid, a.applied as i64, b.valid, b.applied as i64),
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Sql>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Sql>(4)?,
            ))
        },
    )?;
    rows.map(|row| {
        let (key, ka, va, kb, vb) = row?;
        Ok((key, decode(ka, va)?, decode(kb, vb)?))
    })
    .collect()
}

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS events (
        seq INTEGER PRIMARY KEY,
        ts  INTEGER NOT NULL
    ) STRICT;
    CREATE TABLE IF NOT EXISTS changes (
        key   TEXT NOT NULL,
        valid INTEGER NOT NULL,
        seq   INTEGER NOT NULL REFERENCES events (seq),
        kind  INTEGER,
        value ANY,
        PRIMARY KEY (key, valid, seq),
        CHECK ((kind IS NULL AND value IS NULL)
            OR (kind IN (0, 1) AND typeof(value) = 'integer')
            OR (kind = 2 AND typeof(value) = 'real')
            OR (kind = 3 AND typeof(value) = 'text'))
    ) STRICT, WITHOUT ROWID;
    CREATE INDEX IF NOT EXISTS events_by_ts ON events (ts);
    CREATE INDEX IF NOT EXISTS changes_by_seq ON changes (seq, key);

    CREATE VIEW IF NOT EXISTS keys (key) AS
    WITH RECURSIVE scan (key) AS (
        SELECT min(key) FROM changes
        UNION ALL
        SELECT (SELECT min(key) FROM changes WHERE key > scan.key)
        FROM scan WHERE scan.key IS NOT NULL
    )
    SELECT key FROM scan WHERE key IS NOT NULL;

    -- each key's currently-believed history as half-open valid-time
    -- intervals; NULL valid_to is open-ended, NULL kind means absent
    CREATE VIEW IF NOT EXISTS timeline AS
    SELECT c.key, c.valid AS valid_from,
           lead(c.valid) OVER (PARTITION BY c.key ORDER BY c.valid) AS valid_to,
           c.kind, c.value, c.seq
    FROM changes c
    WHERE NOT EXISTS (SELECT 1 FROM changes k
                       WHERE k.key = c.key AND k.valid = c.valid
                         AND k.seq > c.seq);

    CREATE VIEW IF NOT EXISTS latest AS
    WITH now (t) AS (SELECT cast(unixepoch('subsec') * 1000000 AS INTEGER))
    SELECT key, kind, value, seq, valid_from AS valid
    FROM timeline, now
    WHERE valid_from <= t AND (valid_to IS NULL OR t < valid_to)
      AND kind IS NOT NULL;

    CREATE VIEW IF NOT EXISTS scheduled AS
    WITH now (t) AS (SELECT cast(unixepoch('subsec') * 1000000 AS INTEGER))
    SELECT key, valid_from AS valid, kind, value, seq
    FROM timeline, now
    WHERE valid_from > t;

    CREATE VIEW IF NOT EXISTS corrections AS
    SELECT c.key, c.seq, c.valid, c.kind, c.value,
           c.valid < e.ts AS backdated,
           EXISTS (SELECT 1 FROM changes p
                    WHERE p.key = c.key AND p.valid = c.valid
                      AND p.seq < c.seq) AS supersedes
    FROM changes c JOIN events e ON e.seq = c.seq
    WHERE c.valid < e.ts
       OR EXISTS (SELECT 1 FROM changes p
                   WHERE p.key = c.key AND p.valid = c.valid
                     AND p.seq < c.seq);

    CREATE VIEW IF NOT EXISTS assertions AS
    SELECT c.seq, c.key,
           CASE c.kind WHEN 0 THEN 'bool' WHEN 1 THEN 'int'
                       WHEN 2 THEN 'float' WHEN 3 THEN 'str'
                       ELSE 'delete' END AS type,
           c.value,
           strftime('%Y-%m-%dT%H:%M:%f', c.valid / 1000000.0, 'unixepoch')
               AS valid,
           strftime('%Y-%m-%dT%H:%M:%f', e.ts / 1000000.0, 'unixepoch') AS ts
    FROM changes c JOIN events e ON e.seq = c.seq
    ORDER BY c.seq, c.key, c.valid;";

pub fn connect(path: impl AsRef<Path>) -> Result<Db<Durable>, Error> {
    Db::create(Connection::open(path)?)
}

pub fn inspect(path: impl AsRef<Path>) -> Result<Db<ReadOnly>, Error> {
    Db::open(
        Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?
    )
}

pub fn in_memory() -> Result<Db<InMemory>, Error> {
    Db::create(Connection::open_in_memory()?)
}
