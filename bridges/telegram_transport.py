#!/usr/bin/env python3
"""What Telegram may carry, and how a transport delivers it (t594, FLAT_FLEET 7.2).

The owner's rule (2026-10-29): **reply on the transport the question arrived on.
Telegram carries answers to Telegram questions and nothing else** - no web echoes,
no reports, no digests. Pages (security / outage / money, max 3 per hour,
`WATCHDOG_ESCALATION_SPEC.md`) are the one exception.

This module is the policy, deliberately separate from the bridge's Telegram I/O and
from `web.app`, so the rule can be tested without a network, a token or a desk. The
daemon owns the mailbox: this asks it for the outbox and records receipts, and it
never writes a store of its own.

Page marker: the watchdog ALREADY writes `PAGE OWNER` into a page's text
(`dunk_watchdog.py`), so nothing new is invented here - `is_page` reads that marker.
Pairing state stays where the bridge keeps it today; FLAT_FLEET 7.2 moves it
"unchanged", so this module has no opinion about the path.
"""

from __future__ import annotations

PAGE_MARKER = "PAGE OWNER"
PAGE_CLASSES = ("security", "outage", "money")
PAGE_MAX_PER_HOUR = 3
PAGE_WINDOW_S = 3600.0


def is_page(body):
    """Is this owner-mailbox row one of the three page classes?

    The watchdog's existing marker is the test; a page that carries one of the class
    words is classed, and a page without one still pages (it is a page either way -
    the class is for reporting, not for permission).
    """
    text = str(body or "")
    return PAGE_MARKER in text.upper()


def page_class(body):
    """'security' | 'outage' | 'money' | None, from the page text."""
    text = str(body or "").lower()
    for name in PAGE_CLASSES:
        if name in text:
            return name
    return None


def pages_allowed(sent_at, now, limit=PAGE_MAX_PER_HOUR, window=PAGE_WINDOW_S):
    """How many pages may still go out: the 3-per-hour cap, as a number.

    `sent_at` is the list of timestamps already delivered. The cap is a budget, not a
    queue: a fourth page inside the window is held, not dropped, and the caller says so
    in its log rather than silently swallowing it.
    """
    recent = [at for at in (sent_at or []) if now - float(at) < float(window)]
    return max(0, int(limit) - len(recent))


def outbox_items(replies, pages, sent_page_times, now,
                 page_limit=PAGE_MAX_PER_HOUR, window=PAGE_WINDOW_S):
    """What to send now, in order, each with the reason it is going.

    `replies` is the daemon's owner outbox for this transport: rows that are REPLIES
    whose question arrived on it. `pages` are page rows the watchdog produced. Anything
    else - a brief, a digest, a web echo - is not in either list and cannot be sent,
    because nothing here selects it.
    """
    items = []
    for row in replies or []:
        items.append({"message_id": row["message_id"], "body": row.get("body") or "",
                      "kind": "reply", "reason": "reply on its own transport"})
    budget = pages_allowed(sent_page_times, now, limit=page_limit, window=window)
    for row in pages or []:
        if budget <= 0:
            items.append({"message_id": row["message_id"],
                          "body": row.get("body") or "", "kind": "page",
                          "reason": f"held: {page_limit} pages per hour already sent",
                          # Provenance: the store id this page came from, so the
                          # caller advances its watermark only for a page that was
                          # actually delivered (t667).
                          "id": row.get("id"),
                          "hold": True})
            continue
        budget -= 1
        items.append({"message_id": row["message_id"], "body": row.get("body") or "",
                      "kind": "page", "id": row.get("id"),
                      "reason": f"page ({page_class(row.get('body')) or 'unclassified'})",
                      "page_class": page_class(row.get("body"))})
    return items


def deliver(items, send, mark_delivered):
    """Send each item and record the receipt; a failure stops the batch there.

    `send(body)` performs the transport's own send (and raises on failure).
    `mark_delivered(message_id, kind)` records the daemon-side receipt. Returning the
    sent ids lets the caller log exactly what landed - a page is confirmed by the
    receipt, not by the transport's optimism.
    """
    sent = []
    for item in items:
        if item.get("hold"):
            continue
        send(item["body"])
        mark_delivered(item["message_id"], item["kind"])
        sent.append({"message_id": item["message_id"], "kind": item["kind"],
                     "reason": item["reason"]})
    return sent
