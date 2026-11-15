"""t5043: per-lane usage bars in the Projects view, with mocked desk API responses only."""
import json, os, threading
from datetime import datetime, timedelta, timezone
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
def iso(minutes_ago):
    return (datetime.now(timezone.utc) - timedelta(minutes=minutes_ago)).isoformat()
member = {'scope': 'project', 'project': 'alpha'}
lanes = [
    {'pane_id': '%11', 'name': 'Writer A', 'harness': 'claude', 'membership': member, 'state': 'idle'},
    {'pane_id': '%12', 'name': 'Writer B', 'harness': 'claude', 'membership': member, 'state': 'idle'},
    {'pane_id': '%13', 'name': 'Codex C', 'harness': 'codex', 'membership': member, 'state': 'idle'},
    {'pane_id': '%14', 'name': 'Pi D', 'harness': 'pi', 'membership': member, 'state': 'idle'},
    {'pane_id': '%15', 'name': 'Shell E', 'harness': None, 'membership': member, 'state': 'idle'},
]
budget = {'state': 'ok', 'providers': [
    {'provider': 'claude', 'state': 'ok', 'meaning': 'percent_used', 'measured_at': iso(4),
     'windows': [{'name': 'five_hour', 'percent_used': 37.0, 'resets_at': '2026-11-15T14:00:00Z'}, {'name': 'weekly_all', 'percent_used': 12.0}]},
    {'provider': 'codex', 'state': 'stale', 'meaning': 'percent_used', 'measured_at': iso(200), 'reason': 'newest snapshot is too old', 'windows': [{'name': 'primary', 'percent_used': 90}]},
    {'provider': 'pi', 'state': 'ok', 'meaning': 'local_session_estimate', 'measured_at': iso(1), 'windows': [], 'local_estimate': {'session_cost_usd': 0.5}},
]}
def api(route):
    path = urlsplit(route.request.url).path
    if path == '/api/projects': value = {'lanes': lanes}
    elif path == '/api/ops-board': value = {'budget': budget}
    else: value = {'ok': True, 'channels': []}
    route.fulfill(status=200, content_type='application/json', body=json.dumps(value))
try:
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'], headless=True, args=['--no-sandbox', '--disable-dev-shm-usage'])
        for width in (360, 768, 1280):
            page = browser.new_page(viewport={'width': width, 'height': 900})
            page.route('**/api/**', api)
            page.goto(base + '/dash/projects', wait_until='domcontentloaded')
            page.locator('[data-lane-usage="%11"] [role=meter]').wait_for()
            a = page.locator('[data-lane-usage="%11"]').inner_text()
            b = page.locator('[data-lane-usage="%12"]').inner_text()
            assert 'shared: claude account' in a and '37% of five_hour used' in a and 'measured 4 min ago' in a, a
            assert a == b, 'two lanes on one account show the same account numbers'
            assert page.locator('[data-lane-usage="%11"] [role=meter]').get_attribute('aria-valuenow') == '37'
            c = page.locator('[data-lane-usage="%13"]')
            assert 'usage reading is stale' in c.inner_text() and 'newest snapshot is too old' in c.inner_text()
            assert c.locator('[role=meter]').count() == 0, 'no bar for a stale reading'
            d = page.locator('[data-lane-usage="%14"]')
            assert 'estimate: local session cost $0.50' in d.inner_text() and d.locator('[role=meter]').count() == 0
            e = page.locator('[data-lane-usage="%15"]')
            assert "usage unavailable" in e.inner_text() and "provider is not known" in e.inner_text()
            assert e.locator('[role=meter]').count() == 0
            page.locator('[data-lane-usage="%11"] summary').click()
            details = page.locator('[data-lane-usage="%11"] details').inner_text()
            assert 'weekly_all: 12% used' in details and 'Token totals per lane are not measured yet.' in details, details
            assert not page.evaluate('document.documentElement.scrollWidth > window.innerWidth'), f'page scrolls sideways at {width}'
            page.close()
        browser.close()
finally:
    server.shutdown()
print('lane usage browser check passed at 360, 768 and 1280')
