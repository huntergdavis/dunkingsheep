"""Projects consolidation: scoped work/decisions, safe knowledge, no retired board requests."""
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
requests = []
def mock(route):
    url = urlsplit(route.request.url); path = url.path; query = parse_qs(url.query)
    requests.append((path,query))
    value = {'ok':True, 'channels':[]}
    if path == '/api/projects': value = {'lanes':[]}
    elif path == '/api/project-context': value = {'map':{'state':'ok'},'projects':[{'name':'alpha','state':'ok'},{'name':'beta','state':'ok'}]}
    elif path == '/api/project-resources': value = {'budget':{'providers':[]},'host':{'cpu_used_percent':25,'memory':{'used_percent':60},'cpu_pressure_avg60':1,'memory_pressure_avg60':0}}
    elif path == '/api/tasks':
        assert query.get('project') == ['alpha'], query
        value = {'scope':'global','view':'active','legacy_limited':False,'checked_at':100,'total':1,'next_cursor':None,'tasks':[{'id':'t42','title':'Approve alpha change','body':'**Evidence**: tested','kind':'review','status':'blocked','priority':'high','assignees':['%21'],'created_at':10,'block_reason':'needs-owner: Approve?'}]}
    elif path == '/api/tasks/question': value = {'question':'Approve this exact change?','version':'7','options':[{'number':1,'label':'Approve'}]}
    elif path == '/api/tasks/answer':
        assert route.request.post_data_json == {'task_id':'t42','version':'7','option_number':1}
        value = {'ok':True}
    elif path == '/api/project-context/project/alpha': value = {'state':'ok','total':1,'shown':1,'entries':[{'slug':'rule','subject':'Source rule','ground_truth':True,'authority':'owner','source':'owner decision','body':'# Safe heading\n\n**Readable** text <script>alert(1)</script>'}]}
    route.fulfill(status=200,content_type='application/json',body=json.dumps(value))
try:
    with sync_playwright() as p:
        browser = p.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'],headless=True,args=['--no-sandbox','--disable-dev-shm-usage'])
        for width in (360,768,1280):
            page=browser.new_page(viewport={'width':width,'height':900}); errors=[]
            page.on('pageerror',lambda e: errors.append(str(e)))
            page.route('**/api/**',mock)
            page.goto(base+'/dash/operations?project=alpha',wait_until='domcontentloaded')
            page.wait_for_url('**/dash/projects?project=alpha')
            page.get_by_role('heading',name='Project: alpha').wait_for()
            assert page.get_by_role('heading',name='Project: beta').count()==0
            assert page.get_by_role('link',name='Operations board').count()==0
            page.get_by_text('CPU 25.0%',exact=True).wait_for()
            page.get_by_role('button',name='Work & decisions').click()
            page.get_by_text('Approve alpha change').wait_for()
            assert page.get_by_text('Evidence',exact=True).count()==0
            page.get_by_text('Approve alpha change').click()
            page.get_by_text('Evidence',exact=True).wait_for()
            page.get_by_role('button',name='Read question & answer').click()
            page.get_by_role('button',name='Approve',exact=True).click()
            page.get_by_text('Decision recorded.',exact=False).wait_for()
            page.locator('.project-group:not(.vp-project)').get_by_role('button',name='Knowledge',exact=True).click()
            page.get_by_text('Show text',exact=True).click()
            page.get_by_role('heading',name='Safe heading').wait_for()
            assert page.locator('.project-information script').count()==0
            assert page.locator('.project-information strong').inner_text()=='Readable'
            assert page.evaluate('document.documentElement.scrollWidth <= innerWidth'), width
            assert not errors, errors
            page.close()
        assert not [path for path,_ in requests if path.startswith('/api/ops-board')], requests
        browser.close()
finally: server.shutdown()
print('Projects consolidation passed: scopes, decisions, knowledge, resources and responsive layout')
