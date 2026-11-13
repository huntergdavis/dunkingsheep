#!/usr/bin/env python3
"""Golden replay of the Python Telegram bridge, on fakes only (t3231 parity oracle).

Runs `bridges/telegram_bridge.py` (and its `telegram_transport` / `dunk_telegram_routing` modules)
against a stub desk, a fake Telegram, a fake daemon and a fixed clock, for each scenario below. It
writes the scenario INPUTS together with the OBSERVED outputs (every Telegram call, every daemon
call, desk message and attachment snapshots, the final telegram.json and status file, each step's
result or exception) to a JSON file.
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
import math
import os
import shutil
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

    class MessageIn:
        def __init__(self, *, channel, body, attachment_ids, external_id):
            self.channel = channel
            self.body = body
            self.attachment_ids = attachment_ids
            self.external_id = external_id

    def post_message(data, identity):
        assert identity == "api"
        with _db() as conn:
            cursor = conn.execute(
                "INSERT INTO messages(channel,role,body,delivery,external_id) "
                "VALUES(?, 'owner', ?, 'posted', ?)",
                (data.channel, data.body.strip(), data.external_id),
            )
            message_id = cursor.lastrowid
            for attachment_id in data.attachment_ids:
                conn.execute("UPDATE attachments SET message_id=? WHERE id=?",
                             (message_id, attachment_id))
        return {"id": message_id, "delivery": "posted"}

    desk.MessageIn = MessageIn
    desk.post_message = post_message

    def attachment_snapshot():
        with _db() as conn:
            rows = conn.execute(
                "SELECT m.id,m.channel,m.body,m.delivery,m.external_id,"
                "a.filename,a.mime,a.size,a.path FROM messages m "
                "JOIN attachments a ON a.message_id=m.id ORDER BY m.id,a.rowid"
            ).fetchall()
        messages = {}
        for row in rows:
            message = messages.setdefault(row["id"], {
                "message_id": row["id"], "channel": row["channel"], "body": row["body"],
                "delivery": row["delivery"], "external_id": row["external_id"], "attachments": [],
            })
            message["attachments"].append({
                "filename": row["filename"], "mime": row["mime"], "size": row["size"],
                "content": Path(row["path"]).read_bytes().decode("utf-8"),
            })
        return list(messages.values())

    desk.attachment_snapshot = attachment_snapshot
    package = types.ModuleType("web")
    sys.modules["web"] = package
    sys.modules["web.app"] = desk

    # The production shared reader was retired with the Python daemon sources. Keep the exact
    # routing contract needed by this frozen bridge oracle as a fake module (e8bbeebf).
    routing = types.ModuleType("dunk_telegram_routing")

    def resolve_telegram_channel(data_dir, *, channels_root=None):
        data_dir = Path(data_dir)
        route_path = data_dir / "telegram-routing.json"
        config = json.loads(route_path.read_text(encoding="utf-8")) if route_path.exists() else {}
        if not isinstance(config, dict) or set(config) - {"channel"}:
            raise ValueError("Telegram routing config must contain only channel")
        channel = os.environ.get("VP_TELEGRAM_CHANNEL", config.get("channel", "cos"))
        channels_path = os.environ.get("VP_CHANNELS_FILE")
        if channels_path:
            registry_path = Path(channels_path).expanduser()
        else:
            root = Path(channels_root) if channels_root is not None else data_dir.parent
            registry_path = root / "channels.json"
            if not registry_path.exists():
                registry_path = root / "channels.example.json"
        channels = json.loads(registry_path.read_text(encoding="utf-8"))
        if not isinstance(channels, list) or not all(
            isinstance(row, dict) and isinstance(row.get("id"), str) for row in channels
        ):
            raise ValueError("Telegram desk channel registry is invalid")
        if not isinstance(channel, str) or channel not in {row["id"] for row in channels}:
            raise ValueError("Telegram channel must name a configured desk channel")
        return channel

    routing.resolve_telegram_channel = resolve_telegram_channel
    sys.modules["dunk_telegram_routing"] = routing
    return desk


class FakeBot:
    def __init__(self, webhook_url="", files=None):
        self.log = []
        self.fail_on = None
        self.batches = []
        self.webhook_url = webhook_url
        self.files = files or {}

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
        if method == "getFile":
            file = self.files.get(payload.get("file_id"), {})
            return {key: file[key] for key in ("file_path", "file_size") if key in file}
        return {}

    def download(self, file_id):
        info = self.call("getFile", file_id=file_id)
        if info.get("file_size", 0) > 20 * 1024 * 1024:
            raise ValueError("Telegram Bot API cannot download files over 20 MB; use the VP web desk.")
        content = self.files[file_id]["content"].encode("utf-8")
        if len(content) > 20 * 1024 * 1024:
            raise ValueError("Telegram Bot API cannot download files over 20 MB; use the VP web desk.")
        return content


class FakeClient:
    """The daemon: an owner outbox, an ingest answer, the owner mail conversation, and a log.

    `owner_conversation` exists because `/status` in the RUST port reads the owner mailbox. The
    Python bridge answers `/status` from the desk store instead, so it never calls this; that is
    the one deliberate difference between the two bridges (t3671/t3672, owner 2026-11-08), and the
    fixture records it by carrying the port's own expectations in `port`.
    """

    def __init__(self, outbox, ingest, conversation=None):
        self.log = []
        self.outbox = outbox
        self.ingest = ingest
        self.conversation = conversation or []

    def call(self, name, **kwargs):
        self.log.append([name, float_tree(kwargs)])
        if name == "owner_outbox":
            return {"messages": list(self.outbox)}
        if name == "owner_ingest":
            return self.ingest
        if name == "owner_conversation":
            # The daemon's contract: `created_at < before`, newest `limit` of those, oldest first.
            limit = int(kwargs.get("limit") or 50)
            before = kwargs.get("before")
            rows = [dict(row) for row in self.conversation
                    if before is None or float(row.get("created_at") or 0.0) < float(before)]
            rows = rows[-limit:]
            return {"messages": rows, "count": len(rows)}
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
    if desk.UPLOADS.exists():
        shutil.rmtree(desk.UPLOADS)
    desk.UPLOADS.mkdir(parents=True, exist_ok=True)
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
    conn.execute("CREATE TABLE IF NOT EXISTS attachments (id TEXT PRIMARY KEY, filename TEXT NOT NULL, "
                 "mime TEXT NOT NULL, size INTEGER NOT NULL, path TEXT NOT NULL, message_id INTEGER)")
    conn.execute("DELETE FROM messages")
    conn.execute("DELETE FROM attachments")
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
    bot = FakeBot(scenario.get("webhook_url", ""), scenario.get("files", {}))
    bot.fail_on = scenario.get("fail_on")
    daemon_spec = scenario.get("daemon", {})
    client = FakeClient(daemon_spec.get("outbox", []), daemon_spec.get("ingest"),
                       daemon_spec.get("conversation"))
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
    # `expected` stays exactly what the PYTHON bridge did. A scenario that records an intentional
    # difference carries it in its own input as `port` ({bot_log, daemon_log}), so the fixture holds
    # both answers side by side: the Python one here, the port's beside its inputs. Nothing is
    # normalised, so drift in any other key still fails the replay.
    return {
        "name": scenario["name"],
        "input": scenario,
        "expected": {
            "steps": steps_out,
            "bot_log": bot.log,
            "daemon_log": client.log,
            "config": read_json(work / "telegram.json"),
            "status": status,
            "desk_attachments": desk.attachment_snapshot(),
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


def mailbox_row(row_id, transport, state, external_id=None, reply_to=None, created_at=0.0):
    """One owner-mailbox row, as the daemon returns it in `owner_conversation`.

    `created_at` is the paging cursor; give rows distinct values when a page boundary matters.
    """
    return {"id": row_id, "transport": transport, "state": state,
            "external_id": external_id, "reply_to": reply_to, "created_at": created_at}


def status_reply(text):
    """The Telegram log a `/status` answer produces: exactly one sendMessage."""
    return [["sendMessage", {"chat_id": 42, "text": text}]]


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
    add("photo_with_caption_is_saved_as_a_desk_attachment", config=paired(), files={
        "photo-small": {"file_path": "photos/small.jpg", "file_size": 10, "content": "small photo"},
        "photo-large": {"file_path": "photos/large.jpg", "file_size": 12, "content": "photo bytes"},
    }, steps=[{"op": "process_update", "update": {
        "update_id": 20,
        "message": {"chat": {"id": 42, "type": "private"}, "caption": "look at this",
                    "photo": [{"file_id": "photo-small", "file_size": 10},
                              {"file_id": "photo-large", "file_size": 12}]},
    }}])
    add("document_caption_and_sanitized_filename_are_saved", config=paired(), files={
        "document-1": {"file_path": "documents/report.txt", "file_size": 13, "content": "document bytes"},
    }, steps=[{"op": "process_update", "update": {
        "update_id": 21,
        "message": {"chat": {"id": 42, "type": "private"}, "caption": "please review",
                    "document": {"file_id": "document-1", "file_name": "../../report?.txt",
                                 "mime_type": "text/plain", "file_size": 13}},
    }}])
    add("oversized_attachment_is_refused_without_download", config=paired(), steps=[
        {"op": "process_update", "update": {
            "update_id": 22,
            "message": {"chat": {"id": 42, "type": "private"}, "caption": "large file",
                        "document": {"file_id": "too-large", "file_name": "large.bin",
                                     "file_size": 20 * 1024 * 1024 + 1}},
        }}])
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

    # ---- /status: answered from the owner mailbox by the port, from the desk store by Python ----
    #
    # t3671 filed the gap; t3672 wrote the wording; the owner chose it on 2026-11-08 ("b I want it
    # to be useful", routed i50810/i50819) and accepted the consequence: this bridge records its
    # questions in the one owner mailbox, so the mailbox is what it can honestly report. Python
    # reads the legacy desk store. Each scenario below therefore carries `port`: the port's own
    # expected bot_log/daemon_log. Whatever Python observed stays in the fixture beside it, so the
    # difference is on the record instead of being normalised away.
    status_call = [["owner_conversation", {"limit": 100}]]
    add("status_of_a_fresh_config", config={"bot_token": "123:TEST-TOKEN-NOT-REAL", "pair_code": PAIR,
        "chat_id": None}, steps=[{"op": "status"}])
    add("status_reports_routing_and_pairing", config=paired(last_brief_id=13, update_offset=900),
        routing={"channel": "vp"}, steps=[{"op": "status"}])
    add("status_with_no_recorded_question_says_nothing_has_reached_the_desk", config=paired(),
        daemon={"conversation": []},
        steps=[{"op": "process_update", "update": text(9, 42, "/status")}],
        port={"daemon_log": status_call,
              "bot_log": status_reply("No Telegram message has reached Chief of Staff yet.")})
    add("status_answers_from_the_mailbox_where_python_has_no_desk_row", config=paired(),
        # The live case: a question is in the mailbox, the desk store has nothing (t594 moved the
        # question there). Python answers the empty-store line; the port answers the real row.
        daemon={"conversation": [mailbox_row("i7", "telegram", "queued", "telegram:7")]},
        steps=[{"op": "process_update", "update": text(9, 42, "/status")}],
        port={"daemon_log": status_call,
              "bot_log": status_reply("Chief of Staff message #i7: queued")})
    add("status_reports_the_mailbox_state_not_the_desk_delivery", config=paired(),
        # Same row id in both stores, different state word: Python says `sent` (desk delivery),
        # the port must say `queued` (mailbox state). A newer web question and a newer reply are
        # in the conversation and must not be reported.
        owner_rows=[[12, "cos", "sent", "telegram:2"]],
        daemon={"conversation": [
            mailbox_row("i12", "telegram", "queued", "telegram:2"),
            mailbox_row("i13", "web", "queued", "web:9"),
            mailbox_row("i14", "telegram", "acked", None, "i12"),
            mailbox_row("i16", "telegram", "acked", "telegram:900", "i12")]},
        steps=[{"op": "process_update", "update": text(9, 42, " /status ")}],
        port={"daemon_log": status_call,
              "bot_log": status_reply("Chief of Staff message #i12: queued")})
    add("status_names_the_routing_channel_and_ignores_another_desks_question", config=paired(),
        routing={"channel": "vp"},
        owner_rows=[[14, "vp", "acked", "telegram:3"]],
        daemon={"conversation": [
            mailbox_row("i30", "telegram", "read", "telegram:30")]},
        steps=[{"op": "process_update", "update": text(9, 42, "/status")}],
        port={"daemon_log": status_call,
              "bot_log": status_reply("VP message #i30: read")})
    add("status_finds_a_question_older_than_unrelated_mailbox_traffic", config=paired(),
        # Reviewers t3745 and t3762: the conversation also carries owner-lane mail, so a fixed
        # window can fill with unrelated rows. 130 newer rows than the question force a second
        # page, and the first page's boundary timestamp is proved before the walk goes below it.
        daemon={"conversation": [mailbox_row("i7", "telegram", "queued", "telegram:7",
                                             created_at=1.0)]
                + [mailbox_row(f"web-{n}", "web", "queued", f"web:{n}", created_at=10.0 + n)
                   for n in range(130)]},
        steps=[{"op": "process_update", "update": text(9, 42, "/status")}],
        port={"daemon_log": status_call
              # The cluster probe: `next_up(40.0)` is the smallest float above the page's oldest
              # timestamp, so `created_at < that` shows every row at 40.0 (one here).
              + [["owner_conversation", {"limit": 100, "before": math.nextafter(40.0, math.inf)}],
                 ["owner_conversation", {"limit": 100, "before": 40}]],
              "bot_log": status_reply("Chief of Staff message #i7: queued")})
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
