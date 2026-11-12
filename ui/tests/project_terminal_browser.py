"""Synthetic Projects/terminal workflow; never connects to a real daemon or tmux."""
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
lanes = [
    {'pane_id':'%1','name':'Worker','membership':{'scope':'project','project':'alpha'}},
    {'pane_id':'%2','name':'Owner','membership':{'scope':'project','project':'alpha'}},
]
received = []
connections = []
def api(route):
    path = urlsplit(route.request.url).path
    if path == '/api/projects': value = {'lanes':lanes}
    elif path == '/api/terminals': value = [{'pane_id':'%1','name':'worker','cols':80,'rows':24}]
    elif path.endswith('/resize'): value = {'cols':80,'rows':24}
    else: value = {'ok':True,'channels':[]}
    route.fulfill(status=200, content_type='application/json', body=json.dumps(value))
def websocket(ws):
    connections.append(ws)
    ws.send(b'\x1b[31mLIVE-OUTPUT\x1b[0m\r\n')
    def message(data):
        received.append(data)
        if len(connections) == 1: ws.close()
    ws.on_message(message)
try:
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'], headless=True, args=['--no-sandbox','--disable-dev-shm-usage'])
        page = browser.new_page(viewport={'width':390,'height':900})
        page.route('**/api/**', api)
        page.route_web_socket('**/api/terminals/*/stream**', websocket)
        page.goto(base + '/dash/projects', wait_until='domcontentloaded')
        page.get_by_role('heading',name='Project: alpha').wait_for()
        assert page.get_by_role('button',name='Open terminal').count() == 1
        assert page.get_by_role('row').filter(has_text='Owner').get_by_role('button',name='Open terminal').count() == 0
        page.get_by_role('button',name='Open terminal').click()
        page.get_by_text('Connected',exact=True).wait_for()
        page.get_by_text('LIVE-OUTPUT',exact=False).first.wait_for()
        page.locator('.xterm-helper-textarea').focus()
        page.keyboard.type('x')
        page.wait_for_function('document.querySelector(".project-terminal [role=status]")?.textContent === "Connected"', timeout=10000)
        page.wait_for_timeout(1800)
        assert any((data == b'x' or data == 'x') for data in received), received
        assert len(connections) >= 2, len(connections)
        assert len(received) == 1, received
        page.get_by_role('button',name='Close terminal').click()
        assert page.locator('.project-terminal').count() == 0
        page.close()
        for status in (404, 401):
            denied = browser.new_page()
            denied.route('**/api/**', api)
            denied.route('**/api/terminals', lambda route: route.fulfill(status=status, content_type='application/json', body='{}'))
            denied.goto(base + '/dash/projects', wait_until='domcontentloaded')
            if status == 401:
                denied.get_by_label('Owner password').wait_for()
                assert denied.get_by_role('button', name='Open terminal').count() == 0
            else:
                denied.get_by_role('heading', name='Project: alpha').wait_for()
                assert denied.get_by_role('button', name='Open terminal').count() == 0
                assert denied.get_by_role('link', name='Worker').count() == 1
            denied.close()
        browser.close()
finally:
    server.shutdown()
print('project terminal synthetic browser workflow passed')
