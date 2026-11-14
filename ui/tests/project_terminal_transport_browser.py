"""Real transport regression using private auth/state/tmux and a non-owner pane.

Build ui/dist and ds-desk first. Set DS_DESK_TEST_BINARY and CHROMIUM_PATH;
run with a writable TMPDIR. Never attaches to the default tmux server.
"""
import json, os, pathlib, socket, subprocess, time, urllib.request
from playwright.sync_api import sync_playwright

ROOT = pathlib.Path(os.environ["TMPDIR"]).resolve()
UI = pathlib.Path(__file__).resolve().parents[1] / "dist"
BINARY = pathlib.Path(os.environ["DS_DESK_TEST_BINARY"]).resolve()
RUN = ROOT / ('fixture-' + str(os.getpid()))
RUN.mkdir(mode=0o700)
(RUN / 'home').mkdir()
(RUN / 'config').mkdir()
(RUN / 'bin').mkdir()
(RUN / 'ui').mkdir()
(RUN / 'ui/dist').symlink_to(UI)
SOCKET = 'ds-t4955-' + str(os.getpid())
PORT_SOCKET = socket.socket()
PORT_SOCKET.bind(('127.0.0.1', 0))
PORT = PORT_SOCKET.getsockname()[1]
PORT_SOCKET.close()
# Exercise the owned-hostname authority accepted by the LAN-only policy without
# exposing this fixture on a LAN interface. Chromium alone resolves it to 127.
HOST = socket.gethostname().split('.')[0]
BASE = f'http://{HOST}:{PORT}'
for name, text in {
    'tmux': f'#!/bin/sh\nexec /usr/bin/tmux -L {SOCKET} "$@"\n',
    'gh': '#!/bin/sh\nexit 1\n',
}.items():
    p = RUN / 'bin' / name
    p.write_text(text)
    p.chmod(0o700)
(RUN / 'echo.py').write_text(f"""import os, threading, time, tty
tty.setraw(0)
def output_burst():
    for _ in range(500):
        os.write(1, b'output-update\\r\\n')
        time.sleep(.002)
    os.write(1, b't4955-output-end\\r\\n')
while True:
    b = os.read(0, 4096)
    with open({str(RUN / 'received')!r}, 'ab') as f:
        f.write(b)
    os.write(1, b)
    if b'!' in b:
        threading.Thread(target=output_burst, daemon=True).start()
""")
env = {
    'HOME': str(RUN / 'home'), 'PATH': str(RUN / 'bin') + ':/usr/bin:/bin',
    'DUNKINGSHEEP_DIR': str(RUN / 'config'),
    'DUNKINGSHEEP_SOCKET': str(RUN / 'absent-daemon.sock'),
    'DUNKINGSHEEP_SERVICE_TOKEN': 'private-fixture-service-token',
    'DUNKINGSHEEP_TMUX_SOCKET': SOCKET,
    'DUNKINGSHEEP_TMUX_REGISTRY': str(RUN / 'config/tmux-panes.json'),
    'DUNKINGSHEEP_TERMINAL_STREAM': '1',
    'DUNKINGSHEEP_TERMINAL_STREAM_HOSTS': f'{HOST}:{PORT}',
    'VP_CHANNELS_FILE': str(RUN / 'channels.json'),
    'VP_DATA_DIR': str(RUN / 'data'), 'VP_BIND': '127.0.0.1', 'VP_PORT': str(PORT),
    'VP_PASSWORD': 'private-fixture-password', 'DS_DESK_SCHEME': 'http',
    'TMPDIR': str(RUN),
}
(RUN / 'channels.json').write_text(json.dumps([{'id':'fixture','name':'Fixture','match':'fixture','kind':'monitor'}]))
def tmux(*args):
    return subprocess.check_output(['/usr/bin/tmux','-L',SOCKET,*args], env=env, text=True).strip()
desk = None
try:
    pane = tmux('new-session','-d','-P','-F','#{pane_id}','-s','fixture',f'/usr/bin/python3 {RUN}/echo.py')
    pid = int(tmux('display-message','-p','-t',pane,'#{pane_pid}'))
    start = pathlib.Path(f'/proc/{pid}/stat').read_text().rsplit(')',1)[1].split()[19]
    (RUN / 'config/tmux-panes.json').write_text(json.dumps({'panes':{pane:{'name':'fixture-worker','agent':'fixture-worker'}}}))
    (RUN / 'config/roles.json').write_text(json.dumps({'default':'anonymous','lanes':{pane:{'role':'worker','pane_pid':pid,'start':start}}}))
    log = open(RUN / 'desk.log','w')
    desk = subprocess.Popen([str(BINARY)], cwd=RUN, env=env, stdout=log, stderr=log)
    for _ in range(100):
        if desk.poll() is not None: raise RuntimeError((RUN/'desk.log').read_text())
        try:
            probe = urllib.request.Request(f'http://127.0.0.1:{PORT}/', headers={'Host':f'{HOST}:{PORT}'})
            urllib.request.urlopen(probe, timeout=1)
            break
        except Exception: time.sleep(.1)
    with sync_playwright() as p:
        browser = p.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'], headless=True, args=[
            '--no-sandbox', '--disable-dev-shm-usage', '--no-proxy-server',
            f'--host-resolver-rules=MAP {HOST} 127.0.0.1',
        ])
        context = browser.new_context()
        page = context.new_page()
        # APIRequestContext uses OS DNS rather than Chromium's resolver rules.
        # Browser fetch keeps both the login and WebSocket on the owned origin.
        page.goto(BASE+'/', wait_until='domcontentloaded')
        status = page.evaluate('''async () => (await fetch('/api/login', {
            method: 'POST', headers: {'Content-Type':'application/json'},
            body: JSON.stringify({password:'private-fixture-password'})
        })).status''')
        assert status == 200, status
        events = []
        output = bytearray()
        def ws_open(ws):
            events.append({'event':'open','time':time.time()})
            ws.on('close',lambda:events.append({'event':'close','time':time.time()}))
            ws.on('framereceived', lambda data: output.extend(data.encode() if isinstance(data, str) else data))
        page.on('websocket', ws_open)
        def api(route):
            path = route.request.url.split(BASE)[-1]
            if path.startswith('/api/terminals'): return route.continue_()
            value = {'lanes':[{'pane_id':pane,'name':'fixture-worker','membership':{'scope':'project','project':'fixture'}}]} if path == '/api/projects' else {'ok':True,'channels':[]}
            route.fulfill(status=200,content_type='application/json',body=json.dumps(value))
        page.route('**/api/**',api)
        page.goto(BASE+'/dash/projects',wait_until='domcontentloaded')
        page.get_by_role('button',name='Open terminal').click(timeout=10000)
        page.get_by_text('Connected',exact=True).wait_for(timeout=10000)
        page.wait_for_timeout(2000)
        page.locator('.xterm-helper-textarea').focus()
        def wait_until(check, seconds, label):
            deadline = time.monotonic() + seconds
            while not check():
                assert time.monotonic() < deadline, label
                page.wait_for_timeout(100)
        def received_bytes():
            return (RUN/'received').read_bytes() if (RUN/'received').exists() else b''
        timings = {}
        payload = 'abcdefghijklmnopqrstuvwxyz0123456789'*4
        started = time.monotonic()
        page.keyboard.type(payload,delay=0)
        wait_until(lambda: received_bytes() == payload.encode(), 15, 'rapid keys not delivered')
        timings['rapid_keys_s'] = time.monotonic() - started
        # Exceed the input buffer, then start output only once the paste arrives.
        paste = 'p' * (48 * 1024)
        started = time.monotonic()
        page.keyboard.insert_text(paste)
        payload += paste
        wait_until(lambda: received_bytes() == payload.encode(), 30, 'paste not delivered')
        timings['paste_s'] = time.monotonic() - started
        started = time.monotonic()
        page.keyboard.type('!')
        payload += '!'
        wait_until(lambda: b't4955-output-end' in output, 30, 'output burst did not finish')
        timings['output_burst_s'] = time.monotonic() - started
        page.wait_for_timeout(1500)
        (RUN/'output.bin').write_bytes(output)
        received = (RUN/'received').read_bytes() if (RUN/'received').exists() else b''
        result = {'fixture':str(RUN),'host':HOST,'connect_address':'127.0.0.1','pane':pane,'non_owner':True,'sent':len(payload),'received':len(received),'exact':received==payload.encode(),'output_complete':b't4955-output-end' in output,'output_bytes':len(output),'timings':timings,'events':events,'status':page.locator('.project-terminal [role=status]').inner_text()}
        (RUN/'result.json').write_text(json.dumps(result,indent=2))
        print(json.dumps(result),flush=True)
        browser.close()
        assert result['exact'], result
        assert result['output_complete'], result
        assert [event['event'] for event in events] == ['open'], result
finally:
    if desk is not None:
        desk.terminate()
        try: desk.wait(timeout=5)
        except subprocess.TimeoutExpired: desk.kill(); desk.wait()
    subprocess.run(['/usr/bin/tmux','-L',SOCKET,'kill-server'],env=env,check=False,capture_output=True)
