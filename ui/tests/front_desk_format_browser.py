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
RAW = '''A table:

| Name | Value |
| --- | --- |
| Alpha | 42 |

```text
if (x < 2) print("safe");
```

+-----+
| A B |
+-----+

<strong>Safe HTML</strong> and <em>emphasis</em>.

<script>window.__formatAttack = 1</script><img src=x onerror="window.__formatAttack=2">
<a href="javascript:window.__formatAttack=3" onclick="window.__formatAttack=4">Unsafe link</a>
'''

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
    if path == '/api/channels':
        value = {'channels': [{'id': 'cos', 'name': 'Front Desk', 'status': 'ready',
                               'can_message': True, 'pane_id': '%201'}]}
    elif path == '/api/owner-thread':
        value = {'items': [{'id': 1, 'role': 'agent', 'body': RAW,
                            'created_at': '2026-11-08T00:00:00Z', 'attachments': []},
                           {'id': 2, 'role': 'owner', 'body': RAW,
                            'created_at': '2026-11-08T00:01:00Z', 'attachments': []}]}
    else:
        value = {'ok': True}
    route.fulfill(status=200, content_type='application/json', body=json.dumps(value))

try:
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'],
                                             headless=True, args=['--no-sandbox', '--disable-dev-shm-usage'])
        for width in (390, 1440):
            page = browser.new_page(viewport={'width': width, 'height': 900})
            page.route('**/api/**', mock)
            page.goto(base + '/dash/cos', wait_until='domcontentloaded')
            messages = page.locator('.front-desk-markdown')
            messages.first.wait_for()
            assert messages.count() == 1
            for index in range(2):
                page.locator('.owner-inbox-row').nth(index).click()
                message = messages.first
                assert message.locator('table tbody tr').count() == 1
                assert 'Alpha' in message.locator('table').inner_text()
                assert 'if (x < 2)' in message.locator('pre code').inner_text()
                diagram = message.locator('.ascii-diagram')
                assert '+-----+\n| A B |\n+-----+' in diagram.text_content()
                assert diagram.evaluate('(e) => getComputedStyle(e).whiteSpace') == 'pre'
                assert message.locator('strong').get_by_text('Safe HTML').count() == 1
                assert message.locator('em').get_by_text('emphasis').count() == 1
                assert message.locator('script, img, iframe, svg').count() == 0
                assert message.locator('a').get_by_text('Unsafe link').get_attribute('href') is None
            assert page.evaluate('window.__formatAttack || 0') == 0
            raw = page.evaluate("async () => (await (await fetch('/api/owner-thread?limit=50')).json()).items.map(x => x.body)")
            assert raw == [RAW, RAW]
            assert page.locator('.owner-inbox-row').count() == 2
            page.get_by_role('link', name='Open conversation / send a message').click()
            page.locator('.conversation .front-desk-markdown').first.wait_for()
            assert page.locator('.conversation .front-desk-markdown').count() == 2
            page.get_by_role('link', name='Back to Inbox', exact=True).click()
            page.locator('.owner-inbox-reader').wait_for()
            page.close()
        browser.close()
finally:
    server.shutdown()
print('front desk formatting browser regression passed at 390 and 1440')
