"""Render a synthetic owner thread and intercept every API request; never use live data.

Run after `npm run build` with Playwright installed and CHROMIUM_PATH set.
"""
import json
import os
import threading
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit

from playwright.sync_api import sync_playwright

DIST = Path(__file__).resolve().parents[1] / 'dist'
assert (DIST / 'index.html').is_file(), 'build ui/ first'
ITEMS = [{'id': index, 'message_id': f'i{index}', 'sender': f'lane-{index}',
          'role': 'agent', 'transport': 'inbox', 'body': f'Message {index}\n\nFull body {index}',
          'created_at': f'2026-11-08T00:{index:02d}:00Z'} for index in range(1, 28)]
requests = []
mode = 'ready'

class Handler(SimpleHTTPRequestHandler):
    def translate_path(self, path):
        return str(DIST / ('index.html' if path.startswith('/dash/') else path.lstrip('/')))

    def log_message(self, *args):
        pass

server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = f'http://127.0.0.1:{server.server_port}'

def mock(route):
    path = urlsplit(route.request.url).path
    requests.append((route.request.method, path))
    if path == '/api/channels':
        value = {'channels': []}
    elif path == '/api/owner-thread':
        if mode == 'failed':
            route.fulfill(status=503, content_type='application/json', body='{"detail":"Mailbox offline"}')
            return
        value = {'items': ITEMS if mode == 'ready' else []}
    else:
        value = {'ok': True}
    route.fulfill(status=200, content_type='application/json', body=json.dumps(value))

try:
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'],
                                             headless=True, args=['--no-sandbox', '--disable-dev-shm-usage'])
        for width in (360, 768, 1280):
            page = browser.new_page(viewport={'width': width, 'height': 900})
            page.route('**/api/**', mock)
            page.goto(base + '/dash/cos', wait_until='domcontentloaded')
            page.get_by_role('heading', name='Owner inbox', exact=True).wait_for()
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
        mode = 'empty'
        page = browser.new_page()
        page.route('**/api/**', mock)
        page.goto(base + '/dash/cos')
        page.get_by_text('No messages in the owner conversation.', exact=True).wait_for()
        page.close()
        # The existing shell reports viewport dimensions separately from mailbox browsing.
        assert all(method == 'GET' for method, path in requests if path != '/api/viewport'), requests
        assert {path for _, path in requests} <= {'/api/channels', '/api/owner-thread', '/api/version', '/api/viewport'}
        browser.close()
finally:
    server.shutdown()
print('owner inbox browser passed: newest-first, paging, selection, polling, errors, empty, GET-only at 360, 768 and 1280')
