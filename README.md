# 2TDB

2TDB is a bitemporal key-value store. It records when each value was true (valid time) and when the store learned it (transaction time), so it can answer both "what was true on 1 March?" and "what did we believe on 5 March?". History is append-only: a correction is a new assertion, and earlier answers stay reproducible. This version runs on Postgres, and row-level security keeps each tenant's history separate.

The earlier Rust library, an embedded store on SQLite, is on the [`rust`](https://github.com/adityakamath255/2TDB/tree/rust) branch (tag `rust-0.1.0`). Its predecessor in Python is [TTDB](https://github.com/adityakamath255/TTDB).

## Development

```sh
docker compose up -d db
uv run pytest
```

The tests drop and recreate a database named `two_tdb_test` on that server.
