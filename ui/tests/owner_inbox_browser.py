"""Render a synthetic owner thread and intercept every API request; never use live data.

Run after `npm run build` with Playwright installed and CHROMIUM_PATH set.
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
ITEMS = [{'id': index, 'message_id': f'i{index}', 'sender': f'lane-{index}',
          'role': 'agent', 'transport': 'inbox', 'body': f'Message {index}\n\nFull body {index}',
          'created_at': f'2026-11-08T00:{index:02d}:00Z'} for index in range(1, 28)]
requests = []
mode = 'ready'
history_fail = False
history_cursors = []
cursor_failure = False
window_calls = []

class Handler(SimpleHTTPRequestHandler):
    def translate_path(self, path):
        return str(DIST / ('index.html' if path.startswith('/dash/') else path.lstrip('/')))

    def log_message(self, *args):
        pass

server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = f'http://127.0.0.1:{server.server_port}'

def mock(route):
    global history_fail
    query = parse_qs(urlsplit(route.request.url).query)
    path = urlsplit(route.request.url).path
    requests.append((route.request.method, path))
    if path == '/api/channels':
        value = {'channels': []}
    elif path == '/api/owner-thread':
        window_calls.append((mode, query.get('paged') == ['true'], bool(query.get('before'))))
        if mode in ('old_daemon', 'old_desk', 'auth_failure'):
            if query.get('paged') == ['true'] and mode != 'old_desk':
                status = 403 if mode == 'auth_failure' else 502
                return route.fulfill(status=status, content_type='application/json', body=json.dumps({'detail': 'Forbidden history paging' if status == 403 else 'history paging unavailable'}))
            return route.fulfill(status=200, content_type='application/json', body=json.dumps({'items': ITEMS}))
        if mode == 'failed':
            route.fulfill(status=503, content_type='application/json', body='{"detail":"Mailbox offline"}')
            return
        if mode == 'history':
            assert query.get('paged') == ['true']
            before = query.get('before', [None])[0]
            history_cursors.append(before)
            if before and cursor_failure:
                return route.fulfill(status=502, content_type='application/json', body='{"detail":"History cursor removed; reload"}')
            if before and history_fail:
                history_fail = False
                return route.fulfill(status=503, content_type='application/json', body='{"detail":"Older page offline"}')
            boundary = int(before[1:]) if before else 451
            rows = [{'id': index, 'message_id': f'i{index}', 'sender': f'lane-{index}', 'role': 'agent',
                     'body': f'HISTORY-BODY-{index}', 'created_at': '2026-11-08T00:00:00Z'}
                    for index in range(1, boundary)]
            page = rows[-200:]
            value = {'items': page, 'next_before': f"i{page[0]['id']}" if len(rows) > 200 else None}
        else:
            value = {'items': ITEMS if mode == 'ready' else [], 'next_before': None}
    else:
        value = {'ok': True}
    if path == '/api/owner-thread':
        value.update({'source_filter': 'all_sources', 'include_internal': False})
        for row in value.get('items', []):
            row.update({'audience': None, 'conversation_source': 'human', 'mailbox': 'owner'})
    route.fulfill(status=200, content_type='application/json', body=json.dumps(value))

try:
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'],
                                             headless=True, args=['--no-sandbox', '--disable-dev-shm-usage'])
        for width in (360, 768, 1280):
            page = browser.new_page(viewport={'width': width, 'height': 900})
            page.route('**/api/**', mock)
            page.goto(base + '/dash/cos', wait_until='domcontentloaded')
            page.get_by_role('heading', name='Inbox', exact=True).wait_for()
            page.locator('.owner-inbox-row').first.wait_for()
            assert page.locator('.owner-inbox-row').count() == 25
            assert 'lane-27' in page.locator('.owner-inbox-row').first.inner_text()
            assert 'Full body 27' in page.get_by_role('article', name='Selected message').inner_text()
            page.locator('.owner-inbox-row').nth(2).click()
            assert 'Full body 25' in page.get_by_role('article', name='Selected message').inner_text()
            page.get_by_role('button', name='Older', exact=True).click()
            assert page.locator('.owner-inbox-row').count() == 2
            page.locator('.owner-inbox-row').last.click()
            assert 'Full body 1' in page.get_by_role('article', name='Selected message').inner_text()
            # New arrivals and a failed refresh must preserve the selected message.
            ITEMS.append({'id': 28, 'sender': 'new-arrival', 'body': 'New message',
                          'role': 'agent', 'created_at': '2026-11-08T00:28:00Z'})
            page.wait_for_function("document.querySelector('.owner-inbox-pages').textContent.includes('28 loaded')")
            assert 'Full body 1' in page.get_by_role('article', name='Selected message').inner_text()
            mode = 'failed'
            page.get_by_role('alert').wait_for(timeout=10000)
            assert 'previously loaded' in page.get_by_role('alert').inner_text()
            assert 'Full body 1' in page.get_by_role('article', name='Selected message').inner_text()
            mode = 'ready'
            page.get_by_role('button', name='Retry', exact=True).click()
            page.get_by_role('alert').wait_for(state='detached')
            assert page.evaluate('document.documentElement.scrollWidth <= window.innerWidth')
            page.screenshot(path=str(DIST.parent / f'owner-inbox-{width}.png'))
            ITEMS.pop()
            page.close()
        mode = 'history'
        for width in (360, 768, 1280):
            history_fail = True
            page = browser.new_page(viewport={'width': width, 'height': 900})
            page.route('**/api/**', mock)
            page.goto(base + '/dash/cos', wait_until='domcontentloaded')
            reader = page.get_by_role('article', name='Selected message')
            reader.get_by_text('HISTORY-BODY-450', exact=True).wait_for()
            page.get_by_role('button', name='Load older messages', exact=True).click()
            page.get_by_role('alert').filter(has_text='Older page offline').wait_for()
            assert '200 loaded' in page.locator('.owner-inbox-pages').inner_text()
            page.get_by_role('button', name='Retry older messages', exact=True).click()
            page.wait_for_function("document.querySelector('.owner-inbox-pages').textContent.includes('400 loaded')")
            assert 'HISTORY-BODY-450' in reader.inner_text(), 'loading older must preserve selection'
            assert 'lane-250' in page.locator('.owner-inbox-row').first.inner_text()
            page.get_by_role('button', name='Load older messages', exact=True).click()
            page.get_by_text('Start of conversation reached · 450 messages loaded', exact=True).wait_for()
            assert page.get_by_role('button', name='Load older messages', exact=True).count() == 0
            assert 'lane-50' in page.locator('.owner-inbox-row').first.inner_text()
            page.get_by_role('button', name='Older', exact=True).click()
            page.locator('.owner-inbox-row').last.click()
            reader.get_by_text('HISTORY-BODY-1', exact=True).wait_for()
            page.wait_for_timeout(5500)
            assert '450 loaded' in page.locator('.owner-inbox-pages').inner_text()
            assert 'HISTORY-BODY-1' in reader.inner_text(), 'newest polling must retain loaded history'
            assert page.evaluate('document.documentElement.scrollWidth <= innerWidth')
            if os.environ.get('OWNER_HISTORY_SCREENSHOT_DIR'):
                page.screenshot(path=f"{os.environ['OWNER_HISTORY_SCREENSHOT_DIR']}/s-f-history-{width}.png")
            page.close()
        assert 'i251' in history_cursors and 'i51' in history_cursors
        for mode in ('old_daemon', 'old_desk', 'auth_failure'):
            page = browser.new_page()
            page.route('**/api/**', mock)
            page.goto(base + '/dash/cos', wait_until='domcontentloaded')
            page.get_by_role('alert').wait_for()
            assert page.locator('.owner-inbox-row').count() == 0, 'unconfirmed scope must fail closed'
            page.close()
        assert not any(mode in ('old_daemon', 'old_desk', 'auth_failure') and not paged for mode, paged, before in window_calls)
        mode = 'empty'
        page = browser.new_page()
        page.route('**/api/**', mock)
        page.goto(base + '/dash/cos')
        page.get_by_text('No messages match these Inbox filters.', exact=True).wait_for()
        page.close()
        # The existing shell reports viewport dimensions separately from mailbox browsing.
        assert all(method == 'GET' for method, path in requests if path != '/api/viewport'), requests
        assert {path for _, path in requests} <= {'/api/channels', '/api/owner-thread', '/api/version', '/api/viewport'}
        browser.close()
finally:
    server.shutdown()
print('owner inbox browser passed: 450-row complete cursor history/ties/retry/EOF/poll preservation + unconfirmed scope and auth fail closed, newest-first, paging, selection, polling, errors, empty, GET-only at 360, 768 and 1280')
