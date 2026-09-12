#!/usr/bin/env python3
"""
Dunking Sheep TUI - terminal interface for automated text sending, for herdr.

This is a herdr-native sibling of Dunking Bird. Each "dunker" targets a herdr
pane and sends text through the herdr socket API. Run it in a herdr tab and
point dunkers at your agent panes to keep them engaged with prompts like
"continue" or "keep going".

Since 2.0 the TUI is a *view* onto the Dunking Sheep daemon: every keypress is
the same command an agent would send over the MCP server, the unix socket or
the `dunkingsheep` CLI, and dunks created from any of those appear here live.
Quitting the TUI leaves the dunks running in the daemon.
"""

import curses
import os
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
if HERE not in sys.path:
    sys.path.insert(0, HERE)

from dunk_client import DaemonUnavailable, FlockClient  # noqa: E402
from dunk_core import DunkError, Flock, format_interval  # noqa: E402
from dunk_api import dispatch  # noqa: E402
from text_editor import TextEditorBuffer  # noqa: E402

REFRESH_MS = 200
RECONNECT_S = 2.0


def clip(value, width):
    if width <= 0:
        return ""
    text = str(value)
    if len(text) <= width:
        return text
    if width <= 3:
        return text[:width]
    return text[:width - 3] + "..."


class LocalFlock:
    """In-process fallback with the FlockClient call() interface, used when the
    daemon cannot be started (e.g. the config dir is unwritable)."""

    def __init__(self):
        self.flock = Flock(persist=False, restore=False)
        self.self_pane_id = os.environ.get("HERDR_PANE_ID")

    def call(self, cmd, **args):
        return dispatch(self.flock, cmd, args, self_pane_id=self.self_pane_id)

    def close(self):
        self.flock.close()


class DunkingSheepTui:
    """Main terminal application."""

    def __init__(self, stdscr):
        self.stdscr = stdscr
        self.dunks = []
        self.selected = 0
        self.global_status = ""
        self.message = ""
        self.message_until = 0
        self.quit_requested = False
        self.client = None
        self.mode = "daemon"
        self.last_connect_attempt = 0

        curses.curs_set(0)
        self.stdscr.keypad(True)
        self.stdscr.timeout(REFRESH_MS)
        self.connect()
        self.refresh()

    # -- daemon connection ---------------------------------------------------

    def connect(self):
        self.last_connect_attempt = time.time()
        try:
            self.client = FlockClient.connect_or_start()
            self.mode = "daemon"
        except (DaemonUnavailable, OSError) as error:
            self.client = LocalFlock()
            self.mode = "local"
            self.flash(f"Daemon unavailable ({error}); running in-process")

    def rpc(self, cmd, **args):
        """Run a command against the daemon; errors become a status flash."""
        try:
            return self.client.call(cmd, **args)
        except DunkError as error:
            self.flash(str(error))
        except (DaemonUnavailable, OSError, ValueError) as error:
            self.flash(f"Daemon error: {error}")
            if self.mode == "daemon" and time.time() - self.last_connect_attempt > RECONNECT_S:
                self.connect()
        return None

    def refresh(self):
        result = self.rpc("list_dunks")
        if result is None:
            return
        self.dunks = result.get("dunks", [])
        self.selected = max(0, min(self.selected, len(self.dunks) - 1))
        running = sum(1 for d in self.dunks if d.get("running"))
        n = len(self.dunks)
        label = f"{n} dunker{'s' if n != 1 else ''}"
        if running:
            label += f" ({running} running)"
        if self.mode == "local":
            label += "  [in-process, not shared]"
        self.global_status = label

    # -- main loop ---------------------------------------------------------------

    def run(self):
        while not self.quit_requested:
            self.draw()
            key = self.stdscr.getch()
            if key != -1:
                self.handle_key(key)
            self.refresh()
        self.shutdown()

    def flash(self, text, seconds=4):
        self.message = text
        self.message_until = time.time() + seconds

    def current(self):
        if not self.dunks:
            return None
        return self.dunks[self.selected]

    def current_id(self):
        dunk = self.current()
        if dunk is None:
            self.flash("No dunker selected - press a")
            return None
        return dunk["id"]

    def handle_key(self, key):
        if key in (ord("q"), 27):
            self.quit_requested = True
        elif key == ord("Q"):
            self.quit_and_shutdown()
        elif key in (curses.KEY_UP, ord("k")):
            self.selected = max(0, self.selected - 1)
        elif key in (curses.KEY_DOWN, ord("j")):
            self.selected = min(max(0, len(self.dunks) - 1), self.selected + 1)
        elif key == ord("a"):
            result = self.rpc("add_dunk", start=False)
            if result:
                self.refresh()
                for index, dunk in enumerate(self.dunks):
                    if dunk["id"] == result["id"]:
                        self.selected = index
        elif key == ord("d"):
            dunk_id = self.current_id()
            if dunk_id:
                self.rpc("remove_dunk", id=dunk_id)
        elif key in (ord(" "), ord("s")):
            dunk_id = self.current_id()
            if dunk_id:
                self.rpc("toggle_dunk", id=dunk_id)
        elif key == ord("c"):
            self.pick_target()
        elif key == ord("t"):
            dunk_id = self.current_id()
            if dunk_id:
                self.rpc("fire_dunk", id=dunk_id, countdown_s=2, wait=False)
        elif key == ord("i"):
            self.edit_interval()
        elif key == ord("e"):
            self.edit_text()
        elif key == ord("n"):
            self.edit_name()
        elif key == ord("o"):
            self.toggle_only_idle()
        elif key == ord("m"):
            self.edit_max_sends()

    def quit_and_shutdown(self):
        if self.mode == "daemon":
            self.rpc("shutdown", stop_all=True)
        self.quit_requested = True

    # -- editing -------------------------------------------------------------------

    def edit_interval(self):
        dunk = self.current()
        if dunk is None:
            return self.current_id()
        value = self.line_modal("Interval (minutes, or e.g. 90s / 1.5h)",
                                format_interval(dunk["interval_minutes"]))
        if value is not None and value.strip():
            self.rpc("update_dunk", id=dunk["id"], interval_minutes=value.strip())

    def edit_text(self):
        dunk = self.current()
        if dunk is None:
            return self.current_id()
        new_text = self.text_modal(dunk.get("text") or "")
        if new_text is not None:
            self.rpc("update_dunk", id=dunk["id"], text=new_text.strip())

    def edit_name(self):
        dunk = self.current()
        if dunk is None:
            return self.current_id()
        value = self.line_modal("Dunker name", dunk.get("name") or "")
        if value is not None:
            self.rpc("update_dunk", id=dunk["id"], name=value.strip())

    def toggle_only_idle(self):
        dunk = self.current()
        if dunk is None:
            return self.current_id()
        new_value = "any" if dunk.get("only_when") == "idle" else "idle"
        self.rpc("update_dunk", id=dunk["id"], only_when=new_value)
        self.flash("Sends wait for idle agent" if new_value == "idle" else "Sends on schedule")

    def edit_max_sends(self):
        dunk = self.current()
        if dunk is None:
            return self.current_id()
        current = str(dunk.get("max_sends") or "")
        value = self.line_modal("Max sends (blank = forever)", current)
        if value is None:
            return
        value = value.strip()
        if value in ("", "0"):
            self.rpc("update_dunk", id=dunk["id"], max_sends=0)
        elif value.isdigit():
            self.rpc("update_dunk", id=dunk["id"], max_sends=int(value))
        else:
            self.flash("Max sends must be a whole number")

    def pick_target(self):
        """Choose a herdr pane to target (replaces window capture)."""
        dunk = self.current()
        if dunk is None:
            return self.current_id()
        result = self.rpc("list_panes")
        panes = result.get("panes", []) if result else []
        if not panes:
            status = self.rpc("status")
            self.flash((status or {}).get("herdr_hint") or "No herdr panes")
            return
        chosen = self.target_modal(panes)
        if chosen is not None:
            self.rpc("update_dunk", id=dunk["id"], target=chosen["pane_id"])

    # -- modals ----------------------------------------------------------------------

    def line_modal(self, label, default=""):
        h, w = self.stdscr.getmaxyx()
        box_w = min(max(48, len(label) + 18), max(20, w - 4))
        box_h = 7
        y = max(0, (h - box_h) // 2)
        x = max(0, (w - box_w) // 2)
        win = curses.newwin(box_h, box_w, y, x)
        win.keypad(True)
        win.box()
        self.safe_addstr(win, 1, 2, label, box_w - 4, curses.A_BOLD)
        self.safe_addstr(win, 2, 2, f"Current: {default}", box_w - 4)
        self.safe_addstr(win, 3, 2, "Enter keeps current, Esc cancels", box_w - 4)
        input_w = box_w - 4
        value = ""
        cursor = len(value)
        curses.curs_set(1)
        try:
            while True:
                self.safe_addstr(win, 5, 2, " " * input_w, input_w)
                visible = value[-input_w:] if len(value) > input_w else value
                self.safe_addstr(win, 5, 2, visible, input_w)
                win.move(5, 2 + min(cursor, input_w - 1))
                win.refresh()
                key = win.getch()
                if key in (10, 13, curses.KEY_ENTER):
                    return value or default
                if key in (27,):
                    return None
                if key in (curses.KEY_BACKSPACE, 127, 8):
                    if cursor > 0:
                        value = value[:cursor - 1] + value[cursor:]
                        cursor -= 1
                elif key == curses.KEY_DC:
                    if cursor < len(value):
                        value = value[:cursor] + value[cursor + 1:]
                elif key == curses.KEY_LEFT:
                    cursor = max(0, cursor - 1)
                elif key == curses.KEY_RIGHT:
                    cursor = min(len(value), cursor + 1)
                elif key == curses.KEY_HOME:
                    cursor = 0
                elif key == curses.KEY_END:
                    cursor = len(value)
                elif 32 <= key <= 126:
                    ch = chr(key)
                    value = value[:cursor] + ch + value[cursor:]
                    cursor += 1
        finally:
            curses.curs_set(0)

    # Indentation of tab rows beneath their workspace header.
    TARGET_INDENT = 4

    def target_modal(self, panes):
        """Full-screen picker of herdr panes, grouped by workspace with the tab
        rows indented beneath each workspace name. Returns a pane dict or None."""
        # Land the selection on the first agent pane for convenience.
        sel = 0
        for idx, pane in enumerate(panes):
            if pane.get("agent"):
                sel = idx
                break

        # Build the display: a workspace header row before each new workspace,
        # then one selectable row per pane. `display` items are either
        # ("ws", label) or ("pane", pane_index).
        display = []
        last_ws = object()
        for i, pane in enumerate(panes):
            ws = pane.get("workspace_label") or pane.get("workspace_id") or "?"
            if ws != last_ws:
                display.append(("ws", ws))
                last_ws = ws
            display.append(("pane", i))

        while True:
            h, w = self.stdscr.getmaxyx()
            win = curses.newwin(h, w, 0, 0)
            win.keypad(True)
            win.erase()
            win.box()
            self.safe_addstr(win, 1, 2, "Select target pane", w - 4, curses.A_BOLD)
            self.safe_addstr(win, 2, 2,
                             "j/k or arrows move, Enter selects, Esc cancels", w - 4)
            indent = self.TARGET_INDENT
            header = self._format_target_row("Tab", "Agent", "Status", "Directory", w, indent)
            self.safe_addstr(win, 3, 2 + indent, header, w - 4 - indent, curses.A_BOLD)

            list_top = 5
            visible = max(1, h - list_top - 2)

            # Scroll so the selected pane's display row stays on screen.
            sel_row = next(i for i, d in enumerate(display) if d == ("pane", sel))
            start = 0
            if sel_row >= visible:
                start = sel_row - visible + 1

            for screen_i, drow in enumerate(range(start, min(len(display), start + visible))):
                kind, payload = display[drow]
                y = list_top + screen_i
                if kind == "ws":
                    self.safe_addstr(win, y, 2, payload, w - 4, curses.A_BOLD)
                else:
                    pane = panes[payload]
                    tab = pane.get("tab_label") or pane.get("tab_id") or "?"
                    if pane.get("is_self"):
                        tab = f"{tab} (this)"
                    row = self._format_target_row(
                        tab,
                        pane.get("agent") or "-",
                        pane.get("agent_status") or "-",
                        pane.get("cwd") or "-",
                        w,
                        indent,
                    )
                    attr = curses.A_REVERSE if payload == sel else curses.A_NORMAL
                    self.safe_addstr(win, y, 2 + indent, row, w - 4 - indent, attr)
            win.refresh()

            key = win.getch()
            if key in (27,):
                return None
            if key in (10, 13, curses.KEY_ENTER):
                return panes[sel]
            if key in (curses.KEY_UP, ord("k")):
                sel = max(0, sel - 1)
            elif key in (curses.KEY_DOWN, ord("j")):
                sel = min(len(panes) - 1, sel + 1)

    def _format_target_row(self, tab, agent, status, cwd, width, indent=0):
        avail = width - 4 - indent
        # fixed cols + 3 separators: 20 + 1 + 10 + 1 + 9 + 1 = 42
        dir_w = max(10, avail - 42)
        columns = [
            clip(tab, 20).ljust(20),
            clip(agent, 10).ljust(10),
            clip(status, 9).ljust(9),
            clip(cwd, dir_w).ljust(dir_w),
        ]
        return clip(" ".join(columns), avail)

    def text_modal(self, initial_text):
        h, w = self.stdscr.getmaxyx()
        win = curses.newwin(h, w, 0, 0)
        win.keypad(True)
        text_h = max(1, h - 5)
        text_w = max(1, w - 4)
        edit = curses.newwin(text_h, text_w, 3, 2)
        edit.keypad(True)
        editor = TextEditorBuffer(initial_text)
        top_row = 0
        left_col = 0
        visible_width = max(1, text_w - 1)

        try:
            curses.curs_set(1)
            while True:
                if editor.row < top_row:
                    top_row = editor.row
                elif editor.row >= top_row + text_h:
                    top_row = editor.row - text_h + 1
                if editor.col < left_col:
                    left_col = editor.col
                elif editor.col >= left_col + visible_width:
                    left_col = editor.col - visible_width + 1

                win.erase()
                win.box()
                self.safe_addstr(win, 1, 2, "Edit text to send", w - 4, curses.A_BOLD)
                location = (
                    f"Ctrl+G saves, Esc cancels  "
                    f"Line {editor.row + 1}/{len(editor.lines)}  Col {editor.col + 1}"
                    "  Placeholders: {id} {name} {count} {target} {time}"
                )
                self.safe_addstr(win, 2, 2, location, w - 4)

                edit.erase()
                for screen_row, line_index in enumerate(
                    range(top_row, min(len(editor.lines), top_row + text_h))
                ):
                    # Tabs render as one cell so cursor columns stay stable.
                    segment = editor.lines[line_index][
                        left_col:left_col + visible_width
                    ].replace("\t", " ")
                    try:
                        edit.addnstr(screen_row, 0, segment, visible_width)
                    except curses.error:
                        pass

                cursor_y = editor.row - top_row
                cursor_x = editor.col - left_col
                try:
                    edit.move(cursor_y, min(cursor_x, visible_width - 1))
                except curses.error:
                    pass
                win.refresh()
                edit.refresh()

                try:
                    key = edit.get_wch()
                except curses.error:
                    continue

                if key in (7, "\x07"):
                    return editor.text
                if key in (27, "\x1b"):
                    return None
                if key in (10, 13, "\n", "\r", curses.KEY_ENTER):
                    editor.newline()
                elif key in (9, "\t"):
                    editor.insert(" ")
                elif key in (curses.KEY_BACKSPACE, 127, 8, "\x7f", "\b"):
                    editor.backspace()
                elif key == curses.KEY_DC:
                    editor.delete()
                elif key in (curses.KEY_LEFT, 2):
                    editor.move_left()
                elif key in (curses.KEY_RIGHT, 6):
                    editor.move_right()
                elif key in (curses.KEY_UP, 16):
                    editor.move_up()
                elif key in (curses.KEY_DOWN, 14):
                    editor.move_down()
                elif key in (curses.KEY_HOME, 1):
                    editor.move_home()
                elif key in (curses.KEY_END, 5):
                    editor.move_end()
                elif key == curses.KEY_PPAGE:
                    editor.move_up(text_h)
                elif key == curses.KEY_NPAGE:
                    editor.move_down(text_h)
                elif isinstance(key, str) and key.isprintable():
                    editor.insert(key)
        finally:
            curses.curs_set(0)

    # -- drawing ---------------------------------------------------------------------

    def draw(self):
        self.stdscr.erase()
        h, w = self.stdscr.getmaxyx()
        self.safe_addstr(self.stdscr, 0, 0, "Dunking Sheep TUI", w - 1, curses.A_BOLD)
        help_text = ("a add  d remove  c target  t test  i interval  e text  n name  "
                     "o idle-gate  m max  space start/stop  q quit  Q stop all+quit")
        self.safe_addstr(self.stdscr, 1, 0, help_text, w - 1)
        self.safe_hline(self.stdscr, 2, 0, w - 1)

        header = self.format_row("#", "Status", "Target", "Every", "Sent", "Gate", "Text", w)
        self.safe_addstr(self.stdscr, 3, 0, header, w - 1, curses.A_BOLD)

        visible_rows = max(0, h - 7)
        if not self.dunks:
            self.safe_addstr(self.stdscr, 4, 0,
                             "(no dunkers - press a to add one, or let an agent add one)",
                             w - 1)
        start = 0
        if self.selected >= visible_rows:
            start = self.selected - visible_rows + 1
        for screen_y, index in enumerate(range(start, min(len(self.dunks), start + visible_rows)), start=4):
            d = self.dunks[index]
            sent = str(d.get("send_count") or 0)
            if d.get("max_sends"):
                sent += f"/{d['max_sends']}"
            label = d.get("target_label") or "(no target)"
            if d.get("name"):
                label = f"{d['name']}: {label}"
            row = self.format_row(
                d["id"],
                d.get("status") or "",
                label,
                format_interval(d.get("interval_minutes") or 0),
                sent,
                "idle" if d.get("only_when") == "idle" else "-",
                (d.get("text") or "").replace("\n", " "),
                w,
            )
            attr = curses.A_REVERSE if index == self.selected else curses.A_NORMAL
            self.safe_addstr(self.stdscr, screen_y, 0, row, w - 1, attr)

        self.safe_hline(self.stdscr, h - 3, 0, w - 1)
        bottom = self.global_status
        if self.message and time.time() < self.message_until:
            bottom = f"{bottom}  |  {self.message}"
        self.safe_addstr(self.stdscr, h - 2, 0, bottom, w - 1)
        self.stdscr.refresh()

    def format_row(self, num, status, window, minutes, sent, gate, text, width):
        columns = [
            clip(num, 4).ljust(4),
            clip(status, 18).ljust(18),
            clip(window, 26).ljust(26),
            clip(minutes, 6).rjust(6),
            clip(sent, 6).rjust(6),
            clip(gate, 4).ljust(4),
            clip(text, max(10, width - 70)),
        ]
        row = clip(" ".join(columns), width - 1)
        return row.ljust(max(0, width - 1))

    def safe_hline(self, win, y, x, width):
        if width <= 0:
            return
        try:
            win.hline(y, x, "-", width)
        except curses.error:
            pass

    def safe_addstr(self, win, y, x, value, width, attr=0):
        if width <= 0:
            return
        try:
            max_y, max_x = win.getmaxyx()
            if y < 0 or y >= max_y or x < 0 or x >= max_x:
                return
            usable = min(width, max_x - x - 1)
            if usable <= 0:
                return
            win.addstr(y, x, clip(value, usable).ljust(usable), attr)
        except curses.error:
            pass

    def shutdown(self):
        # Dunks live in the daemon and keep running. Only the in-process
        # fallback has anything to tear down.
        if isinstance(self.client, LocalFlock):
            self.client.close()


def main():
    curses.wrapper(lambda stdscr: DunkingSheepTui(stdscr).run())


if __name__ == "__main__":
    main()
