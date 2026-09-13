"""Test doubles shared by the Dunking Sheep test suite."""

import threading


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
        self.available = available
        self.sends = []
        self.statuses = {}
        self.consumed = {}
        self.unreadable = set()
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

    def list_tabs(self):
        return [dict(t) for t in TABS]

    def list_workspaces(self):
        return [dict(w) for w in WORKSPACES]

    def list_panes_grouped(self):
        tabs = {t["tab_id"]: t for t in TABS}
        workspaces = {w["workspace_id"]: w for w in WORKSPACES}
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

    def read_pane(self, pane_id, lines=40, source="recent"):
        """The pane tail: every send not yet consumed is still visible, like an
        unread terminal input buffer."""
        if not any(p["pane_id"] == pane_id for p in self.panes):
            return None
        if pane_id in self.unreadable:
            return None
        mine = [text for pid, text in self.sends if pid == pane_id]
        mine = mine[self.consumed.get(pane_id, 0):]
        return "\n".join(mine[-lines:]) + "\n"

    def consume(self, pane_id, busy=True):
        """Simulate the target picking up everything sent so far: the tail no
        longer shows it and (optionally) the agent goes busy for a turn."""
        with self.lock:
            self.consumed[pane_id] = len([1 for pid, _ in self.sends if pid == pane_id])
        if busy:
            self.statuses[pane_id] = "working"

    def send_text_and_enter(self, pane_id, text):
        with self.lock:
            if self.fail_sends:
                return False, "boom"
            self.sends.append((pane_id, text))
            self.sent.set()
        return True, "sent"

    def notify(self, title, body=None):
        pass
