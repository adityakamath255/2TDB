# 2TDB

2TDB is a bitemporal key-value store served over HTTP. It records when each value was true (valid time) and when the store learned it (transaction time), so it can answer both "what was true on 1 March?" and "what did we believe on 5 March?". History is append-only: a correction is a new assertion, and earlier answers stay reproducible. It runs on Postgres, keeps each tenant's history apart with row-level security, and searches facts by meaning with pgvector. The repository also holds a customer-support agent on Claude Haiku 5.5 whose only memory is a 2TDB store, and an evaluation comparing it with plain retrieval-augmented generation.

The earlier Rust library, an embedded store on SQLite, is on the [`rust`](https://github.com/adityakamath255/2TDB/tree/rust) branch (tag `rust-0.1.0`). Its predecessor in Python is [TTDB](https://github.com/adityakamath255/TTDB).

## Demo

A read-only copy runs at https://2tdb.duckdns.org. It holds the evaluation's customer: the agent read the 26 scenario messages and recorded 24 events, including corrected details and a back-dated move. The page at https://adityakamath255.github.io/2TDB/ charts each key over valid time and events, and queries the API at the point you pick. To call the API yourself, open https://2tdb.duckdns.org/docs, choose Authorize and paste the key below, or use curl:

```sh
KEY=2tdb_6a596f0e533c047d5e5828f6de4c398934073dd02a8c2f84ee7851a603b21f10
curl -H "Authorization: Bearer $KEY" https://2tdb.duckdns.org/state
curl -H "Authorization: Bearer $KEY" "https://2tdb.duckdns.org/history?key=email"
curl -H "Authorization: Bearer $KEY" "https://2tdb.duckdns.org/value?key=city&valid_at=2026-03-01T00:00:00Z"
```

The key reads only the demo tenant and can't commit. Valid times follow the messages' dates; commit times are when the seed ran, on 27 November 2026.

## Usage

Start Postgres and the API, then create a tenant and a key:

```sh
docker compose up -d --wait
docker compose exec db psql -U postgres -d two_tdb -c "INSERT INTO tenants (name) VALUES ('acme')"
docker compose exec db psql -U postgres -d two_tdb -Atc "SELECT create_api_key(1, true)"
```

`create_api_key(tenant, writable)` prints the key once; the database keeps only its SHA-256 hash. A key made with `writable` false can read but not commit.

Record that a customer lived in Mumbai from January and moved to Pune in October:

```sh
curl -X POST localhost:8000/events -H "Authorization: Bearer $KEY" -H 'content-type: application/json' -d '{
  "source": "msg-1",
  "writes": [
    {"key": "city", "value": "Mumbai", "valid_from": "2026-01-01T00:00:00Z"},
    {"key": "city", "value": "Pune", "valid_from": "2026-10-01T00:00:00Z"}
  ]}'
```

Read it back now, and at a valid time in June:

```sh
curl "localhost:8000/value?key=city" -H "Authorization: Bearer $KEY"
# {"snapshot":{"event":1,"valid_at":"2026-11-26T08:03:32.432645Z"},"key":"city","value":"Pune"}
curl "localhost:8000/value?key=city&valid_at=2026-06-01T00:00:00Z" -H "Authorization: Bearer $KEY"
# {"snapshot":{"event":1,"valid_at":"2026-06-01T00:00:00Z"},"key":"city","value":"Mumbai"}
```

Every read of a snapshot returns its coordinates, and passing those coordinates back repeats the read exactly. The interactive API documentation is at `localhost:8000/docs`.

## Model

A store is a log of events. Each event has a number (1, 2, 3, ... per tenant, without gaps), the time it was committed, a source naming where it came from, and one or more assertions. An assertion sets a key to a value, or deletes it, from a valid time on. Values are booleans, 64-bit integers, finite floats or strings.

A snapshot fixes both axes: which events count (all of them, the first N, or those committed by a wall-clock time) and the valid time to read at. Within a snapshot, a key's value comes from the assertion with the latest valid time not after the read time, and among assertions with the same valid time, the one committed last. So a change ("moved to Pune in October") adds a later valid time and leaves the past alone, while a correction ("the renewal is 1 April, not 1 March") re-asserts the same valid time and wins because it was recorded later. The superseded assertion stays in the history.

## API

Reads take a snapshot as query parameters: `event` or `known_at` for the transaction axis (default: the latest event), and `valid_at` for the valid axis (default: the time the snapshot was taken). Timestamps must carry a UTC offset, and unknown parameters are rejected.

| Endpoint | Returns |
|---|---|
| `POST /events` | commits writes as one event: `{"source", "writes": [{"key", "value", "valid_from"}]}`; a `null` value deletes |
| `GET /events/{id}` | an event with its assertions |
| `GET /value?key=` | a key's value at a snapshot |
| `GET /blame?key=` | the assertion that decides that value, with its event and source |
| `GET /history?key=` | every assertion ever made for a key, in commit order |
| `GET /state` | every key's value at a snapshot |
| `GET /search?q=&limit=` | the values at a snapshot closest in meaning to `q` |
| `GET /changepoints` | every valid time at which some key changes |
| `GET /diff` | how two snapshots differ, given as `from_event`, `to_valid_at`, and so on |
| `GET /keys` | every key ever asserted |

The Python library underneath (`two_tdb.Store` and the read-only `two_tdb.Reader`) offers the same operations, plus `bisect`, which finds the first event after which a predicate on the snapshot holds.

## Storage

Events and assertions live in two append-only tables. The service logs in as a role that may only select and insert, and only within the tenant set for the transaction, so a missing filter can't leak another tenant's rows. Commits for one tenant take turns under an advisory lock, which keeps event numbers in commit order and commit times non-decreasing. Values sit in typed columns with CHECK constraints, so the database can't hold a value that contradicts its type.

Each assertion's text (`key: value`) is embedded with all-MiniLM-L6-v2 when it is committed. Search ranks only the assertions that decide each key at the snapshot, so it can search what was known at any point.

## Agent and evaluation

`two_tdb.agent` is a support agent built with LangChain's `create_agent` on Claude Haiku 5.5. It handles each message without chat history: it reads the customer's facts through tools (`facts`, `lookup`, `blame`, `history`, `search`) and records new ones with `remember`, which commits with the message ID as the source. Facts are restricted to a fixed list of typed attributes plus free-text notes.

The evaluation feeds 26 messages from one customer, January to October 2026, to the agent and to a baseline that stores the raw messages and answers from the five closest by meaning. The messages include changes, back-dated and future-dated facts, and eight corrections. Both then answer 24 questions: about current facts, facts at an earlier date, and what the records said before a correction. Answers are compared exactly after normalising case, spacing and punctuation.

| System | current | past | corrected | All | Median s per question | $ per question |
|---|---|---|---|---|---|---|
| 2TDB agent | 24/24 | 24/24 | 24/24 | 72/72 | 2.0 | 0.00048 |
| RAG baseline | 24/24 | 15/24 | 18/24 | 57/72 | 1.0 | 0.00008 |

Counts are over three repeats (26 Nov 2026). Every baseline miss was "unknown" on one of five questions whose answers sit in January messages that five-message retrieval never returned. Run it with:

```sh
docker compose up -d --wait db
ANTHROPIC_API_KEY=... uv run python -m evaluation.run
```

## Deployment

The demo runs on Google Cloud's free-tier e2-micro VM (2 shared vCPUs, 1 GB of memory, a 30 GB standard disk) in us-central1, with Debian 13, Docker from Docker's own repository and a 2 GB swap file. It runs the same `compose.yaml` with the `public` profile, which adds Caddy to serve HTTPS and renew the certificate. A `.env` file next to `compose.yaml` sets:

```sh
COMPOSE_PROFILES=public
DOMAIN=2tdb.duckdns.org
POSTGRES_PASSWORD=...
TWO_TDB_APP_PASSWORD=...
TWO_TDB_CORS_ORIGIN=https://adityakamath255.github.io
```

`TWO_TDB_CORS_ORIGIN` lets the demo page, which GitHub Pages serves from `docs/` on `main`, call the API from a browser. The page quotes each fact's source message from `docs/messages.json`, a copy of the scenario's messages that a test keeps equal to them.

Then `docker compose up -d --build` starts it, and `git pull` with the same command updates it. The firewall opens ports 80 and 443; Postgres listens only on the VM itself. Building the image on the VM took over 10 minutes and peaked at 642 MB of memory; running, the three containers leave about 290 MB free, with 150 MB in swap.

The demo tenant was seeded from a laptop through an SSH tunnel to the VM's Postgres, so the Anthropic key never reached the VM:

```sh
gcloud compute ssh twotdb -- -N -L 15432:127.0.0.1:5432 &
TWO_TDB_DATABASE_URL=postgresql://two_tdb_app:...@127.0.0.1:15432/two_tdb \
    ANTHROPIC_API_KEY=... uv run python -m evaluation.seed 1
```

## Tests

```sh
docker compose up -d --wait db
uv run pytest
```

The tests drop and recreate a database named `two_tdb_test` on that server, and run the agent against a scripted model, so they need no API key.

CI runs the same tests on pushes to `main` and on pull requests, after `uv run ruff format --check .`, `uv run ruff check .` and `uv run pyright`.

## Limitations

- Search ranks every key's current fact without an index, so its cost grows with the number of keys in a tenant, like `state` (about 20 ms at 1,000 keys).
- Commits have no idempotency keys. A client that retries a commit after a timeout may record it twice; reads are unchanged unless another commit to the same key landed in between.
- One tenant's commits run one at a time, so a stuck writer holds up that tenant until it times out.
- The evaluation is one scripted customer and 24 questions. Its baseline retrieves five messages; with this few messages, putting all of them in the prompt would be a stronger baseline.
- The agent's facts come from a fixed attribute list; anything else goes into free-text notes.
- The demo is one small VM with no backups and no rate limit, so anyone with the public key can read the demo tenant as often as they like.
