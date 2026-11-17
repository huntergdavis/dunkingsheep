"""t3250/t5241: the read-only project information panel in the Databases tab, with mocked desk API responses only."""
import json, os, threading
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit, parse_qs
from playwright.sync_api import sync_playwright

DIST = Path(__file__).resolve().parents[1] / 'dist'
class Handler(SimpleHTTPRequestHandler):
    def translate_path(self, path):
        return str(DIST / ('index.html' if path.startswith('/dash/') else path.lstrip('/')))
    def log_message(self, *args): pass
server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = f'http://127.0.0.1:{server.server_port}'
ALPHA = [
    {'slug': 'alpha-rule', 'class': 'ground-truth', 'ground_truth': True, 'type': 'rule', 'authority': 'owner', 'source': 'owner mail', 'subject': 'Alpha rule', 'stale': False, 'retired': False, 'superseded_by': None, 'body': '<img src=x onerror=alert(1)> only text', 'body_truncated': False},
    {'slug': 'alpha-fact', 'class': 'fact', 'ground_truth': False, 'type': 'measurement', 'authority': 'measured', 'subject': 'Alpha numbers', 'stale': True, 'retired': False, 'superseded_by': None, 'body': 'measured', 'body_truncated': False},
    {'slug': 'alpha-old', 'class': 'fact', 'ground_truth': False, 'subject': 'Alpha old', 'stale': False, 'retired': False, 'superseded_by': 'alpha-fact', 'body': 'old', 'body_truncated': True},
]
requests = []
def api(route):
    url = urlsplit(route.request.url)
    path = url.path
    requests.append((route.request.method, path))
    if path == '/api/projects': value = {'lanes': [{'pane_id': '%11', 'name': 'Writer A', 'membership': {'scope': 'project', 'project': 'alpha'}, 'state': 'idle'}]}
    elif path == '/api/project-context':
        value = {'map': {'state': 'ok'}, 'global': {'state': 'ok'}, 'projects': [{'name': 'alpha', 'state': 'ok'}, {'name': 'broken', 'state': 'mapping_invalid', 'reason': 'the directory is not an absolute path'}]}
    elif path == '/api/project-context/project/alpha':
        q = parse_qs(url.query).get('q', [''])[0].lower()
        entries = [e for e in ALPHA if not q or q in (e['subject'] + e['body']).lower()]
        value = {'project': 'alpha', 'state': 'ok' if entries else 'no_match', 'total': len(entries), 'shown': len(entries), 'truncated': False, 'skipped_links': 1, 'ground_truths': 1, 'entries': entries}
    elif path == '/api/project-context/project/broken': value = {'project': 'broken', 'state': 'mapping_invalid', 'reason': 'the directory is not an absolute path'}
    elif path == '/api/project-context/global': value = {'scope': 'global', 'state': 'empty', 'total': 0, 'entries': []}
    else: value = {'ok': True, 'channels': []}
    route.fulfill(status=200, content_type='application/json', body=json.dumps(value))
try:
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'], headless=True, args=['--no-sandbox', '--disable-dev-shm-usage'])
        for width in (360, 768, 1280):
            page = browser.new_page(viewport={'width': width, 'height': 900})
            dialogs = []
            page.on('dialog', lambda d: (dialogs.append(d.message), d.dismiss()))
            page.route('**/api/**', api)
            # Projects no longer holds the panel; each project links to its information.
            page.goto(base + '/dash/projects', wait_until='domcontentloaded')
            link = page.locator('.project-information-link').filter(has_text='Knowledge').first
            link.wait_for()
            assert page.locator('.project-information').count() == 0, 'the panel moved to Databases'
            assert link.get_attribute('href') == '/dash/knowledge?project=alpha'
            assert page.locator('a[href="/dash/knowledge"]').count() >= 1, 'Databases is in the navigation'
            link.click()
            page.wait_for_url('**/dash/knowledge?project=alpha')
            page.get_by_role('heading', name='Knowledge', exact=True).first.wait_for()
            store = page.get_by_label('Information store')
            page.get_by_role('heading', name='Ground truths (1)').wait_for()
            assert store.input_value() == 'project:alpha', 'the link opened the project focused'
            # The deep link survives a reload.
            page.reload(wait_until='domcontentloaded')
            page.get_by_role('heading', name='Ground truths (1)').wait_for()
            assert page.get_by_label('Information store').input_value() == 'project:alpha'
            rule = page.locator('[data-info-entry="alpha-rule"]')
            assert 'authority: owner' in rule.inner_text() and 'source: owner mail' in rule.inner_text()
            assert 'stale: re-check before trusting' in page.locator('[data-info-entry="alpha-fact"]').inner_text()
            assert 'superseded by alpha-fact' in page.locator('[data-info-entry="alpha-old"]').inner_text()
            assert '1 linked file(s) skipped' in page.content()
            # Hostile text stays text: no image element, no script ran.
            rule.get_by_text('Show text').click()
            assert 'only text' in rule.locator('.markdown').inner_text()
            assert page.locator('.project-information img').count() == 0 and not dialogs
            page.get_by_label('Search information').fill('numbers')
            page.wait_for_function('document.querySelectorAll("[data-info-entry]").length === 1', timeout=5000)
            page.get_by_label('Search information').fill('nothing-like-this')
            page.get_by_text('No entry matches the search.').wait_for()
            page.get_by_label('Search information').fill('')
            store.select_option('project:broken')
            page.get_by_text('The mapping for this project is invalid: the directory is not an absolute path.').wait_for()
            store.select_option('global')
            page.get_by_text('This store has no entries yet.').wait_for()
            assert page.url.endswith('/dash/knowledge?project=global'), page.url
            assert not page.evaluate('document.documentElement.scrollWidth > window.innerWidth'), f'page scrolls sideways at {width}'
            page.close()
        browser.close()
    info = [(m, p) for m, p in requests if p.startswith('/api/project-context')]
    assert info and all(method == 'GET' for method, _ in info), 'the panel only reads'
finally:
    server.shutdown()
print('databases tab (project information) browser check passed at 360, 768 and 1280')
