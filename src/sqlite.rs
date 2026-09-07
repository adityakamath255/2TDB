use std::path::Path;

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior};

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
        let rows = statement.query_map([id], |row| {
            Ok(RawAssertion {
                key: row.get(0)?,
                valid_from: row.get(1)?,
                value: RawValue::read(row, 2, 3)?,
            })
        })?;
        let assertions = rows
            .map(|row| row?.decode())
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
            "SELECT c.seq, e.ts, c.key, c.valid, c.kind, c.value
             FROM changes c JOIN events e ON e.seq = c.seq
             WHERE c.key = ?1 ORDER BY c.seq, c.valid",
        )?;
        let rows = statement.query_map([key], RawRecordedAssertion::read)?;
        rows.map(|row| row?.decode()).collect()
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
                let stored = RawValue::encode(assertion.value);
                statement.execute((
                    id,
                    assertion.key,
                    assertion.valid_from,
                    stored.kind,
                    stored.value,
                ))?;
            }
        }
        transaction.commit()?;
        Ok(id)
    }

    pub(crate) fn get(&self, coordinate: Coordinate, key: &str) -> Result<Option<Value>, Error> {
        let stored = self
            .connection
            .query_row(
                "SELECT kind, value FROM changes
                 WHERE key = ?1
                   AND (?2 IS NULL OR valid <= ?2)
                   AND seq <= ?3
                 ORDER BY valid DESC, seq DESC LIMIT 1",
                (
                    key,
                    coordinate.valid.micros(),
                    coordinate.transaction.event_id(),
                ),
                |row| RawValue::read(row, 0, 1),
            )
            .optional()?;
        stored.map_or(Ok(None), RawValue::decode)
    }

    pub(crate) fn blame(
        &self,
        coordinate: Coordinate,
        key: &str,
    ) -> Result<Option<RecordedAssertion>, Error> {
        let raw = self
            .connection
            .query_row(
                "SELECT c.seq, e.ts, c.key, c.valid, c.kind, c.value
                 FROM changes c JOIN events e ON e.seq = c.seq
                 WHERE c.key = ?1
                   AND (?2 IS NULL OR c.valid <= ?2)
                   AND c.seq <= ?3
                 ORDER BY c.valid DESC, c.seq DESC LIMIT 1",
                (
                    key,
                    coordinate.valid.micros(),
                    coordinate.transaction.event_id(),
                ),
                RawRecordedAssertion::read,
            )
            .optional()?;
        raw.map(RawRecordedAssertion::decode).transpose()
    }

    pub(crate) fn changepoints(
        &self,
        transaction: TransactionCutoff,
    ) -> Result<Vec<Timestamp>, Error> {
        let mut statement = self
            .connection
            .prepare("SELECT DISTINCT valid FROM changes WHERE seq <= ?1 ORDER BY valid")?;
        let rows = statement.query_map([transaction.event_id()], |row| row.get::<_, i64>(0))?;
        rows.map(|row| timestamp(row?)).collect()
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
        let rows = statement.query_map(
            (
                coordinate.valid.micros(),
                coordinate.transaction.event_id(),
                StorageKind::Delete as i64,
            ),
            |row| Ok((row.get::<_, String>(0)?, RawValue::read(row, 1, 2)?)),
        )?;
        rows.map(|row| {
            let (key, value) = row?;
            Ok((key, value.decode_present()?))
        })
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

struct RawValue {
    kind: i64,
    value: SqlValue,
}

impl RawValue {
    fn read(row: &rusqlite::Row<'_>, kind: usize, value: usize) -> rusqlite::Result<Self> {
        Ok(RawValue {
            kind: row.get(kind)?,
            value: row.get(value)?,
        })
    }

    fn encode(value: Option<Value>) -> Self {
        let (kind, value) = match value {
            Some(Value::Bool(value)) => (StorageKind::Bool, SqlValue::Integer(value.into())),
            Some(Value::Int(value)) => (StorageKind::Int, SqlValue::Integer(value)),
            Some(Value::Float(value)) => (StorageKind::Float, SqlValue::Real(value)),
            Some(Value::Str(value)) => (StorageKind::Str, SqlValue::Text(value)),
            None => (StorageKind::Delete, SqlValue::Null),
        };
        RawValue {
            kind: kind as i64,
            value,
        }
    }

    fn decode(self) -> Result<Option<Value>, Error> {
        match (StorageKind::try_from(self.kind)?, self.value) {
            (StorageKind::Bool, SqlValue::Integer(0)) => Ok(Some(Value::Bool(false))),
            (StorageKind::Bool, SqlValue::Integer(1)) => Ok(Some(Value::Bool(true))),
            (StorageKind::Int, SqlValue::Integer(value)) => Ok(Some(Value::Int(value))),
            (StorageKind::Float, SqlValue::Real(value)) => Ok(Some(Value::Float(value))),
            (StorageKind::Str, SqlValue::Text(value)) => Ok(Some(Value::Str(value))),
            (StorageKind::Delete, SqlValue::Null) => Ok(None),
            _ => Err(Error::Corrupt),
        }
    }

    fn decode_present(self) -> Result<Value, Error> {
        self.decode()?.ok_or(Error::Corrupt)
    }
}

struct RawAssertion {
    key: String,
    valid_from: i64,
    value: RawValue,
}

impl RawAssertion {
    fn decode(self) -> Result<Assertion, Error> {
        Ok(Assertion {
            key: self.key,
            valid_from: timestamp(self.valid_from)?,
            value: self.value.decode()?,
        })
    }
}

struct RawRecordedAssertion {
    event_id: EventId,
    committed_at: i64,
    assertion: RawAssertion,
}

impl RawRecordedAssertion {
    fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(RawRecordedAssertion {
            event_id: row.get(0)?,
            committed_at: row.get(1)?,
            assertion: RawAssertion {
                key: row.get(2)?,
                valid_from: row.get(3)?,
                value: RawValue::read(row, 4, 5)?,
            },
        })
    }

    fn decode(self) -> Result<RecordedAssertion, Error> {
        Ok(RecordedAssertion {
            event_id: self.event_id,
            committed_at: timestamp(self.committed_at)?,
            assertion: self.assertion.decode()?,
        })
    }
}

fn timestamp(micros: i64) -> Result<Timestamp, Error> {
    Timestamp::from_microsecond(micros).map_err(|_| Error::Corrupt)
}
