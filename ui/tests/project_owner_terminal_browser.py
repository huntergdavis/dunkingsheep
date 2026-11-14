"""Synthetic VP terminal and Inbox label check. All API and websocket traffic is mocked."""
import json, os, threading
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
received = []
requests = []
streams = []
def api(route):
    path = urlsplit(route.request.url).path
    requests.append((route.request.method, path))
    if path == '/api/projects':
        value = {'lanes':[{'pane_id':'%1','name':'owner','membership':{'scope':'global'}}, {'pane_id':'%3','name':'Worker','membership':{'scope':'project','project':'alpha'}}]}
    elif path == '/api/terminals': value = [{'pane_id':'%1','name':'__owner_terminal__','agent':'owner','cols':80,'rows':24}]
    elif path == '/api/fleet': value = {'state':'ok','rows':[{'pane_id':'%1','name':'owner','project':None}], 'unrouted':{'state':'ok','rows':[]}}
    elif path == '/api/owner-thread': value = {'items':[], 'has_more':False}
    elif path == '/api/channels': value = {'channels':[{'id':'cos','name':'Owner','pane_id':'%1','can_message':True,'status':'ready'}]}
    elif path.endswith('/resize'): value = {'cols':80,'rows':24}
    else: value = {'ok':True,'channels':[]}
    route.fulfill(status=200, content_type='application/json', body=json.dumps(value))
def websocket(ws):
    streams.append(ws.url)
    ws.send(b'VP-LIVE\r\n')
    ws.on_message(lambda data: received.append(data))
try:
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'], headless=True, args=['--no-sandbox','--disable-dev-shm-usage'])
        for width in (360,768,1280):
            errors = []
            page = browser.new_page(viewport={'width':width,'height':900})
            page.on('pageerror', lambda error: errors.append(str(error)))
            page.on('console', lambda msg: errors.append(msg.text) if msg.type == 'error' else None)
            page.route('**/api/**', api)
            page.route_web_socket('**/api/terminals/*/stream**', websocket)
            page.goto(base + '/dash/projects', wait_until='domcontentloaded')
            page.get_by_role('heading', name='VP of Agentry', exact=True).wait_for()
            page.get_by_role('link', name='VP of Agentry', exact=True).wait_for()
            assert page.get_by_role('link', name='VP of Agentry').get_attribute('href') == '/dash/projects?pane=%251'
            assert page.get_by_role('button', name='Open VP terminal', exact=True).count() == 2
            assert 'owner terminal' not in page.locator('body').inner_text().lower()
            page.get_by_role('button', name='Open VP terminal', exact=True).first.click()
            page.get_by_text('Connected', exact=True).wait_for()
            page.get_by_text('VP-LIVE', exact=False).first.wait_for()
            assert page.get_by_role('region', name='Terminal VP of Agentry').count() == 1
            page.locator('.xterm-helper-textarea').focus()
            page.keyboard.type('x')
            page.wait_for_timeout(500)
            assert any(data == b'x' or data == 'x' for data in received), received
            assert '__owner_terminal__/stream' in streams[-1] and 'expected_pane=%251' in streams[-1], streams
            assert page.evaluate('document.documentElement.scrollWidth <= window.innerWidth')
            if os.environ.get('VP_LABEL_SCREENSHOT_DIR'):
                page.screenshot(path=str(Path(os.environ['VP_LABEL_SCREENSHOT_DIR']) / f'vp-terminal-{width}.png'), full_page=True)
            page.get_by_role('button', name='Close terminal').click()
            assert page.locator('.project-terminal').count() == 0
            page.goto(base + '/dash/cos', wait_until='domcontentloaded')
            page.get_by_role('heading', name='Inbox', exact=True).wait_for()
            selector=page.get_by_label('Inbox filter', exact=True)
            assert selector.locator('option[value="human"]').inner_text() == 'Your inbox'
            assert selector.locator('option[value="owner_lane"]').inner_text() == 'VP'
            page.get_by_role('button', name='Compose message', exact=True).click()
            recipients=page.get_by_label('Message recipient', exact=True)
            assert recipients.locator('option[value="owner"]').inner_text() == 'VP conversation'
            page.get_by_text('Unassigned · VP of Agentry · %1', exact=True).wait_for(state='attached')
            assert recipients.locator('option[value="%1"]').inner_text() == 'Unassigned · VP of Agentry · %1'
            assert page.get_by_role('link', name='Follow up with VP of Agentry').get_attribute('href') == '/dash/cos?panel=conversation'
            assert not errors, errors
            print(json.dumps({'width':width,'labels':'PASS','terminal_identifier':'__owner_terminal__','pane':'%1','console_errors':errors}), flush=True)
            page.close()
        assert all(path.endswith('/resize') for method,path in requests if method != 'GET'), requests
        browser.close()
finally:
    server.shutdown()
print('synthetic VP labels/terminal/Inbox workflow passed at 360/768/1280')
