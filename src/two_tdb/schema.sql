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
