"""End-to-end: the `dunkingsheep` CLI auto-starts a real daemon, which drives a
fake `herdr` binary placed first on PATH. Nothing here touches the user's real
herdr or ~/.config/dunkingsheep."""

import json
import os
import stat
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CLI = os.path.join(ROOT, "dunkingsheep")

FAKE_HERDR = r'''#!/usr/bin/env python3
import json, os, sys
log = os.environ["FAKE_HERDR_LOG"]
args = sys.argv[1:]
panes = [
  {"pane_id": "w1:p1", "tab_id": "w1:t1", "workspace_id": "w1", "cwd": "/tmp/a", "agent_status": "unknown", "terminal_id": "t1"},
  {"pane_id": "w1:p2", "tab_id": "w1:t2", "workspace_id": "w1", "cwd": "/tmp/b", "agent": "claude", "agent_status": "idle", "terminal_id": "t2"},
]
def out(obj):
    print(json.dumps({"id": "cli", "result": obj}))
if args[:2] == ["status", "server"]:
    print("server:\n  status: running")
elif args[:2] == ["pane", "list"]:
    out({"panes": panes})
elif args[:2] == ["tab", "list"]:
    out({"tabs": [{"tab_id": "w1:t1", "label": "Shell", "number": 1}, {"tab_id": "w1:t2", "label": "Claude Tab", "number": 2}]})
elif args[:2] == ["workspace", "list"]:
    out({"workspaces": [{"workspace_id": "w1", "label": "Work", "number": 1}]})
elif args[:2] == ["pane", "get"]:
    out({"pane": next(p for p in panes if p["pane_id"] == args[2])})
elif args[:2] == ["pane", "send-text"]:
    with open(log, "a") as f: f.write(f"TEXT {args[2]} {args[3]}\n")
elif args[:2] == ["pane", "send-keys"]:
    with open(log, "a") as f: f.write(f"KEYS {args[2]} {' '.join(args[3:])}\n")
elif args[:2] == ["pane", "read"]:
    sys.stdout.write(open(log).read() if os.path.exists(log) else "")
elif args[:2] == ["workspace", "create"]:
    label = args[args.index("--label") + 1] if "--label" in args else "2"
    with open(log, "a") as f: f.write(f"WORKSPACE-CREATE {label}\n")
    out({"type": "workspace_created",
         "workspace": {"workspace_id": "w2", "label": label, "number": 2},
         "tab": {"tab_id": "w2:t1", "workspace_id": "w2", "label": "1", "number": 1},
         "root_pane": {"pane_id": "w2:p1", "tab_id": "w2:t1", "workspace_id": "w2", "cwd": "/tmp"}})
elif args[:2] == ["tab", "create"]:
    label = args[args.index("--label") + 1] if "--label" in args else "3"
    ws = args[args.index("--workspace") + 1] if "--workspace" in args else "w1"
    with open(log, "a") as f: f.write(f"TAB-CREATE {ws} {label}\n")
    out({"type": "tab_created",
         "tab": {"tab_id": ws + ":t9", "workspace_id": ws, "label": label, "number": 9},
         "root_pane": {"pane_id": ws + ":p9", "tab_id": ws + ":t9", "workspace_id": ws, "cwd": "/tmp"}})
elif args[:2] == ["pane", "run"]:
    with open(log, "a") as f: f.write(f"RUN {args[2]} {args[3]}\n")
elif args[:2] == ["tab", "rename"]:
    with open(log, "a") as f: f.write(f"TAB-RENAME {args[2]} {args[3]}\n")
    out({"type": "tab_info", "tab": {"tab_id": args[2], "label": args[3]}})
elif args[:2] == ["tab", "close"]:
    with open(log, "a") as f: f.write(f"TAB-CLOSE {args[2]}\n")
    out({"type": "ok"})
elif args[:2] == ["pane", "zoom"]:
    out({"type": "ok", "zoomed": args[2]})
elif args == ["--help"]:
    print("herdr — terminal workspace manager\n\nUsage: herdr [options]")
elif args == ["tab"]:
    sys.exit("herdr tab commands:\n  herdr tab list")
else:
    sys.exit(f"fake herdr: unsupported {args}")
'''


class CliEndToEndTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        bin_dir = os.path.join(cls.tmp.name, "bin")
        os.makedirs(bin_dir)
        fake = os.path.join(bin_dir, "herdr")
        with open(fake, "w") as handle:
            handle.write(FAKE_HERDR)
        os.chmod(fake, os.stat(fake).st_mode | stat.S_IEXEC)
        cls.log = os.path.join(cls.tmp.name, "herdr.log")
        cls.config = os.path.join(cls.tmp.name, "config")
        cls.env = dict(
            os.environ,
            PATH=bin_dir + os.pathsep + os.environ.get("PATH", ""),
            FAKE_HERDR_LOG=cls.log,
            DUNKINGSHEEP_DIR=cls.config,
            DUNKINGSHEEP_SOCKET=os.path.join(cls.tmp.name, "ds.sock"),
            HERDR_PANE_ID="w1:p2",
        )

    @classmethod
    def tearDownClass(cls):
        subprocess.run([sys.executable, CLI, "shutdown", "--stop-all"], env=cls.env,
                       capture_output=True, text=True, timeout=20)
        time.sleep(0.3)
        cls.tmp.cleanup()

    def run_cli(self, *args, expect=0):
        proc = subprocess.run([sys.executable, CLI, *args], env=self.env,
                              capture_output=True, text=True, timeout=30)
        self.assertEqual(expect, proc.returncode, msg=proc.stdout + proc.stderr)
        return proc

    def run_json(self, *args):
        return json.loads(self.run_cli("--json", *args).stdout)

    def test_full_lifecycle_through_the_cli(self):
        # 1. status auto-starts the daemon.
        status = self.run_json("status")
        self.assertEqual(0, status["dunkers"])
        self.assertTrue(status["herdr_available"])
        self.assertTrue(os.path.exists(self.env["DUNKINGSHEEP_SOCKET"]))
        self.assertTrue(os.path.exists(os.path.join(self.config, "daemon.log")))

        # 2. panes come from the fake herdr, with self marked.
        panes = self.run_json("panes")
        self.assertEqual(["w1:p1", "w1:p2"], [p["pane_id"] for p in panes["panes"]])
        self.assertTrue(panes["panes"][1]["is_self"])
        human = self.run_cli("panes").stdout
        self.assertIn("Claude Tab", human)
        self.assertIn("* = this pane (w1:p2)", human)

        # 3. add a dunk to self (running), another stopped by label.
        dunk = self.run_json("add", "--target", "self", "--text", "ping {id} #{count}",
                             "--every", "90s", "--name", "meta", "--only-idle")
        self.assertEqual("d1", dunk["id"])
        self.assertEqual("w1:p2", dunk["target_pane_id"])
        self.assertEqual(1.5, dunk["interval_minutes"])
        self.assertEqual("idle", dunk["only_when"])
        self.assertTrue(dunk["running"])
        self.assertTrue(dunk["hold_while_typing"])  # on by default
        other = self.run_json("add", "--target", "shell", "--every", "5", "--no-start",
                              "--max-sends", "2", "--skip-if-unconsumed", "--ignore-typing")
        self.assertEqual("w1:p1", other["target_pane_id"])
        self.assertFalse(other["running"])
        self.assertEqual(2, other["max_sends"])
        self.assertTrue(other["skip_if_unconsumed"])
        self.assertFalse(other["hold_while_typing"])
        detail = self.run_cli("get", "d2").stdout
        self.assertIn("skip_if_unconsumed True", detail)
        self.assertIn("hold_while_typing False", detail)

        listing = self.run_cli("list").stdout
        self.assertIn("d1", listing)
        self.assertIn("Claude Tab / claude", listing)
        self.assertIn("0/2", listing)
        self.assertIn("SKIP", listing)
        self.assertIn(" skip ", listing)   # d2's gate column
        self.assertIn(" idle ", listing)   # d1's gate column

        # 4. fire sends through the fake herdr: text, then Enter.
        fired = self.run_json("fire", "d1")
        self.assertTrue(fired["ok"])
        with open(self.log) as handle:
            log = handle.read()
        self.assertIn("TEXT w1:p2 ping d1 #1\n", log)
        self.assertIn("KEYS w1:p2 Enter\n", log)
        self.assertEqual(1, self.run_json("get", "d1")["send_count"])

        # 5. one-off send and read back.
        self.assertIn("sent to", self.run_cli("send", "w1:p1", "hello", "there").stdout)
        self.assertIn("m1    sent", self.run_cli("messages").stdout)
        read = self.run_cli("read", "w1:p1", "--lines", "10").stdout
        self.assertIn("TEXT w1:p1 hello there", read)

        # 6. update, toggle, stop-all.
        updated = self.run_json("update", "d1", "--every", "45", "--any-time", "--text", "go")
        self.assertEqual(45.0, updated["interval_minutes"])
        self.assertIsNone(updated["only_when"])
        self.assertEqual("go", updated["text"])
        self.assertFalse(self.run_json("update", "d2", "--always-send")["skip_if_unconsumed"])
        self.assertTrue(self.run_json("update", "d1", "--skip-if-unconsumed")["skip_if_unconsumed"])
        self.assertTrue(self.run_json("update", "d2", "--hold-while-typing")["hold_while_typing"])
        self.assertFalse(self.run_json("update", "d2", "--ignore-typing")["hold_while_typing"])
        self.assertTrue(self.run_json("toggle", "d2")["running"])
        stopped = self.run_json("stop-all")["dunks"]
        self.assertFalse(any(d["running"] for d in stopped))

        # 6b. herdr layout control through the CLI.
        created = self.run_json("new-workspace", "-l", "Site", "--cwd", "/tmp", "-r", "claude")
        self.assertEqual("w2", created["workspace"]["workspace_id"])
        self.assertEqual("claude", created["ran"])
        human = self.run_cli("new-tab", "-w", "w1", "-l", "writer").stdout
        self.assertIn("created tab w1:t9 'writer', pane w1:p9", human)
        self.assertIn("renamed tab w1:t2 -> 'Main'", self.run_cli("rename", "tab", "Claude Tab", "Main").stdout)
        self.assertIn("closed tab w1:t1", self.run_cli("close", "tab", "Shell").stdout)
        zoom = self.run_json("herdr", "pane", "zoom", "self", "--on")
        self.assertTrue(zoom["ok"])
        self.assertEqual(["pane", "zoom", "w1:p2", "--on"], zoom["argv"])
        self.assertEqual("w1:p2", zoom["result"]["zoomed"])
        self.assertIn("Usage: herdr", self.run_cli("herdr-help").stdout)
        self.assertIn("herdr tab commands", self.run_cli("herdr-help", "tab").stdout)
        listing = self.run_cli("workspaces").stdout  # the fake herdr is stateless: canned w1
        self.assertIn("*w1", listing)
        self.assertIn("Claude Tab", listing)
        with open(self.log) as handle:
            log = handle.read()
        self.assertIn("WORKSPACE-CREATE Site\n", log)
        self.assertIn("RUN w2:p1 claude\n", log)
        self.assertIn("TAB-CREATE w1 writer\n", log)
        self.assertIn("TAB-RENAME w1:t2 Main\n", log)
        self.assertIn("TAB-CLOSE w1:t1\n", log)
        with self.assertRaises(AssertionError):  # own workspace is refused, exit code 1
            self.run_cli("close", "workspace", "self")

        # 7. state persisted on disk.
        with open(os.path.join(self.config, "dunks.json")) as handle:
            saved = json.load(handle)
        self.assertEqual(["d1", "d2"], [d["id"] for d in saved["dunkers"]])

        # 8. errors have a non-zero exit and message on stderr.
        proc = self.run_cli("get", "d77", expect=2)
        self.assertIn("no dunk with id", proc.stderr)
        proc = self.run_cli("add", "--target", "nowhere", expect=2)
        self.assertIn("no herdr pane matches", proc.stderr)

        # 9. remove both; shutdown; restart resumes cleanly.
        self.run_cli("remove", "d1")
        self.run_cli("rm", "d2")
        self.assertEqual([], self.run_json("list")["dunks"])
        self.run_cli("shutdown")
        time.sleep(0.5)
        self.assertFalse(os.path.exists(self.env["DUNKINGSHEEP_SOCKET"]))
        proc = self.run_cli("--no-start-daemon", "status", expect=3)
        self.assertIn("no dunkingsheep daemon", proc.stderr)
        self.assertEqual(0, self.run_json("status")["dunkers"])

    def test_commands_listing_and_version(self):
        out = self.run_cli("commands").stdout
        self.assertIn("add_dunk(", out)
        self.assertIn("[socket/cli only]", out)
        self.assertIn("dunkingsheep 2.3", self.run_cli("--version").stdout)
        registry = json.loads(self.run_cli("commands", "--json").stdout)
        names = [c["name"] for c in registry]
        self.assertIn("help", names)
        self.assertIn("input_schema", registry[0])

    def test_self_discovery_surfaces(self):
        # No arguments prints help rather than an error.
        bare = self.run_cli()
        self.assertIn("quick start", bare.stdout)
        self.assertIn("dunkingsheep guide", bare.stdout)
        help_out = self.run_cli("--help").stdout
        self.assertIn("mcp-setup", help_out)
        add_help = self.run_cli("add", "-h").stdout
        self.assertIn("examples:", add_help)
        self.assertIn("{id}", add_help)

        guide_text = self.run_cli("guide").stdout
        for phrase in ("Concepts", "unix socket API", "MCP server", "meta-dunking",
                       "self_pane_id", "only_when", "max_sends", "{id}"):
            self.assertIn(phrase, guide_text)
        self.assertEqual(guide_text, self.run_cli("help").stdout)
        markdown = self.run_cli("docs", "--markdown").stdout
        self.assertTrue(markdown.startswith("# Dunking Sheep"))
        with open(os.path.join(ROOT, "docs", "GUIDE.md")) as handle:
            self.assertEqual(markdown, handle.read(),
                             "docs/GUIDE.md is stale: run ./dunkingsheep guide --markdown > docs/GUIDE.md")

        setup = self.run_cli("mcp-setup").stdout
        self.assertIn("claude mcp add --scope user dunkingsheep -- python3", setup)
        self.assertIn("codex mcp add dunkingsheep -- python3", setup)
        self.assertIn("[mcp_servers.dunkingsheep]", setup)
        self.assertIn('"mcpServers"', setup)
        self.assertIn(CLI, setup)
        toml = self.run_cli("mcp-setup", "toml").stdout
        self.assertTrue(toml.startswith("[mcp_servers.dunkingsheep]"))
        cursor = self.run_cli("mcp-setup", "cursor", "--apply", expect=2)
        self.assertIn("no CLI to apply", cursor.stderr)

        # The socket API describes itself too (raw protocol, no client library).
        import socket
        self.run_json("status")  # make sure the daemon is up
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.connect(self.env["DUNKINGSHEEP_SOCKET"])
        sock.sendall(b'{"id": "x", "cmd": "help", "args": {"markdown": true}}\n')
        reply = json.loads(sock.makefile("rb").readline())
        sock.close()
        self.assertTrue(reply["ok"])
        self.assertEqual("x", reply["id"])
        self.assertTrue(reply["result"]["guide"].startswith("# Dunking Sheep"))
        self.assertEqual("add_dunk", next(
            c["name"] for c in reply["result"]["commands"] if c["name"] == "add_dunk"))
        self.assertIn("request", reply["result"]["protocol"])


if __name__ == "__main__":
    unittest.main()
