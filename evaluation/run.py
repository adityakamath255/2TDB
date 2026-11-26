"""Runs the evaluation and prints a table of results.

    docker compose up -d db
    ANTHROPIC_API_KEY=... uv run python -m evaluation.run

Each repeat feeds the scenario's messages to the 2TDB agent and to the baseline,
then asks both every question. Results go to evaluation/results/<time>.json.
"""

import argparse
import json
import os
import re
import statistics
import time
from collections.abc import Callable, Iterator
from contextlib import contextmanager
from dataclasses import asdict, dataclass
from datetime import UTC, datetime
from pathlib import Path

import anthropic
import psycopg
from psycopg import sql

from evaluation import baseline
from evaluation.scenario import MESSAGES, QUESTIONS, Kind
from two_tdb import Store, agent, install
from two_tdb.agent import Message, Reply
from two_tdb.embedding import Embedder
from two_tdb.store import Embed

SERVER = os.environ.get("TWO_TDB_EVAL_SERVER", "postgresql://postgres:postgres@localhost:5432")
DATABASE = "two_tdb_eval"
RESULTS = Path(__file__).parent / "results"

# Claude Haiku 5.5, dollars per million tokens
INPUT_PRICE, OUTPUT_PRICE = 0.10, 0.50

type Responder = Callable[[Message], Reply]


def _normalize(text: str) -> str:
    return re.sub(r"\s+", "", text).strip("\"'`.").removesuffix("%").lower()


def correct(answer: str, expected: str) -> bool:
    """Exact match after dropping whitespace, case, quotes, a final period and a percent sign."""
    answer, expected = _normalize(answer), _normalize(expected)
    try:
        return float(answer) == float(expected)
    except ValueError:
        return answer == expected


@dataclass(frozen=True)
class Result:
    system: str
    repeat: int
    question: str
    kind: Kind
    answer: str
    correct: bool
    seconds: float
    input_tokens: int
    output_tokens: int


@dataclass(frozen=True)
class Ingest:
    system: str
    repeat: int
    input_tokens: int
    output_tokens: int


def _timed(respond: Responder, message: Message) -> tuple[Reply, float]:
    start = time.perf_counter()
    reply = respond(message)
    return reply, time.perf_counter() - start


def run_system(name: str, repeat: int, ingest: Responder, ask: Responder) -> tuple[Ingest, list[Result]]:
    """Feeds every scenario message to ingest, then asks every question."""
    ingested = [ingest(message) for message in MESSAGES]
    results = []
    for question in QUESTIONS:
        reply, seconds = _timed(ask, question.message())
        results.append(Result(
            name, repeat, question.id, question.kind, reply.text,
            correct(reply.text, question.expected), seconds, reply.input_tokens, reply.output_tokens,
        ))
    usage = Ingest(name, repeat, sum(r.input_tokens for r in ingested), sum(r.output_tokens for r in ingested))
    return usage, results


@contextmanager
def _stores(embed: Embed) -> Iterator[Callable[[], Store]]:
    """Recreates the evaluation database and yields a function making a store for a fresh tenant."""
    with psycopg.connect(f"{SERVER}/postgres", autocommit=True) as admin:
        admin.execute(sql.SQL("DROP DATABASE IF EXISTS {} WITH (FORCE)").format(sql.Identifier(DATABASE)))
        admin.execute(sql.SQL("CREATE DATABASE {}").format(sql.Identifier(DATABASE)))
    with psycopg.connect(f"{SERVER}/{DATABASE}", autocommit=True) as owner:
        install(owner)
        opened: list[psycopg.Connection] = []

        def new_store() -> Store:
            (tenant,) = owner.execute(
                "INSERT INTO tenants (name) VALUES (%s) RETURNING id", [f"eval-{len(opened)}"]
            ).fetchone()
            connection = psycopg.connect(f"{SERVER}/{DATABASE}", autocommit=True)
            connection.execute("SET ROLE two_tdb_app")
            opened.append(connection)
            return Store(connection, tenant, embed)

        try:
            yield new_store
        finally:
            for connection in opened:
                connection.close()


def evaluate(repeats: int, new_store: Callable[[], Store], two_tdb: Callable[[Store], Responder],
             rag: Callable[[], tuple[Responder, Responder]]) -> tuple[list[Ingest], list[Result]]:
    ingests, results = [], []
    for repeat in range(1, repeats + 1):
        respond = two_tdb(new_store())
        ingest, answers = run_system("2TDB agent", repeat, respond, respond)
        ingests.append(ingest)
        results += answers
        notes, ask = rag()
        ingest, answers = run_system("RAG baseline", repeat, notes, ask)
        ingests.append(ingest)
        results += answers
    return ingests, results


def report(ingests: list[Ingest], results: list[Result]) -> str:
    """Returns a Markdown table: accuracy per kind, median latency and cost per question, ingest cost."""
    systems = list(dict.fromkeys(r.system for r in results))
    kinds = list(dict.fromkeys(r.kind for r in results))
    repeats = len({r.repeat for r in results})
    lines = [
        "| System | " + " | ".join(kinds) + " | All | Median s per question | $ per question | $ to ingest |",
        "|---" * (len(kinds) + 5) + "|",
    ]
    for system in systems:
        mine = [r for r in results if r.system == system]
        cells = []
        for kind in kinds + [None]:
            subset = [r for r in mine if kind is None or r.kind == kind]
            cells.append(f"{sum(r.correct for r in subset)}/{len(subset)}")
        cost = sum(r.input_tokens * INPUT_PRICE + r.output_tokens * OUTPUT_PRICE for r in mine) / 1e6 / len(mine)
        ingest = [i for i in ingests if i.system == system]
        ingest_cost = sum(i.input_tokens * INPUT_PRICE + i.output_tokens * OUTPUT_PRICE for i in ingest) / 1e6 / len(ingest)
        lines.append(
            f"| {system} | " + " | ".join(cells)
            + f" | {statistics.median(r.seconds for r in mine):.1f} | {cost:.5f} | {ingest_cost:.4f} |"
        )
    lines.append(f"\nCounts are correct answers over {repeats} repeats; costs are per repeat at "
                 f"${INPUT_PRICE}/${OUTPUT_PRICE} per million input/output tokens.")
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--repeats", type=int, default=3)
    args = parser.parse_args()

    embed = Embedder()
    client = anthropic.Anthropic()
    graph = agent.build(agent.chat_model())

    def two_tdb(store: Store) -> Responder:
        return lambda message: agent.respond(graph, store, message)

    def rag() -> tuple[Responder, Responder]:
        notes = baseline.Notes(embed)

        def ingest(message: Message) -> Reply:
            notes.add(message)
            return Reply("", 0, 0, 0)

        return ingest, lambda question: baseline.answer(client, notes, question)

    with _stores(embed) as new_store:
        ingests, results = evaluate(args.repeats, new_store, two_tdb, rag)
    table = report(ingests, results)
    RESULTS.mkdir(exist_ok=True)
    path = RESULTS / f"{datetime.now(UTC):%Y-%m-%dT%H%M}.json"
    path.write_text(json.dumps(
        {"ingests": [asdict(i) for i in ingests], "results": [asdict(r) for r in results], "table": table},
        indent=1,
    ))
    print(table)
    print(f"\nwrote {path}")


if __name__ == "__main__":
    main()
