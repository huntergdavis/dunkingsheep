#!/usr/bin/env python3
"""Private Telegram transport for the existing VP desk and its durable inbox."""

from __future__ import annotations

import argparse
import fcntl
import getpass
import importlib
import json
import os
import mimetypes
import re
import secrets
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from pathlib import Path

desk = importlib.import_module("web.app")
import telegram_transport
from dunk_telegram_routing import resolve_telegram_channel
CONFIG = desk.DATA / "telegram.json"
LOCK = desk.DATA / "telegram.lock"
DOWNLOAD_LIMIT = 20 * 1024 * 1024


def save_config(config: dict):
    desk._private_write(CONFIG, json.dumps(config, indent=2) + "\n")


def load_config() -> dict:
    if not CONFIG.exists():
        raise RuntimeError("Run ./telegram-bridge setup first")
    return json.loads(CONFIG.read_text())


class Telegram:
    def __init__(self, token: str):
        self.base = f"https://api.telegram.org/bot{token}/"
        self.file_base = f"https://api.telegram.org/file/bot{token}/"

    def call(self, method: str, **payload):
        payload = {key: value for key, value in payload.items() if value is not None}
        request = urllib.request.Request(
            self.base + method,
            data=json.dumps(payload).encode(),
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=40) as response:
                answer = json.load(response)
        except urllib.error.HTTPError as exc:
            try:
                description = json.load(exc).get("description", "HTTP error")
            except Exception:
                description = "HTTP error"
            raise RuntimeError(f"Telegram {method}: {description}") from None
        except (urllib.error.URLError, TimeoutError) as exc:
            raise RuntimeError(f"Telegram {method}: network unavailable") from None
        if not answer.get("ok"):
            raise RuntimeError(f"Telegram {method}: {answer.get('description', 'request failed')}")
        return answer["result"]

    def download(self, file_id: str) -> bytes:
        info = self.call("getFile", file_id=file_id)
        if info.get("file_size", 0) > DOWNLOAD_LIMIT:
            raise ValueError("Telegram Bot API cannot download files over 20 MB; use the VP web desk")
        path = urllib.parse.quote(info["file_path"], safe="/")
        try:
            with urllib.request.urlopen(self.file_base + path, timeout=60) as response:
                content = response.read(DOWNLOAD_LIMIT + 1)
        except (urllib.error.HTTPError, urllib.error.URLError, TimeoutError):
            raise RuntimeError("Telegram file download failed") from None
        if len(content) > DOWNLOAD_LIMIT:
            raise ValueError("Telegram Bot API cannot download files over 20 MB; use the VP web desk")
        return content

    def send_file(self, chat_id: int, path: Path, mime: str):
        data = path.read_bytes()
        photo = mime in {"image/jpeg", "image/png"} and len(data) <= 10 * 1024 * 1024
        method, field = ("sendPhoto", "photo") if photo else ("sendDocument", "document")
        boundary = "vptelegram" + uuid.uuid4().hex
        filename = path.name.replace('"', "_").replace("\r", "_").replace("\n", "_")
        body = (
            f"--{boundary}\r\nContent-Disposition: form-data; name=\"chat_id\"\r\n\r\n{chat_id}\r\n"
            f"--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{filename}\"\r\n"
            f"Content-Type: {mime}\r\n\r\n"
        ).encode() + data + f"\r\n--{boundary}--\r\n".encode()
        request = urllib.request.Request(self.base + method, data=body,
                                         headers={"Content-Type": f"multipart/form-data; boundary={boundary}"},
                                         method="POST")
        try:
            with urllib.request.urlopen(request, timeout=90) as response:
                answer = json.load(response)
        except (urllib.error.HTTPError, urllib.error.URLError, TimeoutError):
            raise RuntimeError(f"Telegram {method}: file send failed") from None
        if not answer.get("ok"):
            raise RuntimeError(f"Telegram {method}: {answer.get('description', 'file send failed')}")


def _channel(config: dict) -> str:
    if "channel" in config:
        raise ValueError("Move Telegram channel from credential config to telegram-routing.json")
    return resolve_telegram_channel(desk.DATA, channels_root=desk.ROOT)


def _channel_name(channel: str) -> str:
    return desk.CHANNEL_INDEX[channel]["name"]


def _latest_brief_id(channel: str) -> int:
    with desk._db() as conn:
        return conn.execute("SELECT COALESCE(MAX(id),0) FROM messages WHERE channel=? AND role='agent'",
                            (channel,)).fetchone()[0]


def _prepare_route(config: dict) -> str:
    """Scope the forwarding cursor to its channel; legacy cursors belong to VP."""
    channel = _channel(config)
    previous = config.get("brief_channel", "vp")
    if previous != channel:
        # A switch starts at the destination's current tail, without replaying its
        # history or reusing a partially sent message from the previous channel.
        config["last_brief_id"] = _latest_brief_id(channel)
        config.pop("outbound_message_id", None)
        config.pop("outbound_part", None)
    if previous != channel or "brief_channel" not in config:
        config["brief_channel"] = channel
        save_config(config)
    return channel


def _attachment(message: dict):
    if message.get("document"):
        item = message["document"]
        return item["file_id"], item.get("file_name") or "telegram-file", item.get("mime_type") or "application/octet-stream", item.get("file_size")
    if message.get("photo"):
        item = max(message["photo"], key=lambda photo: photo.get("file_size", 0))
        return item["file_id"], "telegram-photo.jpg", "image/jpeg", item.get("file_size")
    for key, default in (("video", "video.mp4"), ("audio", "audio.mp3"), ("voice", "voice.ogg")):
        if message.get(key):
            item = message[key]
            return item["file_id"], item.get("file_name") or default, item.get("mime_type") or mimetypes.guess_type(default)[0] or "application/octet-stream", item.get("file_size")
    return None


def _save_attachment(filename: str, mime: str, content: bytes) -> str:
    filename = re.sub(r"[^\w. ()-]", "_", Path(filename).name)[:180] or "telegram-file"
    ident = uuid.uuid4().hex
    path = desk.UPLOADS / f"{ident}_{filename}"
    path.write_bytes(content)
    path.chmod(0o600)
    with desk._db() as conn:
        conn.execute("INSERT INTO attachments(id,filename,mime,size,path) VALUES(?,?,?,?,?)",
                     (ident, filename, mime, len(content), str(path)))
    return ident


def _message_status(channel: str) -> str:
    desk._reconcile(channel)
    with desk._db() as conn:
        row = conn.execute("SELECT id,delivery FROM messages WHERE channel=? AND role='owner' "
                           "AND external_id LIKE 'telegram:%' ORDER BY id DESC LIMIT 1", (channel,)).fetchone()
    if not row:
        return f"No Telegram message has reached {_channel_name(channel)} yet."
    return f"{_channel_name(channel)} message #{row['id']}: {row['delivery']}"


def process_update(update: dict, config: dict, bot: Telegram):
    message = update.get("message")
    if not message or message.get("chat", {}).get("type") != "private":
        return
    channel = _channel(config)
    name = _channel_name(channel)
    chat_id = message["chat"]["id"]
    text = message.get("text") or message.get("caption") or ""
    if config.get("chat_id") is None:
        parts = text.strip().split()
        if len(parts) == 2 and parts[0] == "/pair" and secrets.compare_digest(parts[1], config["pair_code"]):
            config["chat_id"] = chat_id
            config["last_brief_id"] = _latest_brief_id(channel)
            config["brief_channel"] = channel
            save_config(config)
            bot.call("sendMessage", chat_id=chat_id, text=f"{name} connected. Send text, a photo, or a file. Replies will appear here. /status checks your last send.")
        return
    if chat_id != config["chat_id"]:
        return
    _prepare_route(config)
    if text.strip() in {"/start", "/help"}:
        bot.call("sendMessage", chat_id=chat_id, text=f"Send a message, photo, or file directly to {name}. /status checks your last send. Files over 20 MB use the web desk.")
        return
    if text.strip() == "/status":
        bot.call("sendMessage", chat_id=chat_id, text=_message_status(channel))
        return
    external_id = f"telegram:{update['update_id']}"
    client = _daemon()
    # t594: the question is a row in the ONE owner mailbox, recorded with the transport
    # it arrived on. The daemon dedupes on external_id, so a redelivered update returns
    # the row it already produced instead of asking the owner's question twice.
    if text.strip():
        ingested = client.call("owner_ingest", body=text, transport="telegram",
                               external_id=external_id, subject=None)
        message_id = ingested["message"]["message_id"]
        again = " (already recorded)" if ingested.get("duplicate") else ""
        bot.call("sendMessage", chat_id=chat_id,
                 text=f"{name} message #{message_id}{again}. I'll reply here when ready.")
        return
    attachments = []
    file_spec = _attachment(message)
    if file_spec:
        file_id, filename, mime, file_size = file_spec
        if file_size is not None and file_size > DOWNLOAD_LIMIT:
            bot.call("sendMessage", chat_id=chat_id,
                     text="Telegram Bot API cannot download files over 20 MB; use the VP web desk.")
            return
        try:
            attachments.append(_save_attachment(filename, mime, bot.download(file_id)))
        except ValueError as exc:
            bot.call("sendMessage", chat_id=chat_id, text=str(exc))
            return
    if not attachments:
        bot.call("sendMessage", chat_id=chat_id, text="I can receive text, photos, and files. Other Telegram items need the web desk.")
        return
    # NAMED GAP: an attachment still lands in the desk's legacy store, not the owner
    # mailbox, because the blob store move is not part of this change.
    item = desk.post_message(desk.MessageIn(channel=channel, body=text, attachment_ids=attachments,
                                             external_id=external_id), "api")
    bot.call("sendMessage", chat_id=chat_id,
             text=f"{name} message #{item['id']}: {item['delivery']}. I’ll reply here when ready.")


def _daemon():
    """The daemon client the bridge talks to (t594: the mailbox lives there).

    A unit proves its role with a service token, so FlockClient() picks it up from
    DUNKINGSHEEP_SERVICE_TOKEN(_FILE); the bridge never opens inbox.db itself.
    """
    from dunk_client import FlockClient
    return FlockClient()


def send_owner_outbox(config: dict, bot: Telegram, client=None):
    """Deliver what Telegram may carry, and nothing else (t594, FLAT_FLEET 7.2).

    Answers to Telegram questions, and pages. The reply list comes from the daemon's
    owner outbox, which only returns REPLIES whose question arrived on this transport,
    so a web answer, a brief, a digest or an echo has no path to the phone. Pages are
    still read from the watchdog's existing rows during the transition (they carry the
    PAGE OWNER marker and the 3-per-hour cap); when the watchdog writes owner rows, the
    same cap applies to those instead.
    """
    chat_id = config.get("chat_id")
    if chat_id is None:
        return []
    client = client or _daemon()
    channel = _prepare_route(config)
    replies = (client.call("owner_outbox", transport="telegram") or {}).get("messages") or []
    pages = _legacy_pages(config, channel)
    now = time.time()
    sent_page_times = [float(at) for at in (config.get("page_sent_at") or [])]
    items = telegram_transport.outbox_items(replies, pages, sent_page_times, now=now)

    def send(body):
        for part in _chunks(body):
            bot.call("sendMessage", chat_id=chat_id, text=part)

    def mark_delivered(message_id, kind):
        if kind == "reply":
            # The receipt the watchdog and the desk read instead of a status file.
            client.call("owner_delivered", message_id=message_id, transport="telegram")
            return
        config["page_sent_at"] = ([at for at in sent_page_times
                                   if now - at < telegram_transport.PAGE_WINDOW_S]
                                  + [now])
        save_config(config)
        _write_status_file(config.get("last_page_id") or 0, channel)

    sent = telegram_transport.deliver(items, send, mark_delivered)
    for item in items:
        if item.get("hold"):
            print(f"telegram held {item['message_id']}: {item['reason']}",
                  file=sys.stderr, flush=True)
    return sent


def _legacy_pages(config: dict, channel: str):
    """Pages the watchdog still writes into the desk store, by its own marker.

    Temporary and named: FLAT_FLEET 7.2 moves pages onto the owner mailbox. Until
    then this keeps PAGES working (the one exception the owner kept) while the brief
    path - which carried reports, digests and web echoes - is gone.
    """
    watermark = config.get("last_page_id") or 0
    with desk._db() as conn:
        rows = conn.execute(
            "SELECT id,body FROM messages WHERE channel=? AND role='agent' AND id>? "
            "ORDER BY id LIMIT 20", (channel, watermark)).fetchall()
    pages = []
    for row in rows:
        config["last_page_id"] = max(watermark, row["id"])
        if telegram_transport.is_page(row["body"]):
            pages.append({"message_id": f"page-{row['id']}", "body": row["body"]})
    save_config(config)
    return pages


def _chunks(body: str, limit: int = 3900):
    while body:
        if len(body) <= limit:
            yield body
            return
        cut = body.rfind("\n", 0, limit)
        if cut < limit // 2:
            cut = limit
        yield body[:cut]
        body = body[cut:].lstrip("\n")


def send_briefs(config: dict, bot: Telegram):
    """Retired by t594: the brief path carried reports, digests and web echoes.

    Kept as a name so a caller that still says `send_briefs` gets the owner outbox
    instead of the old firehose.
    """
    return send_owner_outbox(config, bot)


def _write_status_file(last_brief_id: int, channel: str) -> None:
    """Publish the forwarding watermark in a file that holds no secrets.

    The exception watchdog must confirm that an owner page was actually forwarded
    before it records it as delivered, and it is not allowed to read telegram.json
    (which holds the bot token). This file carries the last forwarded brief id, its
    channel and when it was written. Consumers must match the channel as well as the message id.
    Written atomically so a reader can never see a partial file.
    """
    path = desk.DATA / "telegram-status.json"
    payload = {"last_brief_id": int(last_brief_id), "channel": channel, "updated_at": time.time()}
    # Atomic replace through the desk's own writer, then relax the mode: the file
    # holds no secret, and the watchdog (a different process) must be able to read it.
    desk._private_write(path, json.dumps(payload) + "\n")
    os.chmod(path, 0o644)


def setup():
    if CONFIG.exists():
        raise RuntimeError("Telegram is already configured; inspect ./telegram-bridge status")
    token = getpass.getpass("BotFather token (hidden): ").strip()
    if not token or any(ch.isspace() for ch in token):
        raise RuntimeError("Token must be a single nonempty value")
    bot = Telegram(token)
    info = bot.call("getMe")
    config = {"bot_token": token, "pair_code": secrets.token_urlsafe(18), "chat_id": None,
              "update_offset": None, "last_brief_id": None}
    save_config(config)
    print(f"Bot @{info['username']} saved in private local data.\nStart ./install-telegram-service.sh, then DM the bot: /pair {config['pair_code']}")


def run():
    with LOCK.open("a+") as lock:
        LOCK.chmod(0o600)
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise RuntimeError("Telegram bridge is already running") from None
        config = load_config()
        _prepare_route(config)
        bot = Telegram(config["bot_token"])
        webhook = bot.call("getWebhookInfo")
        if webhook.get("url"):
            raise RuntimeError("This bot has a webhook; remove it before using long polling")
        print(f"Telegram bridge running on {_channel(config)}; waiting for the paired private chat", flush=True)
        while True:
            try:
                updates = bot.call("getUpdates", offset=config.get("update_offset"), timeout=10,
                                   allowed_updates=["message"])
                for update in updates:
                    process_update(update, config, bot)
                    config["update_offset"] = update["update_id"] + 1
                    save_config(config)
                send_briefs(config, bot)
            except (RuntimeError, ValueError, OSError) as exc:
                # Telegram URLs include the secret; do not print arbitrary exception strings.
                print(f"Telegram bridge retrying after {type(exc).__name__}", file=sys.stderr, flush=True)
                time.sleep(5)


def main():
    parser = argparse.ArgumentParser(description="Private Telegram bridge for the VP desk")
    parser.add_argument("command", choices=["setup", "run", "status"])
    args = parser.parse_args()
    try:
        if args.command == "setup":
            setup()
        elif args.command == "run":
            run()
        else:
            config = load_config()
            print(f"channel={_channel(config)}; brief_channel={config.get('brief_channel', 'vp')}; paired={'yes' if config.get('chat_id') is not None else 'no'}; last_brief_id={config.get('last_brief_id')}; update_offset={config.get('update_offset')}")
    except (RuntimeError, ValueError, OSError, KeyboardInterrupt) as exc:
        if not isinstance(exc, KeyboardInterrupt):
            print(str(exc), file=sys.stderr)
            sys.exit(1)


if __name__ == "__main__":
    main()
