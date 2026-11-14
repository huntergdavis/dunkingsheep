"""Synthetic mailbox browsing only: never connect to the live desk or daemon.
Run after npm run build with CHROMIUM_PATH and optional MAILBOX_SCREENSHOT_DIR.
"""
import json
import os
import threading
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit
from playwright.sync_api import sync_playwright

DIST = Path(__file__).resolve().parents[1] / 'dist'
assert (DIST / 'index.html').is_file()
LANE = 'lane-alpha & beta (%2)'
RAW = '''AGENT-ONLY-SECRET

| Name | Value |
| --- | --- |
| Alpha | 42 |

+-----+
| A B |
+-----+

<script>window.__mailAttack=1</script><img src=x onerror="window.__mailAttack=2">
'''
requests = []
pending = []
hold_agent = False
fail_summary = False
old_daemon = False

def summary(mid, mailbox, subject, audience=None):
    return {'message_id': mid, 'mailbox': mailbox, 'sender': 'synthetic-lane', 'subject': subject,
            'created_at': 100, 'state': 'read', 'audience': audience}
OWNER = summary('i101', 'owner', 'Owner newest')
OLDER = summary('i99', 'owner', 'Owner older')
PLAIN = summary('i201', LANE, 'Lane plain')
AGENT = summary('i202', LANE, 'Agent secret', 'agent')
ALL = {row['message_id']: row for row in [OWNER, OLDER, PLAIN, AGENT]}
BODIES = {'i101': 'OWNER-BODY', 'i99': 'OLDER-BODY', 'i201': 'LANE-PLAIN-BODY', 'i202': RAW}

class Handler(SimpleHTTPRequestHandler):
    def translate_path(self, path):
        return str(DIST / ('index.html' if path.startswith('/dash/') else path.lstrip('/')))
    def log_message(self, *args):
        pass
server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = f'http://127.0.0.1:{server.server_port}'

def fulfill(route, value, status=200):
    route.fulfill(status=status, content_type='application/json', body=json.dumps(value))

def mock(route):
    url = urlsplit(route.request.url)
    query = parse_qs(url.query)
    requests.append((route.request.method, url.path, query))
    if url.path == '/api/channels':
        return fulfill(route, {'channels': []})
    if url.path == '/api/owner-thread':
        return fulfill(route, {'items': [], 'next_before': None})
    if url.path == '/api/mailboxes':
        more = bool(query.get('after'))
        rows = [{'mailbox': 'retired box', 'destination': 'Retired', 'unread': 0, 'total': 1,
                 'mapping': 'unmapped', 'lanes': []}] if more else [
            {'mailbox': 'owner', 'unread': 2, 'total': 2, 'mapping': 'unmapped', 'lanes': []},
            {'mailbox': LANE, 'unread': 1, 'total': 2, 'mapping': 'live',
             'lanes': [{'name': 'Alpha', 'membership': {'scope': 'project', 'project': 'Arbitrary project'}}]}]
        return fulfill(route, {'mailboxes': rows, 'total': 3, 'next_after': None if more else LANE})
    if url.path == '/api/mailbox-messages':
        if fail_summary:
            return fulfill(route, {'detail': 'Synthetic failure'}, 503)
        mailbox = query['mailbox'][0]
        rows = [OLDER] if mailbox == 'owner' and query.get('before') else [OWNER] if mailbox == 'owner' else [AGENT, PLAIN] if mailbox == LANE else []
        return fulfill(route, {'messages': rows, 'next_before': 'i101' if rows == [OWNER] else None})
    if url.path == '/api/mailbox-message':
        if old_daemon:
            return fulfill(route, {'detail': 'Message unavailable'}, 502)
        mid = query['message_id'][0]
        assert query['mailbox'][0] == ALL[mid]['mailbox'], query
        if mid == 'i202':
            assert query.get('include_agent') == ['true'], 'agent body requested without opt-in'
            if hold_agent:
                pending.append(route)
                return
        return fulfill(route, {'message': {**ALL[mid], 'body': BODIES[mid], 'attachments': []}})
    return fulfill(route, {'ok': True})

try:
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'], headless=True,
                                             args=['--no-sandbox', '--disable-dev-shm-usage'])
        for width in (360, 768, 1280):
            page = browser.new_page(viewport={'width': width, 'height': 900})
            page.route('**/api/**', mock)
            page.goto(base + '/dash/cos?panel=mailboxes', wait_until='domcontentloaded')
            reader = page.get_by_role('article', name='Selected mailbox message')
            reader.get_by_text('OWNER-BODY', exact=True).wait_for()
            page.get_by_role('button', name='Older', exact=True).click()
            reader.get_by_text('OLDER-BODY', exact=True).wait_for()
            page.get_by_role('button', name='Newer', exact=True).click()
            reader.get_by_text('OWNER-BODY', exact=True).wait_for()
            page.locator('.mailbox-folder').filter(has_text=LANE).click()
            reader.get_by_text('LANE-PLAIN-BODY', exact=True).wait_for()
            assert page.get_by_role('heading', name='Project: Arbitrary project').count() == 1
            details = page.locator('.mailbox-summaries details')
            assert details.get_attribute('open') is None
            assert page.locator('.mailbox-message-row').filter(has_text='Agent secret').is_disabled()
            assert all(query.get('message_id') != ['i202'] for _, path, query in requests if path == '/api/mailbox-message')
            optin = page.get_by_role('checkbox', name='Include agent-only mail in this browser')
            optin.check()
            page.locator('.mailbox-message-row').filter(has_text='Agent secret').click()
            reader.get_by_text('AGENT-ONLY-SECRET', exact=True).wait_for()
            assert reader.locator('table').count() == 1
            assert reader.locator('.ascii-diagram').count() == 1
            assert reader.locator('script, img, iframe').count() == 0
            assert page.evaluate('window.__mailAttack || 0') == 0
            optin.uncheck()
            assert reader.get_by_text('AGENT-ONLY-SECRET', exact=True).count() == 0
            reader.get_by_text('LANE-PLAIN-BODY', exact=True).wait_for()
            # A late response from an explicitly opted-in read cannot survive opting out.
            hold_agent = True
            optin.check()
            page.locator('.mailbox-message-row').filter(has_text='Agent secret').click()
            page.wait_for_function("document.querySelector('.mailbox-reader').textContent.includes('Loading message')")
            optin.uncheck()
            assert pending
            for held in pending:
                fulfill(held, {'message': {**AGENT, 'body': RAW, 'attachments': []}})
            pending.clear()
            hold_agent = False
            reader.get_by_text('LANE-PLAIN-BODY', exact=True).wait_for()
            assert reader.get_by_text('AGENT-ONLY-SECRET', exact=True).count() == 0
            page.get_by_role('button', name='Load more mailboxes', exact=True).click()
            page.get_by_role('heading', name='Unmapped / history').wait_for()
            page.locator('.mailbox-folder').filter(has_text='retired box').click()
            page.get_by_text('No messages in this mailbox.', exact=True).wait_for()
            assert reader.get_by_text('LANE-PLAIN-BODY', exact=True).count() == 0
            page.locator('.mailbox-folder').filter(has_text=LANE).click()
            reader.get_by_text('LANE-PLAIN-BODY', exact=True).wait_for()
            fail_summary = True
            page.get_by_role('button', name='Refresh', exact=True).click()
            page.get_by_role('alert').filter(has_text='Messages unavailable').wait_for()
            fail_summary = False
            page.get_by_role('button', name='Refresh', exact=True).click()
            reader.get_by_text('LANE-PLAIN-BODY', exact=True).wait_for()
            old_daemon = True
            page.get_by_role('button', name='Refresh', exact=True).click()
            reader.get_by_text('Mailbox browsing is temporarily unavailable.', exact=False).wait_for()
            assert reader.get_by_text('LANE-PLAIN-BODY', exact=True).count() == 0
            old_daemon = False
            page.get_by_role('button', name='Refresh', exact=True).click()
            reader.get_by_text('LANE-PLAIN-BODY', exact=True).wait_for()
            assert page.evaluate('document.documentElement.scrollWidth <= window.innerWidth')
            if os.environ.get('MAILBOX_SCREENSHOT_DIR'):
                page.screenshot(path=str(Path(os.environ['MAILBOX_SCREENSHOT_DIR']) / f's-d-mailboxes-{width}.png'))
            page.get_by_role('link', name='Owner conversation', exact=True).click()
            page.get_by_role('link', name='Back to Inbox', exact=True).wait_for()
            assert urlsplit(page.url).query == 'panel=conversation'
            page.get_by_role('link', name='Back to Inbox', exact=True).click()
            page.get_by_role('heading', name='Owner inbox', exact=True).wait_for()
            page.get_by_role('link', name='Browse all mailboxes', exact=True).click()
            page.get_by_role('heading', name='Mailboxes', exact=True).wait_for()
            assert not page.get_by_role('checkbox').is_checked()
            page.close()
            assert all(method == 'GET' for method, path, _ in requests if path != '/api/viewport')
            assert {path for _, path, _ in requests} <= {'/api/channels','/api/owner-thread','/api/mailboxes','/api/mailbox-messages','/api/mailbox-message','/api/version','/api/viewport'}
            requests.clear()
        browser.close()
finally:
    server.shutdown()
print('mailbox browser passed at360/768/1280: directory/pages/body, agent opt-in/late response, sanitizer, empty/error/upgrade fallback, GET-only')
