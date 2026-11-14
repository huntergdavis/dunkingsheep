"""The web terminal follows the pane's size and fits its box (t5007). Synthetic stream only:
never a real daemon, tmux, lane or credential."""
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
lanes = [{'pane_id':'%1','name':'Worker','membership':{'scope':'project','project':'alpha'}}]
INTERNAL = ['Live output begins', 'existing lane view', 'unconfirmed', 'Reconcile', 'refused', 'Another tab']

def size(cols, rows): return json.dumps({'type':'size','cols':cols,'rows':rows})
def screen(cols, rows):
    # Every row is exactly as wide as the pane: with any other terminal width it would wrap.
    lines = [(f'ROW{index:03d}' + '=' * cols)[:cols] for index in range(rows)]
    return ('\x1b[?1049l\x1b[0m\x1b[H\x1b[2J' + '\r\n'.join(lines) + '\x1b[0m\x1b[1;1H').encode()

def run(browser, width, cols, rows, expect_fit):
    resizes, sockets = [], []
    def api(route):
        path = urlsplit(route.request.url).path
        if path == '/api/projects': value, status = {'lanes':lanes}, 200
        elif path == '/api/terminals': value, status = [{'pane_id':'%1','name':'worker','cols':cols,'rows':rows}], 200
        elif path.endswith('/resize'):
            resizes.append(json.loads(route.request.post_data))
            # The pane is shared with another client: the server refuses, as it does for real lanes.
            value, status = {'detail':'resize refused: target or client is not isolated'}, 409
        else: value, status = {'ok':True,'channels':[]}, 200
        route.fulfill(status=status, content_type='application/json', body=json.dumps(value))
    def websocket(ws):
        sockets.append(ws)
        ws.send(size(cols, rows)); ws.send(screen(cols, rows))
    page = browser.new_page(viewport={'width':width,'height':900})
    page.route('**/api/**', api)
    page.route_web_socket('**/api/terminals/*/stream**', websocket)
    page.goto(base + '/dash/projects', wait_until='domcontentloaded')
    page.get_by_role('button',name='Open terminal').click()
    page.get_by_text('Connected',exact=True).wait_for()
    page.get_by_text('ROW000',exact=False).first.wait_for()
    def measure():
        return page.evaluate('''() => {
            const box = document.querySelector('.project-terminal-screen'), view = document.querySelector('.xterm-screen');
            const style = getComputedStyle(box);
            const inner = box.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight);
            const innerHeight = box.clientHeight - parseFloat(style.paddingTop) - parseFloat(style.paddingBottom);
            const rows = [...document.querySelectorAll('.xterm-rows > div')];
            return {inner, innerHeight, screenWidth: view.clientWidth, screenHeight: view.clientHeight, rows: rows.length,
                first: rows[0].textContent, second: rows[1].textContent,
                pageScrolls: document.documentElement.scrollWidth > window.innerWidth,
                boxScrolls: box.scrollWidth > box.clientWidth + 1, text: document.querySelector('.project-terminal').innerText};
        }''')
    def check(label, cols, rows, expect_fit):
        page.wait_for_function(f'document.querySelectorAll(".xterm-rows > div").length === {rows}', timeout=10000)
        page.wait_for_timeout(700)
        m = measure()
        where = f'{label} at {width}px, pane {cols}x{rows}: {m}'
        # The terminal has the pane's rows and columns: full-width rows are not wrapped.
        assert m['rows'] == rows, where
        assert m['first'].startswith('ROW000') and len(m['first'].rstrip()) == cols, where
        assert m['second'].startswith('ROW001'), where
        assert not m['pageScrolls'], where
        fits = m['screenWidth'] <= m['inner'] + 1 and m['screenHeight'] <= m['innerHeight'] + 1
        if expect_fit is True: assert fits, where
        if expect_fit is False: assert not fits, where
        if fits:
            assert not m['boxScrolls'], where
        else:
            # Too wide or tall for the smallest readable font (8 px, a cell about 4.8 px wide):
            # the box scrolls, the page does not, and the font is not shrunk further.
            assert m['boxScrolls'], where
            assert 4.5 <= m['screenWidth'] / cols <= 5.3, where
        for phrase in INTERNAL: assert phrase not in m['text'], (phrase, where)
        return m
    check('open', cols, rows, expect_fit)
    # A window resize: the terminal keeps the pane's size and fits the new box.
    page.set_viewport_size({'width': max(360, width - 200), 'height': 700})
    check('after shrinking the window', cols, rows, None)
    page.set_viewport_size({'width': width, 'height': 900})
    check('after restoring the window', cols, rows, expect_fit)
    # Another client resized the pane: the server sends the new size and screen, the terminal follows.
    sockets[-1].send(size(100, 30)); sockets[-1].send(screen(100, 30))
    check('after the pane changed size', 100, 30, None)
    page.wait_for_timeout(2500)
    stable = [measure() for _ in range(2)]
    assert stable[0]['screenWidth'] == stable[1]['screenWidth'] and stable[0]['rows'] == 30, stable
    # A refused resize is quiet, and a refusal is not retried in a loop.
    # It asks once the screen has settled and once per change of box or pane, never in a loop.
    assert 1 <= len(resizes) <= 6, resizes
    assert len({(r['cols'], r['rows']) for r in resizes[-2:]}) == len(resizes[-2:]), resizes
    assert 'Terminal size could not be changed' not in stable[1]['text'], stable[1]['text']
    for request in resizes: assert request['expected_pane'] == '%1' and request['cols'] >= 20, request
    page.close()
    return len(resizes)

try:
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'], headless=True, args=['--no-sandbox','--disable-dev-shm-usage'])
        counts = {}
        for width in (390, 768, 1280):
            counts[width] = run(browser, width, 80, 24, expect_fit=True if width >= 768 else None)
        # A wide pane on a phone: readable font, the box scrolls sideways, the page does not.
        counts['phone-wide'] = run(browser, 390, 200, 50, expect_fit=False)
        browser.close()
finally:
    server.shutdown()
print('terminal fit browser check passed at 390, 768 and 1280 with window and pane resizes; resize requests:', counts)
