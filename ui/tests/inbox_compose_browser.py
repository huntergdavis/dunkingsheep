"""Synthetic compose/delivery boundary checks; never contact the live desk/daemon."""
import json
import os
import threading
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit
from playwright.sync_api import sync_playwright

DIST = Path(__file__).resolve().parents[1] / 'dist'
requests = []
posts = []
pending = []
hold = False
fail = False
missing_receipt = False
lane_present = True

class Handler(SimpleHTTPRequestHandler):
    def translate_path(self, path):
        return str(DIST / ('index.html' if path.startswith('/dash/') else path.lstrip('/')))
    def log_message(self, *args):
        pass

server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = f'http://127.0.0.1:{server.server_port}'

def fulfill(route, data, status=200):
    route.fulfill(status=status, content_type='application/json', body=json.dumps(data))

def mock(route):
    path = urlsplit(route.request.url).path
    requests.append((route.request.method, path))
    if path in ('/api/owner-send', '/api/pane-inbox'):
        assert route.request.method == 'POST'
        posts.append((path, route.request.post_data_json))
        if hold:
            pending.append(route)
            return
        if fail:
            return fulfill(route, {'detail': 'Synthetic refused send'}, 409)
        return fulfill(route, {} if missing_receipt else {'outcome': 'stored', 'message_id': 'i900', 'mailbox': 'redirected-owner', 'notified': False, 'redirected': True})
    if path == '/api/fleet':
        return fulfill(route, {'rows': [{'pane_id': '%9', 'name': 'Test lane', 'project': 'Test project'}] if lane_present else []})
    if path == '/api/owner-thread':
        return fulfill(route, {'items': [], 'next_before': None, 'source_filter': 'all_sources', 'include_internal': False})
    if path == '/api/channels':
        return fulfill(route, {'channels': []})
    if path == '/api/mailboxes':
        return fulfill(route, {'mailboxes': [], 'total': 0, 'next_after': None})
    if path == '/api/mailbox-messages':
        return fulfill(route, {'messages': [], 'next_before': None, 'include_agent': False})
    if path == '/api/mailbox-message':
        raise AssertionError('Collapsed agent mail must not be read by compose')
    return fulfill(route, {'ok': True})

try:
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'], headless=True,
                                             args=['--no-sandbox', '--disable-dev-shm-usage'])
        for width, panel in ((360, ''), (768, '?panel=mailboxes'), (1280, '?panel=mailboxes')):
            page = browser.new_page(viewport={'width': width, 'height': 900})
            page.route('**/api/**', mock)
            page.goto(base + '/dash/cos' + panel, wait_until='domcontentloaded')
            assert not posts
            page.get_by_role('button', name='Compose message', exact=True).click()
            composer = page.get_by_role('region', name='Compose inbox message')
            recipient = composer.get_by_role('combobox', name='Message recipient')
            text = composer.get_by_role('textbox', name='Inbox message')
            send = composer.get_by_role('button', name='Send message', exact=True)
            recipient.locator('option[value="%9"]').wait_for(state='attached')
            assert recipient.input_value() == 'owner'
            assert send.is_disabled()
            text.fill('Owner question')
            hold = True
            send.click()
            composer.get_by_role('button', name='Sending…', exact=True).wait_for()
            text.press('Control+Enter')
            assert len(posts) == 1, posts
            assert posts[-1] == ('/api/owner-send', {'text': 'Owner question'})
            assert recipient.is_disabled()
            assert composer.get_by_role('button', name='Close composer').is_disabled()
            hold = False
            fulfill(pending.pop(), {'outcome': 'stored', 'message_id': 'i901'})
            composer.get_by_text('Stored in owner inbox', exact=False).wait_for()
            assert text.input_value() == ''
            recipient.select_option('%9')
            text.fill('Lane draft kept')
            fail = True
            send.click()
            composer.get_by_role('alert').filter(has_text='Synthetic refused send').wait_for()
            assert text.input_value() == 'Lane draft kept'
            assert posts[-1] == ('/api/pane-inbox', {'pane': '%9', 'text': 'Lane draft kept'})
            fail = False
            missing_receipt = True
            send.click()
            composer.get_by_role('alert').filter(has_text='No storage receipt').wait_for()
            assert text.input_value() == 'Lane draft kept'
            missing_receipt = False
            send.click()
            composer.get_by_text('Stored in redirected-owner inbox', exact=False).wait_for()
            assert 'lane not notified yet' in composer.inner_text()
            assert 'redirected by the daemon' in composer.inner_text()
            assert text.input_value() == ''
            text.fill('Stale lane draft')
            lane_present = False
            composer.get_by_role('button', name='Reload recipients').click()
            composer.get_by_text('No live lanes', exact=False).wait_for()
            assert send.is_disabled()
            assert text.input_value() == 'Stale lane draft'
            recipient.select_option('owner')
            assert send.is_enabled(), 'Owner must work when no lane is available'
            text.fill('x' * 16001)
            assert send.is_disabled()
            text.fill('Preserve on close')
            composer.get_by_role('button', name='Close composer').click()
            page.get_by_role('button', name='Compose message', exact=True).click()
            assert text.input_value() == 'Preserve on close'
            assert composer.get_by_role('link', name='Follow up in owner conversation').get_attribute('href') == '/dash/cos?panel=conversation'
            assert page.get_by_role('button', name='Reply', exact=True).count() == 0
            assert page.evaluate('document.documentElement.scrollWidth <= innerWidth')
            if os.environ.get('INBOX_COMPOSE_SCREENSHOT_DIR'):
                page.screenshot(path=f"{os.environ['INBOX_COMPOSE_SCREENSHOT_DIR']}/s-e-compose-{width}.png")
            page.close()
            posts.clear()
            lane_present = True
        writes = [(method, path) for method, path in requests if method != 'GET' and path != '/api/viewport']
        assert all(method == 'POST' and path in ('/api/owner-send', '/api/pane-inbox') for method, path in writes), writes
        assert len(writes) == 12, writes
        browser.close()
finally:
    server.shutdown()
print('Inbox compose passed: owner/lane routes, single flight, receipts, retained drafts, unavailable lane, length, agent boundary, 360/768/1280')
