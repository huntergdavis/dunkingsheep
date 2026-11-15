"""Server-zone timestamp presentation with a different browser zone; private APIs only."""
import json
import os
import threading
from datetime import datetime
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit

from playwright.sync_api import sync_playwright

DIST = Path(__file__).resolve().parents[1] / 'dist'
ISO = '2026-11-15T20:05:00Z'
SECONDS = datetime.fromisoformat(ISO.replace('Z', '+00:00')).timestamp()
LOCAL = 'Sun, Nov 15, 2026, 12:05 PM PST'
server_zone = 'America/Los_Angeles'
signed_in = True


class Handler(SimpleHTTPRequestHandler):
    def translate_path(self, path):
        path = urlsplit(path).path
        return str(DIST / ('index.html' if path.startswith('/dash/') else path.lstrip('/')))

    def log_message(self, *args):
        pass


def api(route):
    global signed_in
    path = urlsplit(route.request.url).path
    status = 200
    if path == '/api/session':
        status = 200 if signed_in else 401
        value = {'ok': signed_in, 'time_zone': server_zone} if signed_in else {'detail': 'Sign in required'}
    elif path == '/api/login':
        signed_in = True
        value = {'ok': True}
    elif path == '/api/projects':
        value = {'lanes': [{'pane_id': '%21', 'name': 'Fixture', 'harness': 'claude',
                           'membership': {'scope': 'project', 'project': 'alpha'}}]}
    elif path == '/api/ops-board':
        value = {'budget': {'providers': [{'provider': 'claude', 'state': 'ok', 'age_s': 0,
                  'windows': [{'name': 'session', 'percent_used': 30, 'resets_at': ISO}]}]}}
    elif path == '/api/terminals':
        value = []
    elif path == '/api/owner-thread':
        value = {'source_filter': 'human_threads', 'include_internal': False, 'view_revision': 0,
                 'next_before': None, 'items': [{'id': 1, 'message_id': 'i1', 'sender': 'Fixture',
                    'role': 'agent', 'body': 'Timestamp fixture', 'created_at': ISO,
                    'mailbox': 'owner', 'audience': None, 'conversation_source': 'human',
                    'thread_version': 1, 'parent_id': None,
                    'root_id': 'i1', 'conversation_kind': 'human', 'sender_kind': 'agent'}]}
    elif path == '/api/tasks':
        value = {'scope': 'global', 'view': 'active', 'legacy_limited': False,
                 'total': 1, 'next_cursor': None, 'checked_at': SECONDS, 'tasks': [
                    {'id': 't1', 'title': 'Timestamp task', 'status': 'open', 'priority': 'high',
                     'assignees': ['%21'], 'created_at': SECONDS, 'block_reason': None}]}
    elif path == '/api/tasks/metrics':
        value = {'current': {'by_status': {'open': 1}}, 'throughput': {'completed': 0, 'claimed': 1},
                 'window': {'since': SECONDS, 'until': SECONDS}}
    elif path == '/api/project-context':
        value = {'map': {'state': 'ok'}, 'projects': [{'name': 'alpha', 'state': 'ok'}]}
    elif path == '/api/project-context/project/alpha':
        value = {'state': 'ok', 'entries': [{'slug': 'fixture', 'class': 'fact',
            'subject': 'Timestamp fact', 'body': 'Fixture', 'stated': ISO, 'ground_truth': False}]}
    else:
        value = {'ok': True}
    route.fulfill(status=status, content_type='application/json', body=json.dumps(value))


server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = f'http://127.0.0.1:{server.server_port}'
try:
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'],
            headless=True, args=['--no-sandbox', '--disable-dev-shm-usage'])
        context = browser.new_context(timezone_id='Asia/Tokyo', viewport={'width': 390, 'height': 900})
        for path, selector in [('/dash/projects', '.lane-usage-label'),
                               ('/dash/cos', '.owner-inbox-row time'),
                               ('/dash/tasks', '.tasks-checked time'),
                               ('/dash/databases?project=alpha', '.info-provenance')]:
            page = context.new_page()
            page.route('**/api/**', api)
            page.goto(base + path, wait_until='domcontentloaded')
            label = page.locator(selector).first
            label.wait_for()
            assert LOCAL in label.inner_text(), (path, label.inner_text())
            assert label.get_attribute('title') in (ISO, '2026-11-15T20:05:00.000Z')
            page.close()
        # Login must fetch session configuration before mounting timestamped views.
        signed_in = False
        server_zone = 'Europe/Paris'
        page = context.new_page()
        page.route('**/api/**', api)
        page.goto(base + '/dash/tasks', wait_until='domcontentloaded')
        page.get_by_label('Owner password').fill('private-fixture')
        page.get_by_role('button', name='Open desk', exact=True).click()
        label = page.locator('.tasks-checked time')
        label.wait_for()
        assert '09:05 PM GMT+1' in label.inner_text(), label.inner_text()
        page.close()
        # Unresolved server configuration uses explicitly labelled UTC, never Tokyo.
        server_zone = None
        page = context.new_page()
        page.route('**/api/**', api)
        page.goto(base + '/dash/tasks', wait_until='domcontentloaded')
        label = page.locator('.tasks-checked time')
        label.wait_for()
        assert label.inner_text() == 'Sun, Nov 15, 2026, 08:05 PM UTC'
        assert label.get_attribute('title') == '2026-11-15T20:05:00.000Z'
        context.close()
        browser.close()
finally:
    server.shutdown()
print('server-zone timestamps: Projects, Inbox, Tasks, Databases, login and unknown-zone cases passed')
