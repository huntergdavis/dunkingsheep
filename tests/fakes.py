"""Test doubles shared by the Dunking Sheep test suite."""

import json
import re
import threading

ANSI_RE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")


PANES = [
    {"pane_id": "w1:p1", "tab_id": "w1:t1", "workspace_id": "w1", "cwd": "/home/x",
     "agent_status": "unknown", "terminal_id": "term_a",
     "terminal_title_stripped": "shell"},
    {"pane_id": "w1:p2", "tab_id": "w1:t2", "workspace_id": "w1", "cwd": "/home/x/proj",
     "agent": "claude", "agent_status": "working", "terminal_id": "term_b",
     "terminal_title_stripped": "claude proj"},
    {"pane_id": "w2:p1", "tab_id": "w2:t1", "workspace_id": "w2", "cwd": "/home/x/site",
     "agent": "codex", "agent_status": "idle", "terminal_id": "term_c",
     "terminal_title_stripped": "site"},
    # A plain shell whose tab is literally named "Sheep", next to an agent pane
    # whose window title merely mentions sheep.
    {"pane_id": "w2:p2", "tab_id": "w2:t2", "workspace_id": "w2", "cwd": "/home/x/sheep",
     "agent_status": "unknown", "terminal_id": "term_d",
     "terminal_title_stripped": "shell"},
    {"pane_id": "w2:p3", "tab_id": "w2:t3", "workspace_id": "w2", "cwd": "/home/x/other",
     "agent": "claude", "agent_status": "working", "terminal_id": "term_e",
     "terminal_title_stripped": "Sheep release work"},
]
TABS = [
    {"tab_id": "w1:t1", "label": "shell", "number": 1},
    {"tab_id": "w1:t2", "label": "Claude Proj", "number": 2},
    {"tab_id": "w2:t1", "label": "Codex Site", "number": 1},
    {"tab_id": "w2:t2", "label": "Sheep", "number": 2},
    {"tab_id": "w2:t3", "label": "Other", "number": 3},
]
WORKSPACES = [
    {"workspace_id": "w1", "label": "alpha", "number": 1},
    {"workspace_id": "w2", "label": "beta", "number": 2},
]


class FakeHerdr:
    """Stands in for HerdrClient: records sends, serves canned panes."""

    def __init__(self, panes=None, available=True):
        self.panes = [dict(p) for p in (panes or PANES)]
        self.tabs = [dict(t, workspace_id=t['tab_id'].split(':')[0]) for t in TABS]
        self.workspaces = [dict(w) for w in WORKSPACES]
        self.available = available
        # Layout mutations, for assertions: ("create_tab", {...}), ("close", "tab", "w2:t2"), ...
        self.layout_calls = []
        self.runs = []          # (pane_id, command) typed via run_command
        self.raw_runs = []      # argv lists sent through _run (the herdr passthrough)
        self.raw_responses = {}  # tuple(argv) -> (code, stdout, stderr)
        self.fail_layout = None  # message: make the next mutation fail
        self._counter = 100
        self.sends = []
        self.statuses = {}
        self.screens = {}
        self.unreadable = set()
        self.reads = []
        self.ansi_reads = []
        self.status_calls = 0
        self.fail_sends = False
        self.lock = threading.Lock()
        self.sent = threading.Event()

    def is_available(self):
        return self.available

    def server_error_hint(self):
        return "herdr server not running - start herdr"

    def list_panes(self):
        return [dict(p) for p in self.panes]

    def get_pane(self, pane_id):
        for pane in self.panes:
            if pane["pane_id"] == pane_id:
                return dict(pane)
        return None

    def list_tabs(self):
        return [dict(t) for t in self.tabs]

    def list_workspaces(self):
        return [dict(w) for w in self.workspaces]

    # -- layout control (mutates the canned lists like herdr would) ---------

    def _fail(self):
        if self.fail_layout:
            message, self.fail_layout = self.fail_layout, None
            return False, message
        return None

    def _new_pane(self, workspace_id, tab_id, cwd):
        self._counter += 1
        pane = {"pane_id": f"{workspace_id}:p{self._counter}", "tab_id": tab_id,
                "workspace_id": workspace_id, "cwd": cwd or "/home/x",
                "agent_status": "unknown", "terminal_id": f"term_{self._counter}",
                "terminal_title_stripped": "shell", "focused": False}
        self.panes.append(pane)
        return dict(pane)

    def create_workspace(self, label=None, cwd=None, focus=False, env=None):
        failed = self._fail()
        if failed:
            return failed
        self._counter += 1
        ws_id = f"w{self._counter}"
        number = len(self.workspaces) + 1
        workspace = {"workspace_id": ws_id, "label": label or str(number), "number": number,
                     "focused": focus, "agent_status": "unknown"}
        tab = {"tab_id": f"{ws_id}:t1", "workspace_id": ws_id, "label": "1", "number": 1,
               "focused": focus, "agent_status": "unknown", "pane_count": 1}
        self.workspaces.append(workspace)
        self.tabs.append(tab)
        pane = self._new_pane(ws_id, tab["tab_id"], cwd)
        self.layout_calls.append(("create_workspace", label, cwd, focus))
        return True, {"type": "workspace_created", "workspace": dict(workspace),
                      "tab": dict(tab), "root_pane": pane}

    def create_tab(self, workspace_id=None, label=None, cwd=None, focus=False, env=None):
        failed = self._fail()
        if failed:
            return failed
        workspace_id = workspace_id or self.workspaces[0]["workspace_id"]
        if not any(w["workspace_id"] == workspace_id for w in self.workspaces):
            return False, f"workspace {workspace_id} not found"
        siblings = [t for t in self.tabs if t["workspace_id"] == workspace_id]
        number = max([t["number"] for t in siblings] + [0]) + 1
        tab = {"tab_id": f"{workspace_id}:t{number}", "workspace_id": workspace_id,
               "label": label or str(number), "number": number, "focused": focus,
               "agent_status": "unknown", "pane_count": 1}
        self.tabs.append(tab)
        pane = self._new_pane(workspace_id, tab["tab_id"], cwd)
        self.layout_calls.append(("create_tab", workspace_id, label, cwd, focus))
        return True, {"type": "tab_created", "tab": dict(tab), "root_pane": pane}

    def split_pane(self, pane_id, direction="right", cwd=None, ratio=None, focus=False,
                   env=None):
        failed = self._fail()
        if failed:
            return failed
        origin = self.get_pane(pane_id)
        if origin is None:
            return False, f"pane {pane_id} not found"
        pane = self._new_pane(origin["workspace_id"], origin["tab_id"], cwd)
        self.layout_calls.append(("split_pane", pane_id, direction, cwd, focus))
        return True, {"type": "pane_info", "pane": pane}

    def start_agent(self, name, argv, workspace_id=None, tab_id=None, split=None,
                    cwd=None, focus=False, env=None):
        failed = self._fail()
        if failed:
            return failed
        if tab_id:
            tab = next((t for t in self.tabs if t["tab_id"] == tab_id), None)
            if tab is None:
                return False, f"tab {tab_id} not found"
            workspace_id = tab["workspace_id"]
        else:
            workspace_id = workspace_id or self.workspaces[0]["workspace_id"]
            ok, created = self.create_tab(workspace_id=workspace_id, label=name, cwd=cwd)
            if not ok:
                return ok, created
            tab_id = created["tab"]["tab_id"]
            self.panes.remove(next(p for p in self.panes
                                   if p["pane_id"] == created["root_pane"]["pane_id"]))
        pane = self._new_pane(workspace_id, tab_id, cwd)
        for stored in self.panes:
            if stored["pane_id"] == pane["pane_id"]:
                stored["label"] = name
                stored["agent"] = name
                stored["agent_status"] = "idle"
        self.layout_calls.append(("start_agent", name, list(argv), workspace_id, tab_id, split))
        agent = dict(self.get_pane(pane["pane_id"]), name=name)
        return True, {"type": "agent_started", "agent": agent, "argv": list(argv)}

    def rename(self, kind, target_id, label):
        failed = self._fail()
        if failed:
            return failed
        items, key = {"workspace": (self.workspaces, "workspace_id"),
                      "tab": (self.tabs, "tab_id"), "pane": (self.panes, "pane_id")}[kind]
        for item in items:
            if item[key] == target_id:
                item["label"] = label
                self.layout_calls.append(("rename", kind, target_id, label))
                return True, {"type": f"{kind}_info", kind: dict(item)}
        return False, f"{kind} {target_id} not found"

    def close(self, kind, target_id):
        failed = self._fail()
        if failed:
            return failed
        key = {"workspace": "workspace_id", "tab": "tab_id", "pane": "pane_id"}[kind]
        if not any(i.get(key) == target_id for i in self.workspaces + self.tabs + self.panes):
            return False, f"{kind} {target_id} not found"
        self.panes = [p for p in self.panes if p.get(key) != target_id]
        self.tabs = [t for t in self.tabs if t.get(key) != target_id]
        self.workspaces = [w for w in self.workspaces if w.get(key) != target_id]
        self.layout_calls.append(("close", kind, target_id))
        return True, {"type": "ok"}

    def focus(self, kind, target_id):
        self.layout_calls.append(("focus", kind, target_id))
        return True, {"type": "ok"}

    def run_command(self, pane_id, command):
        if self.get_pane(pane_id) is None:
            return False, f"pane {pane_id} not found"
        self.runs.append((pane_id, command))
        return True, "ran"

    def _run(self, args, timeout=None):
        """The raw herdr CLI, for the passthrough command. Canned responses by
        argv tuple; otherwise a JSON result echoing the argv, and usage text
        for a bare command group."""
        self.raw_runs.append(list(args))
        key = tuple(args)
        if key in self.raw_responses:
            return self.raw_responses[key]
        if args == ["--help"]:
            return 0, "herdr — terminal workspace manager\n\nUsage: herdr [options]\n", ""
        if len(args) == 1 and args[0] in ("pane", "tab", "workspace", "agent", "wait"):
            return 2, "", f"herdr {args[0]} commands:\n  herdr {args[0]} list\n"
        if args[:2] == ["workspace", "list"]:
            return 0, json.dumps({"id": "cli", "result": {"workspaces": self.list_workspaces()}}), ""
        if args[:2] == ["tab", "get"]:
            tab = next((t for t in self.tabs if t["tab_id"] == args[2]), None)
            if tab is None:
                return 1, "", json.dumps({"error": {"code": "tab_not_found",
                                                    "message": f"tab {args[2]} not found"}})
            return 0, json.dumps({"id": "cli", "result": {"tab": tab}}), ""
        return 0, json.dumps({"id": "cli", "result": {"type": "ok", "argv": list(args)}}), ""

    def list_panes_grouped(self):
        tabs = {t["tab_id"]: t for t in self.tabs}
        workspaces = {w["workspace_id"]: w for w in self.workspaces}
        panes = self.list_panes()
        for pane in panes:
            tab = tabs.get(pane["tab_id"], {})
            ws = workspaces.get(pane["workspace_id"], {})
            pane["tab_label"] = tab.get("label") or pane["tab_id"]
            pane["tab_number"] = tab.get("number", 0)
            pane["workspace_label"] = ws.get("label") or pane["workspace_id"]
            pane["workspace_number"] = ws.get("number", 0)
        panes.sort(key=lambda p: (p["workspace_number"], p["tab_number"]))
        return panes

    def agent_status(self, pane_id):
        self.status_calls += 1
        if pane_id in self.statuses:
            return self.statuses[pane_id]
        for pane in self.panes:
            if pane["pane_id"] == pane_id:
                return pane.get("agent_status", "unknown")
        return None

    def set_screen(self, pane_id, screen):
        """Emulate exactly what `herdr pane read` would return for this pane
        (its rendered input box included). None makes the pane unreadable."""
        with self.lock:
            if screen is None:
                self.unreadable.add(pane_id)
            else:
                self.unreadable.discard(pane_id)
                self.screens[pane_id] = screen

    def read_pane(self, pane_id, lines=40, source=None, ansi=False):
        self.reads.append((pane_id, source))
        if ansi:
            self.ansi_reads.append(pane_id)
        if not any(p["pane_id"] == pane_id for p in self.panes):
            return None
        if pane_id in self.unreadable:
            return None
        if pane_id in self.screens:
            screen = self.screens[pane_id]
            if not ansi:
                screen = ANSI_RE.sub("", screen)
            tail = screen.splitlines()[-int(lines):]
            return "\n".join(tail) + "\n"
        # Default: the last sends, verbatim (a plain terminal echo).
        mine = [text for pid, text in self.sends if pid == pane_id]
        return "\n".join(mine[-int(lines):]) + "\n"

    def send_text_and_enter(self, pane_id, text):
        with self.lock:
            if self.fail_sends:
                return False, "boom"
            self.sends.append((pane_id, text))
            self.sent.set()
        return True, "sent"

    def notify(self, title, body=None):
        pass


# --- realistic input-box renderings, captured from live Claude Code / Codex panes ---

def _claude_frame(box_lines):
    """A Claude Code screen: some transcript, then the bordered input box."""
    rule = "\u2500" * 54
    return "\n".join([
        "  earlier transcript line",
        "\u273b Saut\u00e9ed for 1m 18s \u00b7 done 9:17 AM",
        "",
        rule,
        *box_lines,
        rule,
        "  \u23f5\u23f5 bypass permissions on (shift+tab to cycle) \u00b7 \u2190 for agents",
    ]) + "\n"


def claude_box_empty():
    return _claude_frame(["\u276f "])


def claude_box_pending(text):
    """A short send still sitting verbatim in the box."""
    return _claude_frame(["\u276f " + text])


def claude_box_collapsed(lines=75):
    """A long send Claude collapsed into a paste placeholder in the box."""
    return _claude_frame([
        "\u276f [Pasted text #1 +%d lines]" % lines,
        "  +%d lines (ctrl+o to expand)" % lines,
    ])


def claude_box_pasted_unsubmitted():
    """The four-hour ghost: a paste that never submitted, box still holding it."""
    return _claude_frame(["\u276f Pasted text #1 (paste again to expand)"])


def codex_box_empty_with_transcript_collapse():
    """Codex idle with an empty box, but a collapse chip up in the transcript
    (which must NOT be mistaken for queued input)."""
    return "\n".join([
        "  \u2514 Map probe passed.",
        "    \u2026 +28 lines (ctrl + t to view transcript)",
        "",
        "\u2022 Working (15m 08s \u00b7 esc to interrupt)",
        "",
        "\u203a Ask Codex to do anything",
        "",
        "  gpt-6-astra xhigh \u00b7 ~/workspace/the_grind_2 \u00b7 Main [default]",
    ]) + "\n"


def codex_box_rotating_hint():
    """Codex idle box showing one of its rotating hints (not typed input)."""
    return "\n".join([
        "  \u2514 Committed and pushed to origin/main.",
        "",
        "\u203a Use /skills to list available skills",
        "",
        "  gpt-5.6-sol xhigh \u00b7 ~/workspace/site",
    ]) + "\n"


def codex_box_pending(text):
    return "\n".join([
        "  \u2514 earlier output",
        "",
        "\u203a " + text,
        "",
        "  gpt-6-astra xhigh \u00b7 ~/workspace \u00b7 Main [default]",
    ]) + "\n"


# --- the same boxes with their ANSI styling, as `herdr pane read --format ansi`
# returns them (captured live 2026-09-19). Typed text is full intensity; the
# empty-box placeholder hints are dim (SGR 2). ---

E = "\x1b["
CLAUDE_BG = E + "48;2;55;55;55m"


def _claude_frame_ansi(box_lines):
    rule = E + "0m" + E + "38;2;136;136;136m" + "\u2500" * 54 + E + "0m"
    return "\r\n".join([
        E + "0m  earlier transcript line",
        E + "0m" + E + "38;2;153;153;153m\u273b Saut\u00e9ed for 1m 18s \u00b7 done 9:17 AM" + E + "0m",
        "",
        rule,
        *box_lines,
        rule,
        "  " + E + "0m" + E + "38;2;255;107;128m\u23f5\u23f5 bypass permissions on" + E + "0m"
        + E + "38;2;153;153;153m (shift+tab to cycle) \u00b7 \u2190 for agents" + E + "0m",
    ]) + "\r\n"


def claude_box_typing_ansi(text):
    """A human mid-sentence: white text after the sigil, on the box background."""
    return _claude_frame_ansi([
        E + "0m" + E + "38;2;80;80;80m" + CLAUDE_BG + "\u276f " + E + "0m"
        + E + "38;2;255;255;255m" + CLAUDE_BG + text + E + "0m" + CLAUDE_BG
        + " " * 20 + E + "0m",
    ])


def claude_box_hint_ansi():
    """Empty Claude box showing a dim hint ("Press up to edit queued messages")."""
    return _claude_frame_ansi([
        E + "0m" + E + "38;2;153;153;153m\u276f\u00a0 " + E + "0m" + E + "2m"
        + "Press up to edit queued messages" + E + "0m",
    ])


def claude_box_empty_ansi():
    return _claude_frame_ansi([
        E + "0m" + E + "38;2;80;80;80m" + CLAUDE_BG + "\u276f " + E + "0m" + CLAUDE_BG
        + " " * 40 + E + "0m",
    ])


def claude_box_collapsed_ansi(lines=75):
    """Our long send collapsed into a paste placeholder (rendered full intensity)."""
    return _claude_frame_ansi([
        E + "0m" + E + "38;2;80;80;80m" + CLAUDE_BG + "\u276f " + E + "0m"
        + E + "38;2;255;255;255m" + CLAUDE_BG + "[Pasted text #1 +%d lines]" % lines + E + "0m",
        E + "0m" + CLAUDE_BG + "  " + E + "38;2;153;153;153m+%d lines (ctrl+o to expand)" % lines
        + E + "0m",
    ])


CODEX_BG = E + "48;2;30;30;30m"


def _codex_frame_ansi(sigil_line):
    return "\r\n".join([
        E + "0m" + E + "2m  done 6:03 PM" + E + "0m",
        " ",
        # Codex's animated braille border above the box, coloured, not dim.
        E + "0m" + E + "38;2;40;40;40m" + CODEX_BG + "\u2801" + E + "0m" + CODEX_BG + "   "
        + E + "0m" + E + "38;2;70;70;70m" + CODEX_BG + "\u2808" + E + "0m",
        sigil_line,
        E + "0m" + CODEX_BG + "       " + E + "0m" + E + "38;2;56;56;56m" + CODEX_BG + "\u2880"
        + E + "0m" + CODEX_BG + "        " + E + "0m",
        "  " + E + "0m" + E + "38;2;246;226;183mgpt-6-astra xhigh" + E + "0m" + E + "2m \u00b7 "
        + E + "0m" + E + "38;2;171;223;167m~/workspace/poolrad-macmaps" + E + "0m",
    ]) + "\r\n"


def codex_box_hint_ansi(hint="Ask Codex to do anything"):
    """Empty Codex box: bold-dim sigil, a coloured braille dot, then the dim hint."""
    return _codex_frame_ansi(
        E + "0m" + E + "1m" + CODEX_BG + "\u203a" + E + "0m" + E + "38;2;49;49;49m" + CODEX_BG
        + "\u2801" + E + "0m" + E + "2m" + CODEX_BG + hint + E + "0m" + CODEX_BG + "   " + E + "0m"
    )


def codex_box_typing_ansi(text):
    return _codex_frame_ansi(
        E + "0m" + E + "1m" + E + "2m" + CODEX_BG + "\u203a " + E + "0m" + CODEX_BG + text
        + E + "0m"
    )
