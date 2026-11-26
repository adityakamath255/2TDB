#!/bin/sh
# run by the postgres image on a fresh volume, after the schema: lets the app log in
set -eu
psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" \
    -v password="$TWO_TDB_APP_PASSWORD" <<'SQL'
ALTER ROLE two_tdb_app LOGIN PASSWORD :'password';
SQL
