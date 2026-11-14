"""Synthetic source-filter and opt-out races. No live daemon, mail or credentials."""
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
calls = []
pending = []
hold_internal = False
unsafe = None
unavailable = False

def item(mid, body, source=None, audience=None):
    row = {'id': mid, 'message_id': f'i{mid}', 'body': body, 'sender': 'synthetic', 'role': 'agent', 'created_at': '2026-11-13T00:00:00Z'}
    if source:
        row.update({'conversation_source': source, 'audience': audience, 'mailbox': 'owner' if source == 'human' else 'codex-rust-owner'})
    return row
ALL = item(900, 'ALL-UNCHANGED')
HUMAN = item(201, 'HUMAN: codex-rust-owner agent-only words do not classify this', 'human')
HUMAN_OLD = item(101, 'HUMAN-OLDER', 'human')
LANE = item(401, 'OWNER-LANE-PLAIN', 'owner_lane')
LANE_OLD = item(301, 'OWNER-LANE-OLDER', 'owner_lane')
AGENT = item(402, 'INTERNAL-AGENT-SECRET', 'owner_lane', 'agent')

def fulfill(route, data, status=200):
    route.fulfill(status=status, content_type='application/json', body=json.dumps(data))
def response(source, internal, before):
    rows = [ALL] if source == 'all' else [HUMAN_OLD] if source == 'human' and before else [HUMAN] if source == 'human' else [AGENT, LANE] if internal else [LANE_OLD] if before else [LANE]
    cursor = None if source == 'all' or internal or before else 'i201' if source == 'human' else 'i401'
    return {'items': rows, 'next_before': cursor, 'source_filter': source, 'include_internal': internal}
def mock(route):
    path = urlsplit(route.request.url).path
    query = parse_qs(urlsplit(route.request.url).query)
    calls.append((route.request.method, path, query))
    if path == '/api/channels':
        return fulfill(route, {'channels': []})
    if path == '/api/owner-thread':
        if query.get('paged') != ['true']:
            return fulfill(route, {'items': [ALL]})
        source = query.get('filter', ['all'])[0]
        internal = query.get('include_internal') == ['true']
        before = query.get('before', [None])[0]
        assert not internal or source == 'owner_lane'
        if before:
            assert before == ('i201' if source == 'human' else 'i401'), query
        if source != 'all' and unavailable:
            return fulfill(route, {'detail': 'Filtered history unavailable'}, 502)
        if source == 'human' and unsafe:
            data = response(source, internal, before)
            if unsafe == 'missing_scope':
                data = {'items': [ALL], 'next_before': None}
            else:
                data['items'] = [{**HUMAN, 'audience': 'agent', 'body': 'INTERNAL-AGENT-SECRET'}]
            return fulfill(route, data)
        if internal and hold_internal:
            pending.append((route, response(source, internal, before)))
            return
        return fulfill(route, response(source, internal, before))
    return fulfill(route, {'ok': True})

try:
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'], headless=True, args=['--no-sandbox', '--disable-dev-shm-usage'])
        for width in (360, 768, 1280):
            page = browser.new_page(viewport={'width': width, 'height': 900})
            page.route('**/api/**', mock)
            page.goto(base + '/dash/cos', wait_until='domcontentloaded')
            reader = page.get_by_role('article', name='Selected message')
            reader.get_by_text('ALL-UNCHANGED', exact=True).wait_for()
            choice = page.get_by_role('combobox', name='Inbox filter')
            assert choice.input_value() == 'all'
            assert page.get_by_role('checkbox').count() == 0
            choice.select_option('human')
            reader.get_by_text(HUMAN['body'], exact=True).wait_for()
            assert reader.get_by_text('ALL-UNCHANGED', exact=True).count() == 0
            page.get_by_role('button', name='Load older messages', exact=True).click()
            page.get_by_text('Start of conversation reached · 2 messages loaded', exact=True).wait_for()
            choice.select_option('owner_lane')
            reader.get_by_text('OWNER-LANE-PLAIN', exact=True).wait_for()
            toggle = page.get_by_role('checkbox', name='Include internal agent-only mail')
            assert not toggle.is_checked()
            page.get_by_role('button', name='Load older messages', exact=True).click()
            page.get_by_text('Start of conversation reached · 2 messages loaded', exact=True).wait_for()
            toggle.check()
            reader.get_by_text('INTERNAL-AGENT-SECRET', exact=True).wait_for()
            assert 'agent-only internal' in reader.inner_text()
            toggle.uncheck()
            assert page.get_by_text('INTERNAL-AGENT-SECRET', exact=True).count() == 0
            reader.get_by_text('OWNER-LANE-PLAIN', exact=True).wait_for()
            hold_internal = True
            toggle.check()
            for _ in range(100):
                if pending:
                    break
                page.wait_for_timeout(50)
            assert pending
            toggle.uncheck()
            reader.get_by_text('OWNER-LANE-PLAIN', exact=True).wait_for()
            hold_internal = False
            for route, data in pending:
                fulfill(route, data)
            pending.clear()
            page.wait_for_timeout(250)
            assert page.get_by_text('INTERNAL-AGENT-SECRET', exact=True).count() == 0, 'late internal body crossed opt-out'
            toggle.check()
            reader.get_by_text('INTERNAL-AGENT-SECRET', exact=True).wait_for()
            choice.select_option('human')
            reader.get_by_text(HUMAN['body'], exact=True).wait_for()
            assert page.get_by_text('INTERNAL-AGENT-SECRET', exact=True).count() == 0
            choice.select_option('owner_lane')
            reader.get_by_text('OWNER-LANE-PLAIN', exact=True).wait_for()
            assert not toggle.is_checked(), 'opt-in must reset after leaving owner-lane scope'
            for bad in ('missing_scope', 'agent_row'):
                unsafe = bad
                choice.select_option('human')
                page.get_by_role('alert').wait_for()
                assert page.get_by_text('ALL-UNCHANGED', exact=True).count() == 0
                assert page.get_by_text('INTERNAL-AGENT-SECRET', exact=True).count() == 0
                choice.select_option('all')
                reader.get_by_text('ALL-UNCHANGED', exact=True).wait_for()
            unsafe = None
            unavailable = True
            start = len(calls)
            choice.select_option('human')
            page.get_by_role('alert').wait_for()
            assert page.get_by_text('ALL-UNCHANGED', exact=True).count() == 0
            assert not any(path == '/api/owner-thread' and query.get('paged') != ['true'] for _, path, query in calls[start:]), 'filtered mode must not fall back to All'
            unavailable = False
            choice.select_option('owner_lane')
            reader.get_by_text('OWNER-LANE-PLAIN', exact=True).wait_for()
            assert not toggle.is_checked()
            assert page.evaluate('document.documentElement.scrollWidth <= innerWidth')
            if os.environ.get('OWNER_FILTER_SCREENSHOT_DIR'):
                page.screenshot(path=f"{os.environ['OWNER_FILTER_SCREENSHOT_DIR']}/filters-{width}.png")
            page.close()
        assert all(method == 'GET' for method, path, _ in calls if path != '/api/viewport'), calls
        browser.close()
finally:
    server.shutdown()
print('Owner Inbox filters passed360/768/1280: All default, metadata partitions, scoped cursors, explicit internal/opt-out/late response, scope reset, no unsafe fallback, GET-only')
