-- run by the database owner on an empty database. the app connects as
-- two_tdb_app, which may only read and append, and only within the tenant
-- named by the transaction-local setting app.tenant.

DO $$
BEGIN
    CREATE ROLE two_tdb_app NOLOGIN;
EXCEPTION WHEN duplicate_object THEN
    NULL;
END
$$;

CREATE FUNCTION current_tenant() RETURNS bigint
LANGUAGE sql STABLE
RETURN nullif(current_setting('app.tenant', true), '')::bigint;

CREATE TABLE tenants (
    id   bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name text NOT NULL UNIQUE CHECK (name <> '')
);

-- seq numbers each tenant's events 1, 2, 3, ... without gaps; commits take
-- the next number under a per-tenant lock, so numbers follow commit order
CREATE TABLE events (
    tenant       bigint NOT NULL DEFAULT current_tenant() REFERENCES tenants,
    seq          bigint NOT NULL CHECK (seq > 0),
    committed_at timestamptz NOT NULL CHECK (isfinite(committed_at)),
    source       text NOT NULL CHECK (source <> ''),
    PRIMARY KEY (tenant, seq)
);

CREATE INDEX events_by_time ON events (tenant, committed_at);

CREATE EXTENSION IF NOT EXISTS vector;

CREATE TYPE value_kind AS ENUM ('bool', 'int', 'float', 'str', 'delete');

CREATE TABLE changes (
    tenant      bigint NOT NULL DEFAULT current_tenant(),
    -- byte order, so key order in SQL matches Python's string order
    key         text COLLATE "C" NOT NULL CHECK (key <> ''),
    valid       timestamptz NOT NULL CHECK (isfinite(valid)),
    seq         bigint NOT NULL,
    kind        value_kind NOT NULL,
    bool_value  boolean,
    int_value   bigint,
    float_value double precision
        CHECK (float_value NOT IN ('NaN', 'Infinity', '-Infinity')),
    str_value   text,
    -- "key: value" embedded by two_tdb.embedding.MODEL; null for a deletion
    embedding   vector(384) CHECK ((kind = 'delete') = (embedding IS NULL)),
    PRIMARY KEY (tenant, key, valid, seq),
    FOREIGN KEY (tenant, seq) REFERENCES events,
    CHECK ((kind = 'bool') = (bool_value IS NOT NULL)
       AND (kind = 'int') = (int_value IS NOT NULL)
       AND (kind = 'float') = (float_value IS NOT NULL)
       AND (kind = 'str') = (str_value IS NOT NULL))
);

CREATE INDEX changes_by_seq ON changes (tenant, seq);

ALTER TABLE events ENABLE ROW LEVEL SECURITY;
ALTER TABLE changes ENABLE ROW LEVEL SECURITY;
CREATE POLICY own_tenant ON events USING (tenant = current_tenant());
CREATE POLICY own_tenant ON changes USING (tenant = current_tenant());

GRANT SELECT, INSERT ON events, changes TO two_tdb_app;

-- API keys are stored as SHA-256 hashes; a key is shown once, by
-- create_api_key. the app can only resolve a key, through api_key_tenant.
CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TABLE api_keys (
    id       bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant   bigint NOT NULL REFERENCES tenants,
    writable boolean NOT NULL,
    hash     bytea NOT NULL UNIQUE
);

CREATE FUNCTION create_api_key(tenant bigint, writable boolean) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
    key text := '2tdb_' || encode(gen_random_bytes(32), 'hex');
BEGIN
    INSERT INTO api_keys (tenant, writable, hash)
    VALUES (tenant, writable, sha256(convert_to(key, 'UTF8')));
    RETURN key;
END
$$;

REVOKE EXECUTE ON FUNCTION create_api_key FROM PUBLIC;

CREATE FUNCTION api_key_tenant(
    key text, OUT tenant bigint, OUT writable boolean
)
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public, pg_temp
RETURN (
    SELECT (k.tenant, k.writable) FROM api_keys k
    WHERE k.hash = sha256(convert_to(key, 'UTF8'))
);

REVOKE EXECUTE ON FUNCTION api_key_tenant FROM PUBLIC;
GRANT EXECUTE ON FUNCTION api_key_tenant TO two_tdb_app;
