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
BASE = f'http://127.0.0.1:{PORT}'
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
    'DUNKINGSHEEP_TERMINAL_STREAM_HOSTS': f'127.0.0.1:{PORT}',
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
            urllib.request.urlopen(BASE + '/', timeout=1)
            break
        except Exception: time.sleep(.1)
    with sync_playwright() as p:
        browser = p.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'], headless=True, args=['--no-sandbox','--disable-dev-shm-usage'])
        context = browser.new_context()
        response = context.request.post(BASE+'/api/login', data={'password':'private-fixture-password'}, headers={'Origin':BASE})
        assert response.status == 200, response.text()
        page = context.new_page()
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
        payload = 'abcdefghijklmnopqrstuvwxyz0123456789'*4
        page.keyboard.type(payload,delay=0)
        page.wait_for_timeout(7000)
        # Exceed the server input buffer with a paste, then exercise many short
        # output notifications while keeping the same authenticated connection.
        paste = 'p' * (48 * 1024)
        page.keyboard.insert_text(paste)
        page.keyboard.type('!')
        page.wait_for_timeout(7000)
        payload += paste + '!'
        received = (RUN/'received').read_bytes() if (RUN/'received').exists() else b''
        result = {'fixture':str(RUN),'pane':pane,'non_owner':True,'sent':len(payload),'received':len(received),'exact':received==payload.encode(),'output_complete':b't4955-output-end' in output,'events':events,'status':page.locator('.project-terminal [role=status]').inner_text()}
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
