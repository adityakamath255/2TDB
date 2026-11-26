use std::path::Path;

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior};

use crate::Timestamp;
use crate::database::{
    Assertion, Coordinate, Error, Event, EventId, RecordedAssertion, State, TransactionCutoff,
    Value, Write, resolve_writes,
};

const SCHEMA: &str = include_str!("schema.sql");

pub(crate) struct Sqlite {
    connection: Connection,
}

impl Sqlite {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let sqlite = Sqlite::create(Connection::open(path)?)?;
        sqlite
            .connection
            .pragma_update(None, "journal_mode", "WAL")?;
        Ok(sqlite)
    }

    pub(crate) fn inspect(path: impl AsRef<Path>) -> Result<Self, Error> {
        Sqlite::configured(Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?)
    }

    pub(crate) fn memory() -> Result<Self, Error> {
        Sqlite::create(Connection::open_in_memory()?)
    }

    fn create(connection: Connection) -> Result<Self, Error> {
        let sqlite = Sqlite::configured(connection)?;
        sqlite.connection.execute_batch(SCHEMA)?;
        Ok(sqlite)
    }

    fn configured(connection: Connection) -> Result<Self, Error> {
        connection.execute_batch("PRAGMA busy_timeout = 5000; PRAGMA foreign_keys = ON")?;
        Ok(Sqlite { connection })
    }

    pub(crate) fn len(&self) -> Result<u64, Error> {
        self.connection
            .query_row("SELECT count(*) FROM events", [], |row| row.get(0))
            .map_err(Error::from)
    }

    pub(crate) fn latest_event_id(&self) -> Result<Option<EventId>, Error> {
        self.connection
            .query_row("SELECT max(seq) FROM events", [], |row| row.get(0))
            .map_err(Error::from)
    }

    pub(crate) fn event_timestamp(&self, id: EventId) -> Result<Timestamp, Error> {
        let micros = self
            .connection
            .query_row("SELECT ts FROM events WHERE seq = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        match micros {
            Some(micros) => timestamp(micros),
            None => Err(Error::OutOfRange {
                id,
                len: self.len()?,
            }),
        }
    }

    pub(crate) fn event_id_at(&self, time: Timestamp) -> Result<Option<EventId>, Error> {
        self.connection
            .query_row(
                "SELECT seq FROM events WHERE ts <= ?1
                 ORDER BY ts DESC, seq DESC LIMIT 1",
                [time.as_microsecond()],
                |row| row.get(0),
            )
            .optional()
            .map_err(Error::from)
    }

    pub(crate) fn event(&self, id: EventId) -> Result<Event, Error> {
        let committed_at = self.event_timestamp(id)?;
        let mut statement = self.connection.prepare(
            "SELECT key, valid, kind, value FROM changes
             WHERE seq = ?1 ORDER BY key, valid",
        )?;
        let assertions = statement
            .query_and_then([id], read_assertion)?
            .collect::<Result<_, Error>>()?;
        Ok(Event {
            id,
            committed_at,
            assertions,
        })
    }

    pub(crate) fn keys(&self) -> Result<Vec<String>, Error> {
        let mut statement = self.connection.prepare("SELECT key FROM keys")?;
        let rows = statement.query_map([], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<_>>().map_err(Error::from)
    }

    pub(crate) fn history(&self, key: &str) -> Result<Vec<RecordedAssertion>, Error> {
        let mut statement = self.connection.prepare(
            "SELECT c.key, c.valid, c.kind, c.value, c.seq, e.ts
             FROM changes c JOIN events e ON e.seq = c.seq
             WHERE c.key = ?1 ORDER BY c.seq, c.valid",
        )?;
        statement
            .query_and_then([key], read_recorded_assertion)?
            .collect()
    }

    pub(crate) fn commit(&mut self, writes: Vec<Write>) -> Result<EventId, Error> {
        if writes.is_empty() {
            return Err(Error::EmptyCommit);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let committed_at = Timestamp::now();
        let id = transaction.query_row(
            "INSERT INTO events (ts) VALUES (?1) RETURNING seq",
            [committed_at.as_microsecond()],
            |row| row.get(0),
        )?;
        let assertions = resolve_writes(writes, committed_at);
        {
            let mut statement = transaction.prepare(
                "INSERT INTO changes (seq, key, valid, kind, value)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for assertion in assertions {
                let (kind, value) = encode_value(assertion.value);
                statement.execute((id, assertion.key, assertion.valid_from, kind, value))?;
            }
        }
        transaction.commit()?;
        Ok(id)
    }

    pub(crate) fn get(&self, coordinate: Coordinate, key: &str) -> Result<Option<Value>, Error> {
        let mut statement = self.connection.prepare(
            "SELECT kind, value FROM changes
             WHERE key = ?1
               AND (?2 IS NULL OR valid <= ?2)
               AND seq <= ?3
             ORDER BY valid DESC, seq DESC LIMIT 1",
        )?;
        let mut rows = statement.query_and_then(
            (
                key,
                coordinate.valid.micros(),
                coordinate.transaction.event_id(),
            ),
            |row| read_value(row, 0, 1),
        )?;
        rows.next().transpose().map(Option::flatten)
    }

    pub(crate) fn blame(
        &self,
        coordinate: Coordinate,
        key: &str,
    ) -> Result<Option<RecordedAssertion>, Error> {
        let mut statement = self.connection.prepare(
            "SELECT c.key, c.valid, c.kind, c.value, c.seq, e.ts
             FROM changes c JOIN events e ON e.seq = c.seq
             WHERE c.key = ?1
               AND (?2 IS NULL OR c.valid <= ?2)
               AND c.seq <= ?3
             ORDER BY c.valid DESC, c.seq DESC LIMIT 1",
        )?;
        let mut rows = statement.query_and_then(
            (
                key,
                coordinate.valid.micros(),
                coordinate.transaction.event_id(),
            ),
            read_recorded_assertion,
        )?;
        rows.next().transpose()
    }

    pub(crate) fn changepoints(
        &self,
        transaction: TransactionCutoff,
    ) -> Result<Vec<Timestamp>, Error> {
        let mut statement = self
            .connection
            .prepare("SELECT DISTINCT valid FROM changes WHERE seq <= ?1 ORDER BY valid")?;
        statement
            .query_and_then([transaction.event_id()], |row| timestamp(row.get(0)?))?
            .collect()
    }

    pub(crate) fn state(&self, coordinate: Coordinate) -> Result<State, Error> {
        let mut statement = self.connection.prepare(
            "SELECT c.key, c.kind, c.value
             FROM keys k
             JOIN changes c ON (c.key, c.valid, c.seq) =
                 (SELECT w.key, w.valid, w.seq FROM changes w
                  WHERE w.key = k.key
                    AND (?1 IS NULL OR w.valid <= ?1)
                    AND w.seq <= ?2
                  ORDER BY w.valid DESC, w.seq DESC LIMIT 1)
             WHERE c.kind != ?3
             ORDER BY c.key",
        )?;
        statement
            .query_and_then(
                (
                    coordinate.valid.micros(),
                    coordinate.transaction.event_id(),
                    StorageKind::Delete as i64,
                ),
                |row| {
                    let value = read_value(row, 1, 2)?.ok_or(Error::Corrupt)?;
                    Ok((row.get(0)?, value))
                },
            )?
            .collect()
    }

    pub(crate) fn close(self) -> Result<(), Error> {
        self.connection
            .close()
            .map_err(|(_, error)| Error::Sqlite(error))
    }
}

#[repr(i64)]
#[derive(Clone, Copy)]
enum StorageKind {
    Bool = 0,
    Int = 1,
    Float = 2,
    Str = 3,
    Delete = 4,
}

impl TryFrom<i64> for StorageKind {
    type Error = Error;

    fn try_from(raw: i64) -> Result<Self, Self::Error> {
        match raw {
            0 => Ok(Self::Bool),
            1 => Ok(Self::Int),
            2 => Ok(Self::Float),
            3 => Ok(Self::Str),
            4 => Ok(Self::Delete),
            _ => Err(Error::Corrupt),
        }
    }
}

fn encode_value(value: Option<Value>) -> (i64, SqlValue) {
    let (kind, value) = match value {
        Some(Value::Bool(value)) => (StorageKind::Bool, SqlValue::Integer(value.into())),
        Some(Value::Int(value)) => (StorageKind::Int, SqlValue::Integer(value)),
        Some(Value::Float(value)) => (StorageKind::Float, SqlValue::Real(value)),
        Some(Value::Str(value)) => (StorageKind::Str, SqlValue::Text(value)),
        None => (StorageKind::Delete, SqlValue::Null),
    };
    (kind as i64, value)
}

fn read_value(row: &Row<'_>, kind: usize, value: usize) -> Result<Option<Value>, Error> {
    let kind = StorageKind::try_from(row.get::<_, i64>(kind)?)?;
    match (kind, row.get::<_, SqlValue>(value)?) {
        (StorageKind::Bool, SqlValue::Integer(0)) => Ok(Some(Value::Bool(false))),
        (StorageKind::Bool, SqlValue::Integer(1)) => Ok(Some(Value::Bool(true))),
        (StorageKind::Int, SqlValue::Integer(value)) => Ok(Some(Value::Int(value))),
        (StorageKind::Float, SqlValue::Real(value)) => Ok(Some(Value::Float(value))),
        (StorageKind::Str, SqlValue::Text(value)) => Ok(Some(Value::Str(value))),
        (StorageKind::Delete, SqlValue::Null) => Ok(None),
        _ => Err(Error::Corrupt),
    }
}

// expects columns: key, valid, kind, value
fn read_assertion(row: &Row<'_>) -> Result<Assertion, Error> {
    Ok(Assertion {
        key: row.get(0)?,
        valid_from: timestamp(row.get(1)?)?,
        value: read_value(row, 2, 3)?,
    })
}

// expects read_assertion's columns, then: seq, ts
fn read_recorded_assertion(row: &Row<'_>) -> Result<RecordedAssertion, Error> {
    Ok(RecordedAssertion {
        event_id: row.get(4)?,
        committed_at: timestamp(row.get(5)?)?,
        assertion: read_assertion(row)?,
    })
}

fn timestamp(micros: i64) -> Result<Timestamp, Error> {
    Timestamp::from_microsecond(micros).map_err(|_| Error::Corrupt)
}
