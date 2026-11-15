"""Missing route/terminal chunks show reload recovery; no daemon or tmux used."""
import json
import os
import re
import threading
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit

from playwright.sync_api import sync_playwright

DIST = Path(__file__).resolve().parents[1] / 'dist'


class Handler(SimpleHTTPRequestHandler):
    def translate_path(self, path):
        path = urlsplit(path).path
        return str(DIST / ('index.html' if path.startswith('/dash/') else path.lstrip('/')))

    def log_message(self, *args):
        pass


def api(route):
    path = urlsplit(route.request.url).path
    if path == '/api/projects':
        data = {'lanes': [{'pane_id': '%21', 'name': 'Worker',
                          'membership': {'scope': 'project', 'project': 'alpha'}}]}
    elif path == '/api/terminals':
        data = [{'pane_id': '%21', 'name': 'Worker', 'cols': 80, 'rows': 24}]
    else:
        data = {'ok': True}
    route.fulfill(status=200, content_type='application/json', body=json.dumps(data))


server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = f'http://127.0.0.1:{server.server_port}'
try:
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'],
            headless=True, args=['--no-sandbox', '--disable-dev-shm-usage'])
        for chunk in ('ProjectsView', 'ProjectTerminal'):
            page = browser.new_page()
            page.route('**/api/**', api)
            pattern = re.compile(r'/assets/' + chunk + r'-[^/]+\.js$')
            failures = []

            def missing(route):
                failures.append(route.request.url)
                route.fulfill(status=404, content_type='text/plain', body='Not found')

            page.route(pattern, missing)
            page.goto(base + '/dash/projects', wait_until='domcontentloaded')
            if chunk == 'ProjectTerminal':
                page.get_by_role('heading', name='Project: alpha').wait_for()
                page.get_by_role('button', name='Open terminal').click()
            alert = page.get_by_role('alert')
            alert.get_by_text('This page was updated. Reload to continue.', exact=True).wait_for()
            assert len(failures) == 1, failures
            if chunk == 'ProjectTerminal':
                assert page.get_by_role('heading', name='Project: alpha').is_visible()
                assert page.get_by_role('link', name='Worker', exact=True).is_visible()
            # Restore the chunk; clicking the fallback must reload the same deep link
            # and recover the route (and terminal, when opened again).
            page.unroute(pattern, missing)
            with page.expect_navigation(wait_until='domcontentloaded'):
                alert.get_by_role('button', name='Reload page').click()
            page.get_by_role('heading', name='Project: alpha').wait_for()
            assert urlsplit(page.url).path == '/dash/projects'
            assert page.get_by_role('alert').count() == 0
            if chunk == 'ProjectTerminal':
                page.route_web_socket('**/api/terminals/*/stream**', lambda ws: ws.send(b'RECOVERED\r\n'))
                page.get_by_role('button', name='Open terminal').click()
                page.get_by_text('Connected', exact=True).wait_for()
                page.get_by_text('RECOVERED', exact=False).first.wait_for()
            page.close()
        browser.close()
finally:
    server.shutdown()
print('missing route and terminal chunks recover through reload')
