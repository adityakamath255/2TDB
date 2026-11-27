import json
from pathlib import Path

import pytest

from evaluation.run import correct, evaluate, report
from evaluation.scenario import MESSAGES, QUESTIONS
from two_tdb.agent import Reply


@pytest.mark.parametrize(
    "answer, expected",
    [
        ("Pune.", "Pune"),
        ("15%", "15"),
        ("15.0", "15"),
        ("+919845012345", "+91 98450 12345"),
        ("`whatsapp`", "WhatsApp"),
    ],
)
def test_answers_match_despite_formatting(answer, expected):
    assert correct(answer, expected)


@pytest.mark.parametrize(
    "answer, expected",
    [("Pune, Maharashtra", "Pune"), ("unknown", "5"), ("1.5", "15")],
)
def test_other_answers_do_not_match(answer, expected):
    assert not correct(answer, expected)


def test_the_harness_scores_each_system_and_kind(new_store):
    expected = {
        q.message().text: q.expected for q in QUESTIONS if q.kind == "current"
    }
    seen = []

    def two_tdb(store):
        def respond(message):
            seen.append(message.id)
            return Reply(expected.get(message.text, "unknown"), 1000, 100, 2)

        return respond

    def rag():
        return (lambda m: Reply("", 0, 0, 0)), (
            lambda q: Reply("unknown", 500, 10, 0)
        )

    ingests, results = evaluate(2, new_store, two_tdb, rag)

    assert seen[: len(MESSAGES) + 1] == [m.id for m in MESSAGES] + [
        QUESTIONS[0].id
    ]
    assert len(results) == 2 * 2 * len(QUESTIONS)
    table = report(ingests, results)
    assert "| 2TDB agent | 16/16 | 0/16 | 0/16 | 16/48 |" in table
    assert "| RAG baseline | 0/16 | 0/16 | 0/16 | 0/48 |" in table
    # 1,000 input and 100 output tokens per call:
    # (1000 * 0.10 + 100 * 0.50) / 1e6
    assert "| 0.00015 | 0.0039 |" in table


def test_the_demo_page_quotes_the_scenario_messages():
    # the page quotes the message behind each fact from this copy
    page = Path(__file__).parent.parent / "docs" / "messages.json"
    assert json.loads(page.read_text()) == {
        message.id: {
            "sent_at": message.sent_at.isoformat(),
            "text": message.text,
        }
        for message in MESSAGES
    }
