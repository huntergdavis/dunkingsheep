#!/usr/bin/env python3
"""
Dunking Sheep TUI - terminal interface for automated text sending, for herdr.

This is a herdr-native sibling of Dunking Bird. Instead of capturing an OS
window and typing with ydotool, each "dunker" targets a herdr pane and sends
text through the herdr socket API (via the `herdr` CLI). Run it in a herdr tab
and point dunkers at your agent panes to keep them engaged with prompts like
"continue" or "keep going".

Supports multiple concurrent dunkers, herdr pane targeting, test sends, custom
text, and live countdowns.
"""

import curses
from curses import textpad
import threading
import time

from herdr_client import HerdrClient


def clip(value, width):
    if width <= 0:
        return ""
    text = str(value)
    if len(text) <= width:
        return text
    if width <= 3:
        return text[:width]
    return text[:width - 3] + "..."


class DunkerTuiRow:
    """One dunking sheep instance shown as one row in the terminal UI."""

    def __init__(self, app, row_num):
        self.app = app
        self.row_num = row_num
        self.is_running = False
        self.timer_thread = None
        self.interval_minutes = "10.0"
        self.interval_seconds = 600
        self.text_value = "continue"
        self.status = "Ready"

        # herdr target (replaces the captured OS window)
        self.target_pane_id = None
        self.target_agent = None
        self.target_cwd = None
        self._lock = threading.Lock()

    def set_status(self, value):
        with self._lock:
            self.status = value

    def get_status(self):
        with self._lock:
            return self.status

    def window_label(self):
        if not self.target_pane_id:
            return "(no target)"
        if self.target_agent:
            return f"{self.target_agent} [{self.target_pane_id}]"
        return self.target_pane_id

    def text_preview(self):
        return self.text_value.replace("\n", " ")

    def toggle_running(self):
        if self.is_running:
            self.stop()
        else:
            self.start()

    def start(self):
        try:
            mins = float(self.interval_minutes)
            if mins <= 0:
                raise ValueError
            self.interval_seconds = mins * 60
        except ValueError:
            self.set_status("Bad interval!")
            return

        if not self.target_pane_id:
            self.set_status("No target - press c")
            return

        if self.is_running:
            return
        self.is_running = True
        self.timer_thread = threading.Thread(target=self._timer_loop, daemon=True)
        self.timer_thread.start()
        self.app.update_count()

    def stop(self):
        self.is_running = False
        self.set_status("Stopped")
        self.app.update_count()

    def destroy(self):
        self.stop()

    def set_target(self, pane):
        """Point this dunker at a herdr pane dict from `herdr pane list`."""
        self.target_pane_id = pane.get("pane_id")
        self.target_agent = pane.get("agent")
        self.target_cwd = pane.get("cwd")
        self.set_status("Target set")

    def test_send(self):
        threading.Thread(target=self._test_send_worker, daemon=True).start()

    def _test_send_worker(self):
        try:
            if not self.target_pane_id:
                self.set_status("No target - press c")
                return
            for i in range(2, 0, -1):
                self.set_status(f"Test in {i}...")
                time.sleep(1)
            with self.app.send_lock:
                self.set_status("Sending...")
                ok = self._do_send()
            t = time.strftime("%H:%M:%S")
            self.set_status(f"Tested {t}" if ok else "Test failed")
        except Exception as e:
            self.set_status("Test failed")
            print(f"Test error dunker #{self.row_num}: {e}")

    def _timer_loop(self):
        while self.is_running:
            try:
                mins = float(self.interval_minutes)
                total = max(1, int(mins * 60))
            except ValueError:
                total = int(self.interval_seconds)

            for tick in range(total):
                if not self.is_running:
                    return
                rem = total - tick
                m, s = divmod(rem, 60)
                self.set_status(f"Next: {m:02d}:{s:02d}")
                time.sleep(1)

            if not self.is_running:
                return

            self.set_status("Waiting...")
            with self.app.send_lock:
                if not self.is_running:
                    return
                self.set_status("Sending...")
                ok = self._do_send()

            if self.is_running:
                t = time.strftime("%H:%M:%S")
                self.set_status(f"Sent {t}" if ok else "Send failed!")
                time.sleep(1)

    def _do_send(self):
        text = self.text_value.strip()
        if not text:
            return True
        ok, msg = self.app.herdr.send_text_and_enter(self.target_pane_id, text)
        if ok:
            self.app.set_global_status(f"Sent to {self.window_label()}")
        else:
            self.app.set_global_status(f"Send failed: {msg}")
        return ok


class DunkingSheepTui:
    """Main terminal application."""

    def __init__(self, stdscr):
        self.stdscr = stdscr
        self.herdr = HerdrClient()
        self.dunkers = []
        self.selected = 0
        self.send_lock = threading.Lock()
        self.status_lock = threading.Lock()
        self.global_status = ""
        self.quit_requested = False

        curses.curs_set(0)
        self.stdscr.keypad(True)
        self.stdscr.timeout(200)
        self.add_dunker()
        self.runtime_checks()

    def run(self):
        while not self.quit_requested:
            self.draw()
            key = self.stdscr.getch()
            if key != -1:
                self.handle_key(key)
        self.shutdown()

    def add_dunker(self):
        self.dunkers.append(DunkerTuiRow(self, len(self.dunkers) + 1))
        self.selected = len(self.dunkers) - 1
        self.update_count()

    def remove_dunker(self):
        if not self.dunkers:
            return
        d = self.dunkers.pop(self.selected)
        d.destroy()
        for index, dunker in enumerate(self.dunkers, start=1):
            dunker.row_num = index
        self.selected = max(0, min(self.selected, len(self.dunkers) - 1))
        if not self.dunkers:
            self.add_dunker()
        self.update_count()

    def update_count(self):
        n = len(self.dunkers)
        running = sum(1 for d in self.dunkers if d.is_running)
        if running:
            self.set_global_status(f"{n} dunker{'s' if n != 1 else ''} ({running} running)")
        else:
            self.set_global_status(f"{n} dunker{'s' if n != 1 else ''}")

    def set_global_status(self, value):
        with self.status_lock:
            self.global_status = value

    def get_global_status(self):
        with self.status_lock:
            return self.global_status

    def runtime_checks(self):
        try:
            if not self.herdr.is_available():
                self.set_global_status(self.herdr.server_error_hint())
            else:
                self.update_count()
        except Exception as e:
            self.set_global_status(f"Check error: {e}")

    def handle_key(self, key):
        if key in (ord("q"), 27):
            self.quit_requested = True
        elif key in (curses.KEY_UP, ord("k")):
            self.selected = max(0, self.selected - 1)
        elif key in (curses.KEY_DOWN, ord("j")):
            self.selected = min(len(self.dunkers) - 1, self.selected + 1)
        elif key == ord("a"):
            self.add_dunker()
        elif key == ord("d"):
            self.remove_dunker()
        elif key in (ord(" "), ord("s")):
            self.current().toggle_running()
        elif key == ord("c"):
            self.pick_target()
        elif key == ord("t"):
            self.current().test_send()
        elif key == ord("i"):
            self.edit_interval()
        elif key == ord("e"):
            self.edit_text()

    def current(self):
        return self.dunkers[self.selected]

    def edit_interval(self):
        d = self.current()
        value = self.line_modal("Interval minutes", d.interval_minutes)
        if value is not None:
            value = value.strip() or d.interval_minutes
            try:
                if float(value) <= 0:
                    raise ValueError
            except ValueError:
                d.set_status("Bad interval!")
                return
            d.interval_minutes = value
            d.set_status("Interval set")

    def edit_text(self):
        d = self.current()
        new_text = self.text_modal(d.text_value)
        if new_text is not None:
            d.text_value = new_text.strip()
            d.set_status("Text set")

    def pick_target(self):
        """Choose a herdr pane to target (replaces window capture)."""
        panes = self.herdr.list_panes()
        if not panes:
            self.current().set_status("No herdr panes")
            self.set_global_status(self.herdr.server_error_hint())
            return
        chosen = self.target_modal(panes)
        if chosen is not None:
            self.current().set_target(chosen)

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

    def target_modal(self, panes):
        """Full-screen selectable list of herdr panes. Returns a pane dict or None."""
        sel = 0
        # Prefer to land on agent panes first for convenience.
        for idx, pane in enumerate(panes):
            if pane.get("agent"):
                sel = idx
                break
        while True:
            h, w = self.stdscr.getmaxyx()
            win = curses.newwin(h, w, 0, 0)
            win.keypad(True)
            win.erase()
            win.box()
            self.safe_addstr(win, 1, 2, "Select target pane", w - 4, curses.A_BOLD)
            self.safe_addstr(win, 2, 2,
                             "j/k or arrows move, Enter selects, Esc cancels", w - 4)
            header = self._format_target_row("Pane", "Agent", "Status", "Directory", w)
            self.safe_addstr(win, 4, 2, header, w - 4, curses.A_BOLD)

            list_top = 5
            visible = max(1, h - list_top - 2)
            start = 0
            if sel >= visible:
                start = sel - visible + 1
            for screen_i, index in enumerate(range(start, min(len(panes), start + visible))):
                pane = panes[index]
                row = self._format_target_row(
                    pane.get("pane_id", "?"),
                    pane.get("agent") or "-",
                    pane.get("agent_status") or "-",
                    pane.get("cwd") or "-",
                    w,
                )
                attr = curses.A_REVERSE if index == sel else curses.A_NORMAL
                self.safe_addstr(win, list_top + screen_i, 2, row, w - 4, attr)
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

    def _format_target_row(self, pane, agent, status, cwd, width):
        columns = [
            clip(pane, 12).ljust(12),
            clip(agent, 12).ljust(12),
            clip(status, 9).ljust(9),
            clip(cwd, max(10, width - 44)),
        ]
        return clip(" ".join(columns), width - 4)

    def text_modal(self, initial_text):
        h, w = self.stdscr.getmaxyx()
        win = curses.newwin(h, w, 0, 0)
        win.keypad(True)
        text_h = max(1, h - 5)
        text_w = max(1, w - 4)
        edit = curses.newwin(text_h, text_w, 3, 2)
        edit.keypad(True)
        edit.scrollok(True)

        cancelled = False

        def validate(ch):
            nonlocal cancelled
            if ch == 27:
                cancelled = True
                return 7
            if ch in (9,):
                return ord(" ")
            return ch

        try:
            win.erase()
            win.box()
            self.safe_addstr(win, 1, 2, "Edit text to send", w - 4, curses.A_BOLD)
            self.safe_addstr(win, 2, 2, "Ctrl+G saves, Esc cancels", w - 4)
            for idx, line in enumerate(initial_text.splitlines() or [""]):
                if idx >= text_h:
                    break
                self.safe_addstr(edit, idx, 0, line, text_w)
            win.refresh()
            edit.refresh()
            curses.curs_set(1)
            box = textpad.Textbox(edit, insert_mode=True)
            text = box.edit(validate)
            if cancelled:
                return None
            return text.rstrip()
        finally:
            curses.curs_set(0)

    def draw(self):
        self.stdscr.erase()
        h, w = self.stdscr.getmaxyx()
        self.safe_addstr(self.stdscr, 0, 0, "Dunking Sheep TUI", w - 1, curses.A_BOLD)
        help_text = "a add  d remove  c target  t test  i interval  e text  space start/stop  q quit"
        self.safe_addstr(self.stdscr, 1, 0, help_text, w - 1)
        self.safe_hline(self.stdscr, 2, 0, w - 1)

        header = self.format_row("#", "Status", "Target", "Min", "Text", w)
        self.safe_addstr(self.stdscr, 3, 0, header, w - 1, curses.A_BOLD)

        visible_rows = max(0, h - 7)
        start = 0
        if self.selected >= visible_rows:
            start = self.selected - visible_rows + 1
        for screen_y, index in enumerate(range(start, min(len(self.dunkers), start + visible_rows)), start=4):
            d = self.dunkers[index]
            row = self.format_row(
                str(d.row_num),
                d.get_status(),
                d.window_label(),
                d.interval_minutes,
                d.text_preview(),
                w,
            )
            attr = curses.A_REVERSE if index == self.selected else curses.A_NORMAL
            self.safe_addstr(self.stdscr, screen_y, 0, row, w - 1, attr)

        self.safe_hline(self.stdscr, h - 3, 0, w - 1)
        self.safe_addstr(self.stdscr, h - 2, 0, self.get_global_status(), w - 1)
        self.stdscr.refresh()

    def format_row(self, num, status, window, minutes, text, width):
        columns = [
            clip(num, 4).ljust(4),
            clip(status, 18).ljust(18),
            clip(window, 28).ljust(28),
            clip(minutes, 7).rjust(7),
            clip(text, max(10, width - 62)),
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
        for dunker in self.dunkers:
            dunker.destroy()


def main():
    curses.wrapper(lambda stdscr: DunkingSheepTui(stdscr).run())


if __name__ == "__main__":
    main()
