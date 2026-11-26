from datetime import datetime
from itertools import count

from langchain_core.language_models import BaseChatModel
from langchain_core.messages import AIMessage, BaseMessage, ToolMessage
from langchain_core.outputs import ChatGeneration, ChatResult

from two_tdb import agent
from two_tdb.agent import Message

SENT = datetime.fromisoformat("2026-03-05T10:00:00+05:30")
_ids = count()


class Scripted(BaseChatModel):
    """Replies with the given messages in turn and keeps what it was shown."""

    replies: list[AIMessage]
    seen: list[list[BaseMessage]] = []

    @property
    def _llm_type(self) -> str:
        return "scripted"

    def bind_tools(self, tools, **kwargs):
        return self

    def _generate(self, messages, stop=None, run_manager=None, **kwargs) -> ChatResult:
        self.seen.append(list(messages))
        return ChatResult(generations=[ChatGeneration(message=self.replies.pop(0))])


def calls(*calls: tuple[str, dict]) -> AIMessage:
    tool_calls = [{"name": name, "args": args, "id": f"call-{next(_ids)}"} for name, args in calls]
    return AIMessage("", tool_calls=tool_calls, usage_metadata={"input_tokens": 100, "output_tokens": 10, "total_tokens": 110})


def say(text: str) -> AIMessage:
    return AIMessage(text, usage_metadata={"input_tokens": 50, "output_tokens": 5, "total_tokens": 55})


def run(store, *replies: AIMessage, text: str = "We moved to Pune last month.") -> tuple[agent.Reply, Scripted]:
    model = Scripted(replies=list(replies), seen=[])
    reply = agent.respond(agent.build(model), store, Message("customer", "msg-1", SENT, text))
    return reply, model


def test_the_agent_remembers_with_the_message_as_source(store):
    reply, model = run(
        store,
        calls(("remember", {"facts": [{"key": "city", "value": "Pune", "valid_from": "2026-02-01T00:00:00+05:30"}]})),
        say("Noted, thanks!"),
    )

    assert reply == agent.Reply("Noted, thanks!", input_tokens=150, output_tokens=15, tool_calls=1)
    assert store.latest().valid_at(SENT).get("city") == "Pune"
    assert store.latest().blame("city").source == "msg-1"
    assert "[customer message msg-1, sent 2026-03-05T10:00:00+05:30]" in model.seen[0][-1].text


def test_store_errors_go_back_to_the_model(store):
    reply, model = run(
        store,
        calls(("remember", {"facts": [{"key": "shoe_size", "value": 9, "valid_from": "2026-03-05T10:00:00+05:30"}]})),
        calls(("remember", {"facts": [{"key": "seats", "value": "twelve", "valid_from": "2026-03-05T10:00:00+05:30"}]})),
        calls(("lookup", {"key": "city", "valid_at": "2026-03-05T10:00:00"})),
        say("Sorry, I couldn't save that."),
    )

    errors = [m.text for m in model.seen[-1] if isinstance(m, ToolMessage)]
    assert errors == [
        "error: 'shoe_size' is not a known attribute or note/<topic>",
        "error: seats takes a int, got 'twelve'",
        "error: expected a datetime with a UTC offset, got datetime.datetime(2026, 3, 5, 10, 0)",
    ]
    assert store.is_empty()
    assert reply.text == "Sorry, I couldn't save that."


def test_a_message_gets_a_bounded_number_of_tool_calls(store):
    looping = [calls(("facts", {})) for _ in range(agent.TOOL_CALLS_PER_MESSAGE + 2)]
    reply, model = run(store, *looping, say("done"))

    assert reply.tool_calls == agent.TOOL_CALLS_PER_MESSAGE + 1
    assert len(model.seen) == agent.TOOL_CALLS_PER_MESSAGE + 1


def test_reads_take_valid_times_and_show_superseded_facts(store):
    store.commit([agent._write(agent.Fact(key="plan", value="free", valid_from=SENT))], source="msg-0")
    store.commit([agent._write(agent.Fact(key="plan", value="pro", valid_from=SENT))], source="msg-1")
    _, model = run(
        store,
        calls(("history", {"key": "plan"}), ("lookup", {"key": "plan", "valid_at": "2026-01-01T00:00:00Z"})),
        say("free, then pro"),
        text="What plan did we have before the correction?",
    )

    history, before = (m.text for m in model.seen[-1] if isinstance(m, ToolMessage))
    assert '"value": "free"' in history and '"source": "msg-0"' in history and '"value": "pro"' in history
    assert before == "null"
