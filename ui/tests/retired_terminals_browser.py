"""S5 UI-only checks. Every API request and terminal websocket is synthetic."""
import json
import os
import threading
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit
from playwright.sync_api import sync_playwright

DIST = Path(__file__).resolve().parents[1] / 'dist'
class Handler(SimpleHTTPRequestHandler):
    def translate_path(self, path):
        return str(DIST / ('index.html' if path.startswith('/dash/') else path.lstrip('/')))
    def log_message(self, *args):
        pass
server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = f'http://127.0.0.1:{server.server_port}'
terminal_status = 200
requests = []
connections = []
lanes = [
    {'pane_id': '%1', 'name': 'Worker', 'membership': {'scope': 'project', 'project': 'alpha'}},
    {'pane_id': '%2', 'name': 'Unavailable lane', 'membership': {'scope': 'project', 'project': 'alpha'}},
]
def mock(route):
    path = urlsplit(route.request.url).path
    requests.append((route.request.method, path))
    if path == '/api/projects':
        data = {'lanes': lanes}
    elif path == '/api/terminals':
        return route.fulfill(status=terminal_status, content_type='application/json', body=json.dumps([{'pane_id': '%1', 'name': 'worker', 'cols': 80, 'rows': 24}] if terminal_status == 200 else {}))
    elif path == '/api/channels':
        data = {'channels': [{'id': 'cos', 'name': 'Owner', 'pane_id': '%1', 'can_message': True}]}
    elif path == '/api/owner-thread':
        data = {'items': [{'id': 1, 'role': 'owner', 'body': 'Synthetic uncertain question', 'delivery': 'uncertain', 'created_at': '2026-11-13T00:00:00Z'}]}
    elif path == '/api/fleet':
        data = {'state': 'ready', 'rows': [], 'unrouted': []}
    elif path.endswith('/resize'):
        data = {'cols': 80, 'rows': 24}
    else:
        data = {'ok': True}
    route.fulfill(status=200, content_type='application/json', body=json.dumps(data))
def websocket(ws):
    connections.append(ws.url)
    assert parse_qs(urlsplit(ws.url).query).get('expected_pane') == ['%1']
    ws.send(b'SYNTHETIC-LIVE-TERMINAL\r\n')
def no_old_links(page):
    assert page.locator('a[href^="/dash/fleet"]').count() == 0
    assert page.get_by_role('link', name='Terminals', exact=True).count() == 0
def connected(page):
    page.locator('.project-terminal').get_by_text('Connected', exact=True).wait_for()
    no_old_links(page)
def close_terminal(page):
    page.get_by_role('button', name='Close terminal', exact=True).click()
    assert page.locator('.project-terminal').count() == 0

try:
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'], headless=True, args=['--no-sandbox', '--disable-dev-shm-usage'])
        for width in (360, 768, 1280):
            page = browser.new_page(viewport={'width': width, 'height': 900})
            page.route('**/api/**', mock)
            page.route_web_socket('**/api/terminals/*/stream**', websocket)
            page.goto(base + '/dash/fleet?pane=%251&keep=yes#output', wait_until='domcontentloaded')
            connected(page)
            assert urlsplit(page.url).path == '/dash/projects'
            assert parse_qs(urlsplit(page.url).query) == {'pane': ['%1'], 'keep': ['yes']}
            assert urlsplit(page.url).fragment == 'output'
            close_terminal(page)
            page.wait_for_timeout(250)
            assert page.locator('.project-terminal').count() == 0, 'close must not auto-reopen'
            page.get_by_role('link', name='Worker', exact=True).click()
            connected(page)
            close_terminal(page)
            page.get_by_role('row').filter(has_text='Worker').get_by_role('button', name='Open terminal', exact=True).click()
            connected(page)
            close_terminal(page)
            page.goto(base + '/dash/fleet', wait_until='domcontentloaded')
            page.get_by_role('heading', name='Projects', exact=True).wait_for()
            assert urlsplit(page.url).path == '/dash/projects'
            assert page.locator('.project-terminal').count() == 0
            no_old_links(page)
            page.evaluate("history.pushState(null, '', '/dash/fleet?pane=%251'); dispatchEvent(new PopStateEvent('popstate'))")
            connected(page)
            assert urlsplit(page.url).path == '/dash/projects'
            page.goto(base + '/dash/fleet?pane=%252', wait_until='domcontentloaded')
            page.get_by_text('Selected lane %2', exact=False).wait_for()
            row = page.get_by_role('row').filter(has_text='Unavailable lane')
            row.get_by_text('No terminal:', exact=False).wait_for()
            assert row.get_by_role('link', name='Unavailable lane', exact=True).get_attribute('aria-current') == 'true'
            assert page.locator('.project-terminal').count() == 0
            no_old_links(page)
            for status, reason in [(403, 'not trusted'), (503, 'could not read its lane records')]:
                terminal_status = status
                page.goto(base + '/dash/projects', wait_until='domcontentloaded')
                for lane in lanes:
                    row = page.get_by_role('row').filter(has_text=lane['name'])
                    row.locator('.lane-terminal-reason').filter(has_text=reason).wait_for()
                    assert row.get_by_role('button', name='Open terminal', exact=True).count() == 0
                no_old_links(page)
            terminal_status = 401
            page.goto(base + '/dash/projects', wait_until='domcontentloaded')
            page.get_by_label('Owner password').wait_for()
            assert page.locator('.project-terminal').count() == 0
            terminal_status = 200
            page.goto(base + '/dash/cos?panel=conversation', wait_until='domcontentloaded')
            page.get_by_role('button', name='Live signal', exact=True).click()
            connected(page)
            assert parse_qs(urlsplit(page.url).query).get('pane') == ['%1']
            assert page.evaluate('document.documentElement.scrollWidth <= innerWidth')
            if os.environ.get('S5_SCREENSHOT_DIR'):
                page.screenshot(path=f"{os.environ['S5_SCREENSHOT_DIR']}/s5-{os.environ.get('S5_VARIANT', 'a')}-{width}.png")
            page.close()
        assert all(method == 'GET' or path.endswith('/resize') or path == '/api/viewport' for method, path in requests), requests
        browser.close()
finally:
    server.shutdown()
print('S5 passed360/768/1280: lane/button/Inspect terminals, legacy redirect/pane/query/hash/popstate, no fleet links, 200/401/403/503, unavailable lane, no sends')
