CREATE TABLE IF NOT EXISTS events (
    seq INTEGER PRIMARY KEY CHECK (seq > 0),
    ts  INTEGER NOT NULL
) STRICT;

-- kind values match StorageKind in sqlite.rs
CREATE TABLE IF NOT EXISTS changes (
    key   TEXT NOT NULL,
    valid INTEGER NOT NULL,
    seq   INTEGER NOT NULL REFERENCES events (seq),
    kind  INTEGER NOT NULL,
    value ANY,
    PRIMARY KEY (key, valid, seq),
    CHECK ((kind = 0 AND typeof(value) = 'integer' AND value IN (0, 1))
        OR (kind = 1 AND typeof(value) = 'integer')
        OR (kind = 2 AND typeof(value) = 'real')
        OR (kind = 3 AND typeof(value) = 'text')
        OR (kind = 4 AND value IS NULL))
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
-- intervals; NULL valid_to is open-ended, kind 4 means absent
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
  AND kind != 4;

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
                   WHEN 4 THEN 'delete' END AS type,
       c.value,
       strftime('%Y-%m-%dT%H:%M:%f', c.valid / 1000000.0, 'unixepoch') AS valid,
       strftime('%Y-%m-%dT%H:%M:%f', e.ts / 1000000.0, 'unixepoch') AS ts
FROM changes c JOIN events e ON e.seq = c.seq
ORDER BY c.seq, c.key, c.valid;
