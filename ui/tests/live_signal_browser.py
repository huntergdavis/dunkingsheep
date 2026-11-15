"""Synthetic UI regression: serve a built dist, intercept every API, and never send live data.

Run with a Python environment containing Playwright and CHROMIUM_PATH pointing at a local
Chromium executable. The browser and the temporary HTTP listener must share a namespace.
"""
import json
import os
import threading
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

from playwright.sync_api import sync_playwright

DIST = Path(__file__).resolve().parents[1] / 'dist'
assert (DIST / 'index.html').is_file(), 'build ui/ first'


class Handler(SimpleHTTPRequestHandler):
    def translate_path(self, path):
        return str(DIST / ('index.html' if path.startswith('/dash/') else path.lstrip('/')))

    def log_message(self, *args):
        pass


server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = f'http://127.0.0.1:{server.server_port}'
requests = []
pane_id = '%201'
channel_pane = pane_id
fleet_contains = True


def mock(route):
    request = route.request
    path = urlsplit(request.url).path
    requests.append((request.method, path))
    if path == '/api/channels':
        value = {'channels': [{'id': 'cos', 'name': 'Front Desk', 'status': 'ready',
                               'can_message': True, 'pane_id': channel_pane}]}
    elif path == '/api/owner-thread':
        value = {'items': [{'id': 1, 'role': 'owner', 'body': 'fixture message',
                            'created_at': '2026-11-08T00:00:00Z', 'delivery': 'uncertain',
                            'delivery_detail': 'fixture', 'attachments': []}]}
    elif path == '/api/fleet':
        value = {'state': 'ok', 'rows': [{'pane_id': pane_id, 'name': 'Fixture lane',
                  'agent': 'fixture', 'project': 'Scratch', 'pending': 0, 'pending_state': 'ok'}],
                 'unrouted': {'state': 'ok', 'rows': [], 'note': 'fixture'}}
        if not fleet_contains:
            value['rows'] = []
    elif path == '/api/pane-text':
        value = {'pane': pane_id, 'text': 'fixture terminal output'}
    else:
        value = {'ok': True}
    route.fulfill(status=200, content_type='application/json', body=json.dumps(value))


try:
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'],
                                             headless=True, args=['--no-sandbox', '--disable-dev-shm-usage'])
        for width in (390, 1440):
            for case in ('matched', 'missing', 'unrouted'):
                channel_pane = None if case == 'missing' else pane_id
                fleet_contains = case != 'unrouted'
                requests.clear()
                page = browser.new_page(viewport={'width': width, 'height': 900})
                page.route('**/api/**', mock)
                # /dash/cos opens the Inbox (t5010); the conversation with Live signal is this panel.
                page.goto(base + '/dash/cos?panel=conversation', wait_until='domcontentloaded')
                page.get_by_role('button', name='Live signal').click()
                # t4646 retired the Terminals page: Live signal opens the lane in Projects, with
                # the conversation's pane when it has one.
                page.wait_for_url('**/dash/projects*')
                query = parse_qs(urlsplit(page.url).query)
                if case == 'missing':
                    assert 'pane' not in query, page.url
                else:
                    assert query.get('pane') == [pane_id], page.url
                assert not any(path == '/api/pane-text' for _, path in requests), requests
                assert not any(method != 'GET' and path != '/api/viewport' for method, path in requests), requests
                page.close()
        browser.close()
finally:
    server.shutdown()
print('live signal browser regression passed at 390 and 1440')
