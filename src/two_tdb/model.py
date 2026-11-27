import math
from dataclasses import dataclass
from datetime import datetime

type Value = bool | int | float | str

_INT_MIN = -(2**63)
_INT_MAX = 2**63 - 1


class StoreError(Exception):
    pass


class InvalidInput(StoreError):
    pass


class EmptyCommit(StoreError):
    def __init__(self) -> None:
        super().__init__("cannot commit an empty collection of writes")


class OutOfRange(StoreError):
    def __init__(self, event_id: int, length: int) -> None:
        super().__init__(f"no event {event_id}: the log holds {length} events")
        self.event_id = event_id
        self.length = length


def aware(time: datetime) -> datetime:
    """Returns time unchanged; raises InvalidInput unless it has a UTC
    offset.
    """
    if not isinstance(time, datetime) or time.utcoffset() is None:
        raise InvalidInput(
            f"expected a datetime with a UTC offset, got {time!r}"
        )
    return time


def _check_text(what: str, text: object) -> None:
    if not isinstance(text, str) or text == "" or "\x00" in text:
        raise InvalidInput(
            f"{what} must be a non-empty string without NUL, got {text!r}"
        )


def _check_value(value: object) -> None:
    match value:
        case bool():
            pass
        case int() if not _INT_MIN <= value <= _INT_MAX:
            raise InvalidInput(f"integer {value} does not fit in 64 bits")
        case float() if not math.isfinite(value):
            raise InvalidInput(f"float {value} is not finite")
        case str() if "\x00" in value:
            raise InvalidInput("strings cannot contain NUL")
        case int() | float() | str():
            pass
        case _:
            raise InvalidInput(
                f"unsupported value type {type(value).__name__}"
            )


def check_source(source: object) -> None:
    """Raises InvalidInput unless source can name where an event came from."""
    _check_text("source", source)


@dataclass(frozen=True)
class Write:
    """One assertion to commit: a value for key from valid_from on, or None
    to delete.

    A write without valid_from takes effect at its event's commit time.
    """

    key: str
    value: Value | None
    valid_from: datetime | None = None

    def __post_init__(self) -> None:
        _check_text("key", self.key)
        if self.value is not None:
            _check_value(self.value)
        if self.valid_from is not None:
            aware(self.valid_from)

    @classmethod
    def set(
        cls, key: str, value: Value, valid_from: datetime | None = None
    ) -> Write:
        if value is None:
            raise InvalidInput("use Write.delete to delete a key")
        return cls(key, value, valid_from)

    @classmethod
    def delete(cls, key: str, valid_from: datetime | None = None) -> Write:
        return cls(key, None, valid_from)


@dataclass(frozen=True)
class Assertion:
    key: str
    valid_from: datetime
    value: Value | None


@dataclass(frozen=True)
class Event:
    id: int
    committed_at: datetime
    source: str
    assertions: tuple[Assertion, ...]


@dataclass(frozen=True)
class RecordedAssertion:
    event_id: int
    committed_at: datetime
    source: str
    assertion: Assertion


@dataclass(frozen=True)
class Added:
    value: Value


@dataclass(frozen=True)
class Removed:
    value: Value


@dataclass(frozen=True)
class Changed:
    before: Value
    after: Value


type Delta = Added | Removed | Changed
