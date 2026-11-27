"""A customer-support agent whose only memory is a 2TDB store.

LangChain stays inside this module. Each message runs the agent afresh: what it
knows from earlier messages is what it wrote to the store.
"""

import json
from dataclasses import dataclass
from datetime import datetime
from typing import Any, Literal

from langchain.agents import create_agent
from langchain.agents.middleware import (
    AgentMiddleware,
    AgentState,
    InputAgentState,
    OutputAgentState,
    ToolCallLimitMiddleware,
    ToolErrorMiddleware,
)
from langchain.tools import ToolRuntime, tool
from langchain_anthropic import ChatAnthropic
from langchain_core.language_models import BaseChatModel
from langchain_core.messages import AIMessage, HumanMessage
from langgraph.graph.state import CompiledStateGraph
from pydantic import BaseModel

from py2tdb.model import (
    InvalidInput,
    RecordedAssertion,
    StoreError,
    Value,
    Write,
)
from py2tdb.store import Reader, Snapshot, Store

MODEL = "claude-haiku-5-5"
TOOL_CALLS_PER_MESSAGE = 8

# the facts the agent may record, with their types; free-text facts go
# under note/<topic>
ATTRIBUTES: dict[str, tuple[type, str]] = {
    "name": (str, "contact person's full name"),
    "email": (str, "email address"),
    "phone": (str, "phone number"),
    "city": (str, "city the customer is based in"),
    "address": (str, "street address"),
    "company": (str, "company name"),
    "role": (str, "job title"),
    "plan": (str, "subscription plan: free, pro or enterprise"),
    "seats": (int, "number of paid seats"),
    "billing_cycle": (str, "monthly or annual"),
    "payment_method": (str, "how the customer pays, such as card or UPI"),
    "renewal_date": (str, "next renewal date as YYYY-MM-DD"),
    "discount_percent": (float, "agreed discount in percent"),
    "contract_signed": (bool, "whether the customer has signed the contract"),
    "contact_channel": (str, "preferred way to reach the customer"),
    "invoice_delivery": (str, "how invoices are sent, such as post or email"),
}

_ATTRIBUTE_LINES = "\n".join(
    f"- {key}: {kind.__name__}, {meaning}"
    for key, (kind, meaning) in ATTRIBUTES.items()
)

SYSTEM_PROMPT = f"""\
You are the memory of a customer-support team for one customer. Each message is
either from the customer or a question from support staff, and starts with a
header giving its sender, its ID and when it was sent. You remember nothing
between messages except what is in the store, so read it with your tools and
write to it with `remember`.

The store is bitemporal. Each fact has a valid time (when it became true in the
world) and a recorded time (when the store learned it). Writing a fact never
erases the old one: reads at a valid time pick, per key, the fact with the
latest valid_from at or before that time, and among facts with the same
valid_from, the one recorded last.

Facts you may record (key: type, meaning):
{_ATTRIBUTE_LINES}
- note/<topic>: str, anything else worth keeping, one fact per topic

On a customer message:
- Record each fact the customer states about themselves. Set valid_from to when
  it became true, resolving phrases like "last month" or "from 1 April"
  against the message's sent time; if they don't say, use the sent time.
- A change ("we moved to Pune") is a new fact from the time it happened, so the
  old value stays true for the time before.
- A correction ("my renewal is 1 April, not 1 March") fixes a mistake. Use
  `blame` to find when the wrong fact took effect and record the right value
  with that same valid_from, so it replaces the wrong one for the whole period.
- Reply to the customer in one or two sentences.

On a staff question, use the tools to find the answer: `facts` or `lookup` with
valid_at for what was true at a time, and `history` for what the store held
before a later correction or change. Follow any answer format the question asks
for, and say "unknown" when the store doesn't have the answer.
"""


@dataclass(frozen=True)
class Context:
    """What one run of the agent works on: the customer's store and the
    message being handled.
    """

    store: Store
    message_id: str


class Fact(BaseModel):
    key: str
    value: Value | None  # null forgets the key from valid_from on
    valid_from: datetime


def _snapshot(reader: Reader, valid_at: datetime | None) -> Snapshot:
    snapshot = reader.latest()
    return snapshot if valid_at is None else snapshot.valid_at(valid_at)


def _recorded(entry: RecordedAssertion) -> dict:
    return {
        "value": entry.assertion.value,
        "valid_from": entry.assertion.valid_from.isoformat(),
        "recorded_at": entry.committed_at.isoformat(),
        "event": entry.event_id,
        "source": entry.source,
    }


def _json(data: object) -> str:
    return json.dumps(data, ensure_ascii=False)


def _write(fact: Fact) -> Write:
    if fact.key.startswith("note/") and len(fact.key) > len("note/"):
        kind = str
    elif fact.key in ATTRIBUTES:
        kind = ATTRIBUTES[fact.key][0]
    else:
        raise InvalidInput(
            f"{fact.key!r} is not a known attribute or note/<topic>"
        )
    value = fact.value
    if kind is float and type(value) is int:
        value = float(value)
    if value is not None and type(value) is not kind:
        raise InvalidInput(
            f"{fact.key} takes {kind.__name__} values, got {value!r}"
        )
    return Write(fact.key, value, fact.valid_from)


@tool
def remember(facts: list[Fact], runtime: ToolRuntime[Context]) -> str:
    """Records facts about the customer, all in one event."""
    context = runtime.context
    event = context.store.commit(
        [_write(fact) for fact in facts], source=context.message_id
    )
    return f"recorded as event {event}"


@tool
def facts(
    runtime: ToolRuntime[Context], valid_at: datetime | None = None
) -> str:
    """Returns every fact true at valid_at (default: now), as known now."""
    return _json(_snapshot(runtime.context.store, valid_at).state())


@tool
def lookup(
    key: str, runtime: ToolRuntime[Context], valid_at: datetime | None = None
) -> str:
    """Returns key's value at valid_at (default: now), or null if none."""
    return _json(_snapshot(runtime.context.store, valid_at).get(key))


@tool
def blame(
    key: str, runtime: ToolRuntime[Context], valid_at: datetime | None = None
) -> str:
    """Returns the record deciding key's value at valid_at (default: now)."""
    entry = _snapshot(runtime.context.store, valid_at).blame(key)
    return _json(None if entry is None else _recorded(entry))


@tool
def history(key: str, runtime: ToolRuntime[Context]) -> str:
    """Returns every fact ever recorded for key, in the order recorded,
    including superseded ones.
    """
    return _json(
        [_recorded(entry) for entry in runtime.context.store.history(key)]
    )


@tool
def search(
    query: str, runtime: ToolRuntime[Context], valid_at: datetime | None = None
) -> str:
    """Returns the facts at valid_at (default: now) most similar to query."""
    return _json(_snapshot(runtime.context.store, valid_at).search(query))


def _store_error(error: Exception, request: object) -> str | None:
    # store errors describe bad input, which the model can fix; anything
    # else propagates
    return f"error: {error}" if isinstance(error, StoreError) else None


def chat_model() -> ChatAnthropic:
    """Claude Haiku via the Anthropic SDK, which reads ANTHROPIC_API_KEY."""
    # pyright reads the model's pydantic field aliases and positional Field
    # defaults as required parameters
    return ChatAnthropic(model=MODEL, max_tokens=4096)  # pyright: ignore[reportCallIssue]


type Agent = CompiledStateGraph[
    AgentState[Any], Context, InputAgentState, OutputAgentState[Any]
]


def build(model: BaseChatModel) -> Agent:
    """Returns the agent graph, to run with respond."""
    middleware: list[AgentMiddleware[Any, Context]] = [
        ToolErrorMiddleware(on_error=_store_error),
        ToolCallLimitMiddleware(
            run_limit=TOOL_CALLS_PER_MESSAGE, exit_behavior="end"
        ),
    ]
    return create_agent(
        model,
        [remember, facts, lookup, blame, history, search],
        system_prompt=SYSTEM_PROMPT,
        context_schema=Context,
        middleware=middleware,
    )


@dataclass(frozen=True)
class Message:
    sender: Literal["customer", "staff"]
    id: str
    sent_at: datetime
    text: str


@dataclass(frozen=True)
class Reply:
    text: str
    input_tokens: int
    output_tokens: int
    tool_calls: int


def respond(agent: Agent, store: Store, message: Message) -> Reply:
    """Runs the agent on one message against store, committing with the
    message's ID as source.
    """
    header = (
        f"[{message.sender} message {message.id}, "
        f"sent {message.sent_at.isoformat()}]"
    )
    result = agent.invoke(
        {"messages": [HumanMessage(f"{header}\n{message.text}")]},
        context=Context(store, message.id),
    )
    replies = [m for m in result["messages"] if isinstance(m, AIMessage)]
    usage = [m.usage_metadata for m in replies if m.usage_metadata]
    return Reply(
        text=replies[-1].text,
        input_tokens=sum(u["input_tokens"] for u in usage),
        output_tokens=sum(u["output_tokens"] for u in usage),
        tool_calls=sum(len(m.tool_calls) for m in replies),
    )
