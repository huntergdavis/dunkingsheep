"""Synthetic owner-terminal UI check. All API and websocket traffic is mocked."""
import json, os, threading
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit
from playwright.sync_api import sync_playwright

DIST = Path(__file__).resolve().parents[1] / 'dist'
class Handler(SimpleHTTPRequestHandler):
    def translate_path(self, path):
        return str(DIST / ('index.html' if path.startswith('/dash/') else path.lstrip('/')))
    def log_message(self, *args): pass
server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = f'http://127.0.0.1:{server.server_port}'
received = []
def api(route):
    path = urlsplit(route.request.url).path
    if path == '/api/projects':
        value = {'lanes':[{'pane_id':'%3','name':'Worker','membership':{'scope':'project','project':'alpha'}}]}
    elif path == '/api/terminals': value = [{'pane_id':'%1','name':'__owner_terminal__','agent':'owner','cols':80,'rows':24}]
    elif path.endswith('/resize'): value = {'cols':80,'rows':24}
    else: value = {'ok':True,'channels':[]}
    route.fulfill(status=200, content_type='application/json', body=json.dumps(value))
def websocket(ws):
    ws.send(b'OWNER-LIVE\r\n')
    ws.on_message(lambda data: received.append(data))
try:
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'], headless=True, args=['--no-sandbox','--disable-dev-shm-usage'])
        page = browser.new_page(viewport={'width':390,'height':900})
        page.route('**/api/**', api)
        page.route_web_socket('**/api/terminals/*/stream**', websocket)
        page.goto(base + '/dash/projects', wait_until='domcontentloaded')
        page.get_by_role('button', name='Open owner terminal').click()
        page.get_by_text('Connected', exact=True).wait_for()
        page.get_by_text('OWNER-LIVE', exact=False).first.wait_for()
        page.locator('.xterm-helper-textarea').focus()
        page.keyboard.type('x')
        page.wait_for_timeout(500)
        assert any(data == b'x' or data == 'x' for data in received), received
        page.get_by_role('button', name='Close terminal').click()
        assert page.locator('.project-terminal').count() == 0
        browser.close()
finally:
    server.shutdown()
print('synthetic owner terminal UI workflow passed')
