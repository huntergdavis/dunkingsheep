#!/usr/bin/env python3
"""Golden replay of the Python Telegram bridge, on fakes only (t3231 parity oracle).

Runs `bridges/telegram_bridge.py` (and its `telegram_transport` / `dunk_telegram_routing` modules)
against a stub desk, a fake Telegram, a fake daemon and a fixed clock, for each scenario below. It
writes the scenario INPUTS together with the OBSERVED outputs (every Telegram call, every daemon
call, the final telegram.json and status file, each step's result or exception) to a JSON file.
The Rust port replays the same inputs in `rust/crates/ds-telegram/tests/oracle_replay.rs` and must
produce the same outputs.

No network, no token, no live desk: the desk is a stub `web.app` module over a temporary sqlite
file, and every Telegram or daemon call goes to a fake that records it.

Usage: python3 tools/telegram_oracle_replay.py --work DIR --out FILE
"""

from __future__ import annotations

import argparse
import contextlib
import io
import json
import os
import sqlite3
import sys
import types
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "bridges"))
sys.path.insert(0, str(ROOT))

NOW = 10000  # the pinned wall clock; an int so the JSON output is stable
PAIR = "PAIRCODE"


class StopLoop(BaseException):
    """Ends `run()` when the scripted updates are used up. BaseException, so the loop's own
    `except (RuntimeError, ValueError, OSError)` cannot swallow it."""


def float_tree(value):
    """Every number as a float, so int-vs-float spelling never decides a comparison."""
    if isinstance(value, bool) or value is None or isinstance(value, str):
        return value
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, list):
        return [float_tree(item) for item in value]
    if isinstance(value, dict):
        return {key: float_tree(item) for key, item in value.items()}
    return value


def install_desk(data: Path):
    """A stub `web.app`: the same names the bridge imports, over a real sqlite file."""
    desk = types.ModuleType("web.app")
    desk.DATA = data
    desk.ROOT = ROOT
    desk.UPLOADS = data / "uploads"
    desk.DB = data / "messages.sqlite3"
    desk.CHANNEL_INDEX = {}

    @contextlib.contextmanager
    def _db():
        conn = sqlite3.connect(desk.DB, timeout=10)
        conn.row_factory = sqlite3.Row
        try:
            with conn:
                yield conn
        finally:
            conn.close()

    def _private_write(path, content):
        Path(path).write_text(content, encoding="utf-8")

    desk._db = _db
    desk._private_write = _private_write
    desk._reconcile = lambda channel: None
    package = types.ModuleType("web")
    sys.modules["web"] = package
    sys.modules["web.app"] = desk
    return desk


class FakeBot:
    def __init__(self, webhook_url=""):
        self.log = []
        self.fail_on = None
        self.batches = []
        self.webhook_url = webhook_url

    def call(self, method, **payload):
        # The real Telegram.call drops None-valued keys before it sends; so does the fake.
        payload = {key: value for key, value in payload.items() if value is not None}
        self.log.append([method, float_tree(payload)])
        if method == "sendMessage":
            text = payload.get("text", "")
            if self.fail_on and self.fail_on in text:
                raise RuntimeError("Telegram sendMessage: 502")
            return {}
        if method == "getUpdates":
            if not self.batches:
                raise StopLoop()
            return self.batches.pop(0)
        if method == "getWebhookInfo":
            return {"url": self.webhook_url}
        return {}

    def download(self, file_id):
        raise RuntimeError("attachments are not replayed")


class FakeClient:
    """The daemon: an owner outbox and an ingest answer, and a record of every call."""

    def __init__(self, outbox, ingest):
        self.log = []
        self.outbox = outbox
        self.ingest = ingest

    def call(self, name, **kwargs):
        self.log.append([name, float_tree(kwargs)])
        if name == "owner_outbox":
            return {"messages": list(self.outbox)}
        if name == "owner_ingest":
            return self.ingest
        return {}


class FakeTime:
    def time(self):
        return NOW

    def sleep(self, seconds):
        pass


def reset(work: Path, scenario: dict, desk):
    for name in ("telegram.json", "telegram-routing.json", "telegram-status.json",
                 "messages.sqlite3", "telegram.lock"):
        target = work / name
        if target.exists():
            target.unlink()
    registry = work / "channels.json"
    registry.write_text(json.dumps(scenario["registry"]), encoding="utf-8")
    os.environ["VP_CHANNELS_FILE"] = str(registry)
    os.environ.pop("VP_TELEGRAM_CHANNEL", None)
    desk.CHANNEL_INDEX.clear()
    desk.CHANNEL_INDEX.update({row["id"]: row for row in scenario["registry"]})
    if scenario.get("routing") is not None:
        (work / "telegram-routing.json").write_text(json.dumps(scenario["routing"]), encoding="utf-8")
    # The desk store always has its table, as the real desk does (an absent table is not a state
    # the desk ever leaves). `delivery` and `external_id` are the two columns `/status` reads; they
    # carry the desk's own defaults `delivery TEXT NOT NULL DEFAULT 'posted'` / NULL.
    conn = sqlite3.connect(work / "messages.sqlite3")
    conn.execute("CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY, channel TEXT, role TEXT, "
                 "body TEXT, delivery TEXT NOT NULL DEFAULT 'posted', external_id TEXT)")
    conn.execute("DELETE FROM messages")
    for row in scenario.get("desk_rows", []):
        # (id, channel, role, body[, external_id]): an id-bearing row of any role.
        row_id, channel, role, body = row[:4]
        external_id = row[4] if len(row) > 4 else None
        conn.execute("INSERT INTO messages (id, channel, role, body, external_id) "
                     "VALUES (?, ?, ?, ?, ?)",
                     (row_id, channel, role, body, external_id))
    for row_id, channel, delivery, external_id in scenario.get("owner_rows", []):
        conn.execute("INSERT INTO messages (id, channel, role, body, delivery, external_id) "
                     "VALUES (?, ?, 'owner', '', ?, ?)",
                     (row_id, channel, delivery, external_id))
    conn.commit()
    conn.close()
    if scenario.get("config") is not None:
        (work / "telegram.json").write_text(json.dumps(scenario["config"], indent=2) + "\n", encoding="utf-8")


def read_json(path: Path):
    if not path.exists():
        return None
    return float_tree(json.loads(path.read_text(encoding="utf-8")))


def run_scenario(work: Path, scenario: dict, desk, bridge):
    reset(work, scenario, desk)
    bot = FakeBot(scenario.get("webhook_url", ""))
    bot.fail_on = scenario.get("fail_on")
    daemon_spec = scenario.get("daemon", {})
    client = FakeClient(daemon_spec.get("outbox", []), daemon_spec.get("ingest"))
    bridge._daemon = lambda: client
    bridge.Telegram = lambda token: bot
    bridge.time = FakeTime()
    config = bridge.load_config() if scenario.get("config") is not None else None
    steps_out = []
    for step in scenario["steps"]:
        op = step["op"]
        record = {"op": op, "error": None}
        try:
            if op == "process_update":
                bridge.process_update(step["update"], config, bot)
            elif op == "send_owner_outbox":
                sent = bridge.send_owner_outbox(config, bot, client)
                record["sent"] = float_tree(sent)
            elif op == "run":
                bot.batches = [list(batch) for batch in step["batches"]]
                try:
                    bridge.run()
                except StopLoop:
                    record["stopped"] = True
            elif op == "set_fail_on":
                # The transport recovers: later sends stop failing.
                bot.fail_on = step["value"]
            elif op == "write_config":
                # The operator (or a test) edits telegram.json between two ticks.
                (work / "telegram.json").write_text(json.dumps(step["config"], indent=2) + "\n", encoding="utf-8")
                config = bridge.load_config()
            elif op == "status":
                out, err = io.StringIO(), io.StringIO()
                saved = (sys.argv, sys.stdout, sys.stderr)
                sys.argv = ["telegram_bridge.py", "status"]
                sys.stdout, sys.stderr = out, err
                try:
                    bridge.main()
                except SystemExit:
                    pass
                finally:
                    sys.argv, sys.stdout, sys.stderr = saved
                record["stdout"] = out.getvalue()
                record["stderr"] = err.getvalue()
            else:
                raise ValueError(f"unknown op {op}")
        except (RuntimeError, ValueError, OSError) as exc:
            record["error"] = [type(exc).__name__, str(exc)]
        steps_out.append(record)
        if op in ("process_update", "send_owner_outbox"):
            # The config the next step sees is the one on disk, as the real bridge reloads it.
            config = bridge.load_config() if (work / "telegram.json").exists() else config
    status = read_json(work / "telegram-status.json")
    if isinstance(status, dict):
        status.pop("updated_at", None)
    return {
        "name": scenario["name"],
        "input": scenario,
        "expected": {
            "steps": steps_out,
            "bot_log": bot.log,
            "daemon_log": client.log,
            "config": read_json(work / "telegram.json"),
            "status": status,
        },
    }


def registry(*rows):
    return [dict(row) for row in rows]


VP_COS = registry({"id": "vp", "name": "VP"}, {"id": "cos", "name": "Chief of Staff"})


def paired(**extra):
    base = {"bot_token": "123:TEST-TOKEN-NOT-REAL", "pair_code": PAIR, "chat_id": 42,
            "update_offset": None, "last_brief_id": None, "brief_channel": "cos"}
    base.update(extra)
    return base


def unpaired(**extra):
    base = paired(chat_id=None)
    base.update(extra)
    return base


def text(update_id, chat_id, body, kind="private"):
    return {"update_id": update_id, "message": {"chat": {"id": chat_id, "type": kind}, "text": body}}


def outbox_row(message_id, sender, body, reply_to=None):
    return {"message_id": message_id, "sender": sender, "transport": "telegram",
            "reply_to": reply_to, "body": body}


def watermark_rows():
    return [
        [11, "cos", "agent", "PAGE OWNER outage: web desk down"],
        [12, "cos", "agent", "PAGE OWNER money: provider cap hit"],
        [13, "cos", "agent", "an ordinary agent row, not a page"],
        [14, "cos", "agent", "PAGE OWNER security: token exposed"],
    ]


def scenarios():
    ingest_ok = {"message": {"message_id": "i7"}, "duplicate": False}
    ingest_dup = {"message": {"message_id": "i7"}, "duplicate": True}
    out = []

    def add(name, **kw):
        base = {"name": name, "registry": VP_COS, "routing": None, "desk_rows": [],
                "owner_rows": [], "config": None, "daemon": {}, "steps": []}
        base.update(kw)
        out.append(base)

    # ---- pairing and the private-chat gate ----
    add("pair_with_the_code", config=unpaired(), desk_rows=[],
        steps=[{"op": "process_update", "update": text(1, 42, f"/pair {PAIR}")}])
    add("pair_with_a_wrong_code", config=unpaired(), steps=[
        {"op": "process_update", "update": text(1, 42, "/pair NOPE")}])
    add("pair_ignores_an_unpaired_plain_message", config=unpaired(), steps=[
        {"op": "process_update", "update": text(1, 42, "hello there")}])
    add("group_chat_and_other_chat_are_ignored", config=paired(), daemon={"ingest": ingest_ok}, steps=[
        {"op": "process_update", "update": text(1, 42, "hi", kind="group")},
        {"op": "process_update", "update": text(2, 99, "hi")}])

    # ---- inbound text into the owner mailbox ----
    add("text_question_is_recorded_and_acknowledged", config=paired(), daemon={"ingest": ingest_ok}, steps=[
        {"op": "process_update", "update": text(7, 42, "is the desk up?")}])
    add("redelivered_question_says_already_recorded", config=paired(), daemon={"ingest": ingest_dup}, steps=[
        {"op": "process_update", "update": text(7, 42, "again")}])
    add("start_and_help_get_the_help_text", config=paired(), steps=[
        {"op": "process_update", "update": text(3, 42, "/start")},
        {"op": "process_update", "update": text(4, 42, " /help ")}])
    add("an_empty_message_gets_the_other_items_reply", config=paired(), steps=[
        {"op": "process_update", "update": {"update_id": 5, "message": {"chat": {"id": 42, "type": "private"}, "sticker": {}}}}])
    add("routing_file_names_the_desk", config=paired(), routing={"channel": "vp"},
        daemon={"ingest": ingest_ok}, steps=[{"op": "process_update", "update": text(8, 42, "hello")}])
    add("routing_file_with_an_extra_key_is_refused", config=paired(), routing={"channel": "cos", "extra": 1},
        steps=[{"op": "process_update", "update": text(1, 42, "hi")}])
    add("routing_channel_missing_from_registry_is_refused", config=paired(), routing={"channel": "nope"},
        steps=[{"op": "process_update", "update": text(1, 42, "hi")}])
    add("config_that_still_names_a_channel_is_refused", config=paired(channel="vp"),
        steps=[{"op": "process_update", "update": text(1, 42, "hi")}])

    # ---- the outbox: replies and owner pages ----
    add("a_web_question_produces_nothing_for_telegram", config=paired(last_page_id=5),
        daemon={"outbox": []}, steps=[{"op": "send_owner_outbox"}])
    add("a_telegram_reply_is_sent_and_receipted", config=paired(last_page_id=5), daemon={
        "outbox": [outbox_row("m9", None, "the answer", "q1")]},
        steps=[{"op": "send_owner_outbox"}])
    add("mailbox_page_is_sent_once_and_receipted", config=paired(last_page_id=5), daemon={
        "outbox": [outbox_row("i90", "owner-page", "PAGE OWNER security: token exposed")]},
        steps=[{"op": "send_owner_outbox"}])
    add("mailbox_pages_share_the_budget", config=paired(last_page_id=5), daemon={
        "outbox": [outbox_row(f"i{n}", "owner-page", f"PAGE OWNER outage: incident {n}") for n in range(1, 5)]},
        steps=[{"op": "send_owner_outbox"}])
    add("mailbox_page_does_not_move_the_legacy_watermark", config=paired(last_page_id=5), daemon={
        "outbox": [outbox_row("i90", "owner-page", "PAGE OWNER money: spend cap")]},
        steps=[{"op": "send_owner_outbox"}])
    add("unpaired_bridge_sends_nothing", config=unpaired(), daemon={
        "outbox": [outbox_row("i1", None, "answer", "q")]}, steps=[{"op": "send_owner_outbox"}])

    # ---- the legacy watermark (desk store pages) ----
    add("held_page_is_selected_again", config=paired(last_page_id=10, page_sent_at=[NOW, NOW, NOW]),
        desk_rows=watermark_rows(), steps=[{"op": "send_owner_outbox"},
        {"op": "write_config", "config": paired(last_page_id=10, page_sent_at=[])},
        {"op": "send_owner_outbox"}])
    add("delivered_pages_move_the_watermark", config=paired(last_page_id=10),
        desk_rows=watermark_rows(), steps=[{"op": "send_owner_outbox"}, {"op": "send_owner_outbox"}])
    add("failed_send_does_not_advance_past_the_failed_page", config=paired(last_page_id=10),
        desk_rows=watermark_rows(), fail_on="money", steps=[{"op": "send_owner_outbox"},
        {"op": "set_fail_on", "value": None}, {"op": "send_owner_outbox"}])
    add("pages_in_one_tick_each_spend_the_budget", config=paired(last_page_id=10),
        desk_rows=watermark_rows() + [[15, "cos", "agent", "PAGE OWNER outage: a fourth page"],
                                      [16, "cos", "agent", "PAGE OWNER money: a fifth page"]],
        steps=[{"op": "send_owner_outbox"}, {"op": "send_owner_outbox"}])
    add("page_older_than_the_window_does_not_count", config=paired(last_page_id=10,
        page_sent_at=[NOW - 7200, NOW - 7200]), desk_rows=watermark_rows(),
        steps=[{"op": "send_owner_outbox"}])
    add("first_run_carries_the_old_watermark_over", config=paired(last_brief_id=13),
        desk_rows=watermark_rows(), steps=[{"op": "send_owner_outbox"}])
    add("first_run_with_no_marker_starts_from_now", config=paired(), desk_rows=watermark_rows(),
        steps=[{"op": "send_owner_outbox"}])
    add("ordinary_rows_do_not_starve_a_page", config=paired(last_page_id=10),
        desk_rows=[[100 + n, "cos", "agent", f"an ordinary agent row {n}"] for n in range(25)]
        + [[200, "cos", "agent", "PAGE OWNER security: token exposed"]],
        steps=[{"op": "send_owner_outbox"}])
    add("ordinary_rows_never_move_the_watermark", config=paired(last_page_id=10), desk_rows=[
        [30, "cos", "agent", "a report"], [31, "cos", "agent", "a web echo"],
        [32, "cos", "agent", "PAGE OWNER outage: desk down"]],
        steps=[{"op": "send_owner_outbox"}])

    # ---- the poll loop ----
    add("poll_loop_processes_updates_in_order_and_saves_the_offset", config=paired(update_offset=7),
        daemon={"ingest": ingest_ok, "outbox": []}, steps=[{"op": "run", "batches": [
            [text(7, 42, "one"), text(8, 42, "two")], []]}])
    add("poll_loop_sends_replies_after_each_batch", config=paired(last_page_id=5), daemon={
        "ingest": ingest_ok, "outbox": [outbox_row("m2", None, "answer", "q")]},
        steps=[{"op": "run", "batches": [[text(3, 42, "question")]]}])

    # ---- /status: the delivery state of the last Telegram owner row (t3671) ----
    add("status_of_a_fresh_config", config={"bot_token": "123:TEST-TOKEN-NOT-REAL", "pair_code": PAIR,
        "chat_id": None}, steps=[{"op": "status"}])
    add("status_reports_routing_and_pairing", config=paired(last_brief_id=13, update_offset=900),
        routing={"channel": "vp"}, steps=[{"op": "status"}])
    add("status_with_no_telegram_message_says_nothing_has_reached_the_desk", config=paired(),
        owner_rows=[[11, "cos", "posted", "web:1"]],
        steps=[{"op": "process_update", "update": text(9, 42, "/status")}])
    add("status_reports_the_newest_telegram_owner_row_on_this_channel", config=paired(),
        owner_rows=[[11, "cos", "sent", "telegram:1"], [12, "cos", "inbox", "telegram:2"],
                    [13, "cos", "posted", "web:9"], [14, "vp", "acked", "telegram:3"]],
        # A newer agent row that also carries a telegram: id is NOT an owner message: the
        # Python filters role='owner', so it must not be the row the status reports.
        desk_rows=[[15, "cos", "agent", "a lane row with a telegram id", "telegram:5"]],
        steps=[{"op": "process_update", "update": text(9, 42, " /status ")}])
    add("status_follows_the_routing_channel", config=paired(), routing={"channel": "vp"},
        owner_rows=[[12, "cos", "inbox", "telegram:2"], [14, "vp", "acked", "telegram:3"]],
        steps=[{"op": "process_update", "update": text(9, 42, "/status")}])
    return out


def main(argv=None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--work", required=True, help="a scratch data directory")
    parser.add_argument("--out", required=True, help="where to write the golden JSON")
    args = parser.parse_args(argv)
    work = Path(args.work)
    work.mkdir(parents=True, exist_ok=True)
    os.environ["VP_DATA_DIR"] = str(work)
    desk = install_desk(work)
    import telegram_bridge as bridge  # noqa: E402 - needs the stub desk installed first
    results = [run_scenario(work, scenario, desk, bridge) for scenario in scenarios()]
    Path(args.out).write_text(json.dumps({"now": NOW, "scenarios": results}, indent=2, sort_keys=True) + "\n",
                              encoding="utf-8")
    print(f"wrote {len(results)} scenarios to {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
