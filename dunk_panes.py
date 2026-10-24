"""Retired panes: the ones we deliberately stopped feeding.

A pane can be superseded (a replacement was started elsewhere) or replaced by
machinery (the dispatcher took over a PM's job). Its mailbox is then mail nobody
will ever read, and paging about it every four hours is an alarm that cannot be
satisfied - the failure mode we keep removing.

Retiring is bookkeeping, not deletion: the mail stays as the record of what was
sent, the pane stops counting as idle or ready, and the fact that it is retired
is reported instead of silently vanishing.
"""
import json
import os
import re
import time

CONFIG_DIR = os.environ.get("DUNKINGSHEEP_DIR", os.path.expanduser("~/.config/dunkingsheep"))
PATH = os.path.join(CONFIG_DIR, "retired-panes.json")

# 'name (w9:pH)' and 'w9:pH' both name a pane; the mailbox table carries both.
PANE_IN_MAILBOX = re.compile(r"([A-Za-z0-9_.-]+:p[A-Za-z0-9_.-]+)")


class RetireError(ValueError):
    """A named refusal."""


def _load(path=PATH):
    try:
        with open(path, encoding="utf-8") as handle:
            data = json.load(handle)
        return data if isinstance(data, dict) else {}
    except (OSError, ValueError):
        return {}


def _write(data, path=PATH):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    temporary = path + ".tmp"
    with open(temporary, "w", encoding="utf-8") as handle:
        json.dump(data, handle, indent=2, sort_keys=True)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, path)


def retire(pane_id, reason, by=None, *, labels=(), mailboxes=(), path=PATH, now=None):
    """Mark a pane retired. The reason is required: an unexplained silence is a bug."""
    pane_id = str(pane_id or "").strip()
    if not pane_id:
        raise RetireError("a pane id is required")
    reason = (reason or "").strip()
    if not reason:
        raise RetireError("a reason is required (e.g. superseded-by:wW:p7)")
    data = _load(path)
    names = sorted({str(label).strip() for label in labels if str(label or "").strip()})
    # MAILBOXES, not only labels: mail addressed to a dead pane is filed under a NAME
    # ('crackdown-codex'), which is not a pane id and not always the tab label. A
    # retire that only recorded labels left those mailboxes alerting and unarchived.
    boxes = sorted({str(box).strip() for box in mailboxes if str(box or "").strip()})
    data[pane_id] = {"pane_id": pane_id, "reason": reason, "by": by,
                     "labels": names, "mailboxes": boxes,
                     "retired_at": now or time.time()}
    _write(data, path)
    return data[pane_id]


def unretire(pane_id, *, path=PATH):
    data = _load(path)
    if pane_id not in data:
        raise RetireError(f"{pane_id} is not retired")
    removed = data.pop(pane_id)
    _write(data, path)
    return removed


def all_retired(path=PATH):
    return [data for _pane, data in sorted(_load(path).items())]


def is_retired(pane_id, *, path=PATH):
    return str(pane_id or "").strip() in _load(path)


def mailbox_retired(mailbox, *, path=PATH):
    """True when a mailbox belongs to a retired pane, however it is spelled.

    The table holds 'name (w9:pH)', 'w9:pH' and plain names, so the pane id is
    extracted from whatever form the row uses.
    """
    data = _load(path)
    if not data:
        return False
    text = str(mailbox or "")
    if text.strip() in data:
        return True
    match = PANE_IN_MAILBOX.search(text)
    if match and match.group(1) in data:
        return True
    plain = re.sub(r"\s*\([^)]*\)\s*$", "", text).strip()
    for record in data.values():
        for label in record.get("labels") or ():
            if label == text.strip() or label == plain:
                return True
        for box in record.get("mailboxes") or ():
            if box == text.strip() or box == plain:
                return True
    return False
