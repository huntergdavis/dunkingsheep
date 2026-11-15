"""t4644 (manual, NOT in CI: not named *_browser.py on purpose, it needs built binaries): the quick message, end to end on a PRIVATE fixture: a real daemon, a real desk and a
real browser, against fixture state only (private tmux server, config dir, socket and a
fixture-only web token and password). Nothing live is touched.

Build first (debug is fine) and set DS_BIN (the dunkingsheep binary), DS_DESK_TEST_BINARY and
CHROMIUM_PATH. Reports what happened to one quick message: the HTTP answer, the dialog's text,
and where the daemon stored it.
"""
import hashlib, json, os, pathlib, socket, subprocess, sys, tempfile, time, urllib.request
from playwright.sync_api import sync_playwright

UI = pathlib.Path(__file__).resolve().parents[1] / "dist"
DS = pathlib.Path(os.environ["DS_BIN"]).resolve()
DESK = pathlib.Path(os.environ["DS_DESK_TEST_BINARY"]).resolve()
# A short root: the daemon's socket path must stay under 107 bytes.
RUN = pathlib.Path(tempfile.mkdtemp(prefix="qm-", dir="/tmp"))
for sub in ("home", "config", "ui", "data"):
    (RUN / sub).mkdir()
(RUN / "ui/dist").symlink_to(UI)
TMUX = "qm" + str(os.getpid())
port_socket = socket.socket(); port_socket.bind(("127.0.0.1", 0)); PORT = port_socket.getsockname()[1]; port_socket.close()
TOKEN = "fixture-only-web-token"
(RUN / "config/service-tokens.json").write_text(json.dumps({"tokens": {
    hashlib.sha256(TOKEN.encode()).hexdigest(): {"role": "web", "label": "desk", "issued_at": 1, "by": "fixture"}}}))
os.chmod(RUN / "config/service-tokens.json", 0o600)
env = {
    "HOME": str(RUN / "home"), "PATH": "/usr/bin:/bin",
    "DUNKINGSHEEP_DIR": str(RUN / "config"), "DUNKINGSHEEP_SOCKET": str(RUN / "d.sock"),
    "DUNKINGSHEEP_TMUX_SOCKET": TMUX, "DUNKINGSHEEP_TMUX_REGISTRY": str(RUN / "config/tmux-panes.json"),
    "VP_CHANNELS_FILE": str(RUN / "channels.json"), "VP_DATA_DIR": str(RUN / "data"),
    "VP_BIND": "127.0.0.1", "VP_PORT": str(PORT), "VP_PASSWORD": "fixture-only", "DS_DESK_SCHEME": "http",
    "TMPDIR": str(RUN),
}
(RUN / "channels.json").write_text(json.dumps([{"id": "fixture", "name": "Fixture", "match": "fixture", "kind": "monitor"}]))

def tmux(*args):
    return subprocess.check_output(["/usr/bin/tmux", "-L", TMUX, *args], env=env, text=True).strip()

procs = []
result = {}
try:
    # A pane that looks like a codex lane to tmux: its foreground program is named codex.
    (RUN / "bin").mkdir()
    # A copy, not a link: the host's multi-call coreutils refuse to run under another name, and
    # the kernel names a process after its executable file.
    import shutil
    shutil.copy2(os.path.realpath("/usr/bin/python3"), RUN / "bin/codex")
    pane = tmux("new-session", "-d", "-P", "-F", "#{pane_id}", "-s", "fixture",
                f"exec {RUN}/bin/codex -c 'import time; time.sleep(600)'")
    (RUN / "config/tmux-panes.json").write_text(json.dumps({"panes": {pane: {"name": "fixture-worker", "agent": "fixture-worker", "agent_kind": "codex"}}}))
    daemon = subprocess.Popen([str(DS), "serve"], cwd=RUN, env=env, stdout=open(RUN / "daemon.log", "w"), stderr=subprocess.STDOUT)
    procs.append(daemon)
    for _ in range(300):
        if (RUN / "d.sock").exists() or daemon.poll() is not None: break
        time.sleep(0.1)
    result["daemon_started"] = {"socket": (RUN / "d.sock").exists(), "exit": daemon.poll()}
    desk_env = dict(env, DUNKINGSHEEP_SERVICE_TOKEN=TOKEN)
    desk = subprocess.Popen([str(DESK)], cwd=RUN, env=desk_env, stdout=open(RUN / "desk.log", "w"), stderr=subprocess.STDOUT)
    procs.append(desk)
    for _ in range(150):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{PORT}/", timeout=1); break
        except Exception: time.sleep(0.1)
    with sync_playwright() as p:
        # The desk accepts sign-in only on an owned host name (LAN policy); map it to 127.0.0.1
        # inside Chromium only, as the terminal transport fixture does.
        host = socket.gethostname().split(".")[0]
        browser = p.chromium.launch(executable_path=os.environ["CHROMIUM_PATH"], headless=True, args=[
            "--no-sandbox", "--disable-dev-shm-usage", "--no-proxy-server", f"--host-resolver-rules=MAP {host} 127.0.0.1"])
        page = browser.new_page()
        base = f"http://{host}:{PORT}"
        page.goto(base + "/dash/projects", wait_until="domcontentloaded")
        page.get_by_label("Owner password").wait_for()
        result["login_api"] = page.evaluate(
            "() => fetch('/api/login', {method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify({password: 'fixture-only', next: '/dash/projects'})}).then(async r => ({status: r.status, body: (await r.text()).slice(0, 300)}))")
        result["cookies"] = [c["name"] for c in page.context.cookies()]
        page.wait_for_timeout(500)
        result["after_login_url"] = page.url
        result["login_error"] = page.locator("[role=alert]").all_inner_texts()
        fleet = page.evaluate("fetch('/api/fleet').then(r => r.json()).then(j => ({state: j.state, rows: (j.rows||[]).map(r => ({pane_id: r.pane_id, name: r.name})), error: j.error}))")
        for cmd in (["panes", "--json"], ["roster", "--json"], ["lanes", "--json"]):
            out = subprocess.run([str(DS), *cmd], env=env, capture_output=True, text=True).stdout
            result["cli_" + cmd[0]] = out[:700]
        result["fleet"] = fleet
        # The dialog, as the human uses it.
        page.goto(base + "/dash/projects", wait_until="domcontentloaded")
        page.get_by_role("button", name="Quick message").click()
        dialog = page.get_by_role("dialog", name="Quick message")
        dialog.wait_for()
        page.wait_for_timeout(1500)
        result["dialog_status"] = dialog.locator("[role=status], [role=alert]").all_inner_texts()
        result["dialog_options"] = dialog.locator("#quick-message-pane option").all_inner_texts()
        if dialog.locator("#quick-message-pane option").count() > 1:
            dialog.locator("#quick-message-pane").select_option(index=1)
            dialog.locator("#quick-message-text").fill("quick message from the dialog")
            dialog.get_by_role("button", name="Send").click()
            page.wait_for_timeout(2000)
            result["dialog_after_send"] = dialog.locator("[role=status], [role=alert]").all_inner_texts()
        answer = page.evaluate(
            "pane => fetch('/api/pane-inbox', {method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify({pane, text: 'quick message probe'})}).then(async r => ({status: r.status, body: await r.text()}))",
            pane)
        result["pane_inbox"] = answer
        # (b) Does the sent message appear in the owner's Inbox ('Your inbox' = human_threads)?
        result["owner_inbox"] = page.evaluate(
            "() => fetch('/api/owner-thread?limit=200&paged=true&filter=human_threads').then(r => r.json()).then(j => ({count: (j.items||[]).length, items: (j.items||[]).slice(0, 5).map(x => ({id: x.id, mailbox: x.mailbox, sender: x.sender, to: x.to, kind: x.conversation_kind, text: String(x.body || x.text || '').slice(0, 40)}))}))")
        browser.close()
    result["socket_exists"] = (RUN / "d.sock").exists()
    result["daemon_log_tail"] = (RUN / "daemon.log").read_text()[-600:]
    result["desk_log_tail"] = (RUN / "desk.log").read_text()[-600:]
finally:
    for proc in procs:
        proc.terminate()
    subprocess.run(["/usr/bin/tmux", "-L", TMUX, "kill-server"], env=env, capture_output=True)
print(json.dumps(result, indent=1))
print("fixture dir:", RUN)
# t4644 (a): the recipient is labelled by the registered lane name, not tmux's name.
assert any(option.startswith("fixture-worker") for option in result.get("dialog_options", [])), result.get("dialog_options")
assert result.get("dialog_after_send", [""])[0].startswith("Stored in fixture-worker inbox"), result.get("dialog_after_send")
print("quick message private-fixture check passed")
