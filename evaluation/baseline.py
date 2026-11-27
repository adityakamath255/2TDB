"""The baseline: plain RAG over the customer's raw messages.

It keeps every customer message with its sent time, and answers a question
from the messages closest in meaning to it, with the same model and embedder
as the agent.
"""

import math
from collections.abc import Sequence

import anthropic

from two_tdb.agent import MODEL, Message, Reply
from two_tdb.store import Embed

RETRIEVED = 5

SYSTEM_PROMPT = """\
You answer support staff's questions about one customer, using only the
customer's messages given with each question. Each message starts with the time
it was sent. Follow any answer format the question asks for, and say "unknown"
when the messages don't contain the answer.
"""


def _cosine(a: Sequence[float], b: Sequence[float]) -> float:
    return sum(x * y for x, y in zip(a, b, strict=True)) / (
        math.hypot(*a) * math.hypot(*b)
    )


class Notes:
    """Every customer message, embedded, searchable by meaning."""

    def __init__(self, embed: Embed) -> None:
        self._embed = embed
        self._messages: list[tuple[Message, Sequence[float]]] = []

    def add(self, message: Message) -> None:
        (vector,) = self._embed([message.text])
        self._messages.append((message, vector))

    def search(self, text: str, limit: int = RETRIEVED) -> list[Message]:
        """Returns the limit messages most similar to text, oldest first."""
        (query,) = self._embed([text])
        ranked = sorted(
            self._messages, key=lambda entry: -_cosine(query, entry[1])
        )
        return sorted(
            (message for message, _ in ranked[:limit]),
            key=lambda message: message.sent_at,
        )


def answer(
    client: anthropic.Anthropic, notes: Notes, question: Message
) -> Reply:
    found = "\n".join(
        f"[{m.sent_at.isoformat()}] {m.text}"
        for m in notes.search(question.text)
    )
    response = client.messages.create(
        model=MODEL,
        max_tokens=4096,
        system=SYSTEM_PROMPT,
        messages=[
            {
                "role": "user",
                "content": f"Customer messages:\n{found}\n\n"
                f"Question, asked {question.sent_at.isoformat()}: "
                f"{question.text}",
            }
        ],
    )
    text = "".join(
        block.text for block in response.content if block.type == "text"
    )
    return Reply(
        text,
        response.usage.input_tokens,
        response.usage.output_tokens,
        tool_calls=0,
    )
