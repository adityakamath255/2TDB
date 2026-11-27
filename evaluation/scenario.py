"""One customer's messages from January to October 2026, with questions.

Kinds of question:
- current: what is true when the question is asked
- past: what was true at an earlier date
- corrected: what the records said before the customer corrected a mistake
"""

from dataclasses import dataclass
from datetime import datetime
from typing import Literal

from py2tdb.agent import Message


def _at(text: str) -> datetime:
    return datetime.fromisoformat(f"{text}:00+05:30")


_MESSAGES = [
    (
        "2026-01-10T10:00",
        (
            "Hi, I'm writing from Halliard & Pike. "
            "We just signed up and we're on the free plan for now. "
            "We're based in Mumbai."
        ),
    ),
    (
        "2026-01-12T11:30",
        "Best way to reach us is email: opps@halyardpike.example",
    ),
    (
        "2026-01-13T09:15",
        "Sorry, typo in our email. It's ops@halyardpike.example",
    ),
    (
        "2026-02-03T16:00",
        "We upgraded to the pro plan today, with 5 seats, billed monthly.",
    ),
    ("2026-02-20T12:00", "By the way, I'm the operations manager here."),
    (
        "2026-02-21T10:00",
        (
            "Could you fix our company name in your records? "
            "It's Halyard & Pike, with a Y: H-A-L-Y-A-R-D."
        ),
    ),
    (
        "2026-02-22T18:45",
        (
            "I misspoke the other day: my title is head of operations, not "
            "operations manager."
        ),
    ),
    (
        "2026-03-05T14:20",
        (
            "Our renewal date is 1 March 2027. "
            "We haven't signed the contract yet, legal is still reviewing it."
        ),
    ),
    (
        "2026-03-18T09:40",
        (
            "I got the renewal date wrong earlier. "
            "It's 1 April 2027, not 1 March."
        ),
    ),
    (
        "2026-04-02T15:10",
        (
            "We moved our office to Pune at the end of March, and the "
            "whole team is there now."
        ),
    ),
    (
        "2026-04-15T11:00",
        "We added 7 more seats this week, so we have 12 now.",
    ),
    ("2026-04-17T10:30", "Small fix: we have 11 seats in total, not 12."),
    ("2026-05-06T13:00", "Please switch us to annual billing from 1 June."),
    ("2026-05-14T17:25", "Is there a dark mode in the dashboard?"),
    ("2026-05-22T12:10", "Your sales rep agreed a 10% discount for us."),
    (
        "2026-06-10T09:05",
        "WhatsApp is easier for me than email now, please use that.",
    ),
    ("2026-06-25T16:30", "Good news, I signed the contract today."),
    (
        "2026-07-08T11:45",
        (
            "Correction on the discount: it's 15%, not 10%. "
            "The rep confirmed it was 15% from the start."
        ),
    ),
    ("2026-08-01T10:00", "We pay by card from this month."),
    (
        "2026-08-03T10:20",
        "Sorry, I meant UPI, not card. We pay by UPI from this month.",
    ),
    (
        "2026-09-14T15:00",
        "Big news: we're moving to the enterprise plan on 1 October.",
    ),
    (
        "2026-09-30T12:40",
        (
            "Could you email invoices as PDFs? "
            "The paper copies keep getting lost."
        ),
    ),
    ("2026-10-12T10:15", "Our new phone number is +91 05550 12345."),
    (
        "2026-10-20T14:00",
        "Also, we relocated again, to Bengaluru, two weeks ago.",
    ),
    (
        "2026-10-25T09:30",
        "The phone number we gave was wrong. It's +91 05550 54321.",
    ),
    ("2026-10-28T17:00", "Thanks for all the help this year!"),
]

MESSAGES = [
    Message("customer", f"msg-{n:02}", _at(sent), text)
    for n, (sent, text) in enumerate(_MESSAGES, 1)
]

ASKED_AT = _at("2026-10-31T12:00")

type Kind = Literal["current", "past", "corrected"]


@dataclass(frozen=True)
class Question:
    id: str
    kind: Kind
    text: str
    expected: str

    def message(self) -> Message:
        return Message(
            "staff",
            self.id,
            ASKED_AT,
            f"{self.text} Reply with only the answer, no sentence, "
            "or unknown.",
        )


_QUESTIONS: list[tuple[Kind, str, str]] = [
    ("current", "Which city is the customer based in?", "Bengaluru"),
    ("current", "Which plan is the customer on?", "enterprise"),
    (
        "current",
        "How many seats does the customer pay for? Answer with a number.",
        "11",
    ),
    (
        "current",
        (
            "Is the customer billed monthly or annually? "
            "Answer monthly or annual."
        ),
        "annual",
    ),
    (
        "current",
        (
            "What discount does the customer get, in percent? "
            "Answer with a number."
        ),
        "15",
    ),
    (
        "current",
        "When does the customer's subscription renew? Answer as YYYY-MM-DD.",
        "2027-04-01",
    ),
    (
        "current",
        "Which channel does the customer prefer to be contacted on?",
        "WhatsApp",
    ),
    ("current", "How does the customer pay?", "UPI"),
    ("past", "Which city was the customer based in on 2026-03-15?", "Mumbai"),
    ("past", "Which city was the customer based in on 2026-05-01?", "Pune"),
    ("past", "Which plan was the customer on on 2026-01-20?", "free"),
    ("past", "Which plan was the customer on on 2026-09-20?", "pro"),
    (
        "past",
        (
            "How many seats did the customer pay for on 2026-04-01? "
            "Answer with a number."
        ),
        "5",
    ),
    (
        "past",
        (
            "Was the customer billed monthly or annually on 2026-05-20? "
            "Answer monthly or annual."
        ),
        "monthly",
    ),
    (
        "past",
        (
            "Which channel did the customer prefer to be contacted on on "
            "2026-03-01?"
        ),
        "email",
    ),
    (
        "past",
        (
            "Had the customer signed the contract as of 2026-06-01? "
            "Answer yes or no."
        ),
        "no",
    ),
    (
        "corrected",
        (
            "Before the customer corrected it, what renewal date did we have "
            "on file? Answer as YYYY-MM-DD."
        ),
        "2027-03-01",
    ),
    (
        "corrected",
        (
            "Before the customer corrected it, what discount did we have on "
            "file, in percent? Answer with a number."
        ),
        "10",
    ),
    (
        "corrected",
        (
            "Before the customer corrected it, what phone number did we have "
            "on file?"
        ),
        "+91 05550 12345",
    ),
    (
        "corrected",
        (
            "Before the customer corrected it, how many seats did we have on "
            "file for April? Answer with a number."
        ),
        "12",
    ),
    (
        "corrected",
        (
            "Before the customer corrected it, how was the customer's "
            "company name spelled in our records?"
        ),
        "Halliard & Pike",
    ),
    (
        "corrected",
        (
            "Before the customer corrected it, what email address did we have "
            "on file?"
        ),
        "opps@halyardpike.example",
    ),
    (
        "corrected",
        (
            "Before the customer corrected it, what job title did we have on "
            "file?"
        ),
        "operations manager",
    ),
    (
        "corrected",
        (
            "Before the customer corrected it, what payment method did we "
            "have on file?"
        ),
        "card",
    ),
]

QUESTIONS = [
    Question(f"q-{n:02}", kind, text, expected)
    for n, (kind, text, expected) in enumerate(_QUESTIONS, 1)
]
