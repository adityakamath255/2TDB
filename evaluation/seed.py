"""Fills a tenant with what the agent records from the scenario's messages.

    ANTHROPIC_API_KEY=... PY2TDB_DATABASE_URL=... \\
        uv run python -m evaluation.seed TENANT

PY2TDB_DATABASE_URL must log in as pg2tdb_app. The tenant must have no
events, so a seed that stopped partway is never mixed with another.
"""

import argparse
import os

import psycopg

from evaluation.scenario import MESSAGES
from py2tdb import Store, agent
from py2tdb.embedding import Embedder


def main() -> None:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("tenant", type=int)
    args = parser.parse_args()

    url = os.environ["PY2TDB_DATABASE_URL"]
    with psycopg.connect(url, autocommit=True) as connection:
        store = Store(connection, args.tenant, Embedder())
        if not store.is_empty():
            raise SystemExit(f"tenant {args.tenant} already has events")
        graph = agent.build(agent.chat_model())
        for message in MESSAGES:
            reply = agent.respond(graph, store, message)
            print(f"{message.id}: {reply.tool_calls} tool calls")
        print(f"{store.length()} events")


if __name__ == "__main__":
    main()
