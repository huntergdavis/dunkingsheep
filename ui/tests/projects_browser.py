"""Exercise the Projects dashboard with synthetic daemon responses only."""
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
lanes = [
    {'pane_id':'%1','name':'Writer A','membership':{'scope':'project','project':'alpha'},'state':'working','task':{'id':'t1','title':'Build'},'model':{'value':'model-a','effort':'medium','source':'launch flag','observed':False,'age_s':10805,'reason':None}},
    {'pane_id':'%2','name':'Reviewer B','membership':{'scope':'project','project':'beta'},'state':None,'task':None,'model':{'value':'model-old','effort':'high','source':'process environment','observed':False,'age_s':901,'reason':None}},
    {'pane_id':'%3','name':'Global lane','membership':{'scope':'global','project':None},'state':None,'task':None,'model':{'value':None,'age_s':None,'reason':'no agent-state sample for this lane in the last 24 hours'}},
    {'pane_id':'%4','name':'Stale lane','membership':{'scope':'stale','project':None},'state':None,'task':None,'model':{'value':'model-seen','effort':None,'source':'pane footer','observed':True,'age_s':185,'reason':None}},
    {'pane_id':'%5','name':'Unknown lane','membership':{'scope':'unknown','project':None},'state':None,'task':None,'model':None},
    {'pane_id':'%6','name':'Literal global','membership':{'scope':'project','project':'Global'},'state':None,'task':None,'model':None},
]
original_lanes = list(lanes)
def mock(route):
    path = urlsplit(route.request.url).path
    value = {'lanes':lanes, 'as_of':'2026-01-01T00:00:00Z'} if path == '/api/projects' else {'ok':True,'channels':[]}
    route.fulfill(status=200, content_type='application/json', body=json.dumps(value))
try:
    with sync_playwright() as playwright:
        browser=playwright.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'],headless=True,args=['--no-sandbox','--disable-dev-shm-usage'])
        for width in (360,768,1280):
            lanes[:] = original_lanes
            page=browser.new_page(viewport={'width':width,'height':900})
            page.route('**/api/**',mock)
            page.goto(base+'/dash/projects',wait_until='domcontentloaded')
            page.get_by_role('heading',name='Project: alpha').wait_for()
            assert page.locator('.project-group').count()==6
            assert page.get_by_text('Writer A').count()==1
            fresh=page.locator('[data-lane-model="%1"]').inner_text()
            assert 'launched as model-a medium' in fresh and 'launch flag, launched 3 h ago' in fresh and 'seen' not in fresh, fresh
            stale=page.locator('[data-lane-model="%2"]').inner_text()
            assert stale.startswith('unknown') and 'older than 15 minutes' in stale and 'model-old' not in stale, stale
            missing=page.locator('[data-lane-model="%3"]').inner_text()
            assert missing.startswith('unknown') and 'no agent-state sample' in missing, missing
            seen=page.locator('[data-lane-model="%4"]').inner_text()
            assert 'model-seen' in seen and 'seen 3 min ago, pane footer' in seen and 'launched' not in seen, seen
            assert page.evaluate('document.documentElement.scrollWidth <= window.innerWidth'), f'page scrolls sideways at {width}'
            assert page.get_by_text('Unknown / unregistered').count()>=1
            assert page.get_by_role('heading',name='Project: Global').count()==1
            assert page.get_by_role('heading',name='Global scope').count()==1
            page.get_by_label('Filter projects').select_option('project:beta')
            assert page.locator('.project-group').count()==1
            assert page.get_by_text('Reviewer B').count()==1
            assert page.get_by_text('Writer A').count()==0
            assert page.get_by_role('link',name='Reviewer B').get_attribute('href')=='/dash/projects?pane=%252'
            lanes[:] = [lane for lane in lanes if lane['pane_id'] != '%2']
            page.get_by_text('Writer A').wait_for(timeout=15000)
            assert page.get_by_label('Filter projects').input_value() == 'all'
            page.close()
        browser.close()
finally: server.shutdown()
print('projects browser regression passed at 360, 768 and 1280')
