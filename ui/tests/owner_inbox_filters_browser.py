"""Three Inbox views using only synthetic recorded lineage; all API traffic mocked."""
import json, os, threading
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit
from playwright.sync_api import sync_playwright

DIST = Path(__file__).resolve().parents[1] / 'dist'
class Handler(SimpleHTTPRequestHandler):
    def translate_path(self, path):
        return str(DIST / ('index.html' if path.startswith('/dash/') else path.lstrip('/')))
    def log_message(self, *args): pass
server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = f'http://127.0.0.1:{server.server_port}'
calls, pending = [], []
hold_human = False
unsafe = None
unavailable = False

def item(mid, body, source, audience=None):
    historical = source == 'historical'
    return {'id':mid,'message_id':f'i{mid}','body':body,'sender':'recorded sender','role':'agent',
        'created_at':'2026-11-13T00:00:00Z','conversation_source':source,'audience':audience,
        'mailbox':'owner' if source == 'human' else 'codex-rust-owner',
        'thread_version':None if historical else 1,'parent_id':None,'root_id':None if historical else f'i{mid}',
        'conversation_kind':None if historical else source,'sender_kind':None if historical else 'agent'}
HUMAN = item(201,'HUMAN-CONVERSATION: agent-only words do not classify','human')
HUMAN_OLD = item(101,'HUMAN-OLDER','human')
VP = item(401,'VP-UNTAGGED-COORDINATION','vp')
TAGGED = item(402,'VP-TAGGED-COORDINATION','vp','agent')
VP_OLD = item(301,'VP-OLDER','vp')
HISTORY = item(901,'PRIOR-VERSION-UNTAGGED','historical')
HISTORY_TAGGED = item(902,'PRIOR-VERSION-TAGGED','historical','agent')

def fulfill(route, value, status=200):
    route.fulfill(status=status,content_type='application/json',body=json.dumps(value))
def response(view,before):
    rows = ([HUMAN_OLD] if before else [HUMAN]) if view == 'human_threads' else ([VP_OLD] if before else [TAGGED,VP]) if view == 'vp_threads' else [HISTORY_TAGGED,HISTORY]
    cursor = None if before or view == 'historical' else 'i201' if view == 'human_threads' else 'i401'
    return {'items':rows,'next_before':cursor,'source_filter':view,'include_internal':False}
def mock(route):
    path=urlsplit(route.request.url).path;query=parse_qs(urlsplit(route.request.url).query)
    calls.append((route.request.method,path,query))
    if path != '/api/owner-thread': return fulfill(route,{'ok':True,'channels':[]})
    assert query.get('paged') == ['true'],query
    assert 'include_internal' not in query,query
    view=query['filter'][0];before=query.get('before',[None])[0]
    assert view in ('human_threads','vp_threads','historical'),query
    if before: assert before == ('i201' if view == 'human_threads' else 'i401'),query
    if unavailable: return fulfill(route,{'detail':'View unavailable'},502)
    if hold_human and view == 'human_threads': pending.append(route);return
    data=response(view,before)
    if unsafe == 'missing_scope': data={'items':[HUMAN],'next_before':None}
    elif unsafe == 'wrong_view': data['items']=[{**TAGGED,'body':'DO-NOT-DISPLAY'}]
    elif unsafe == 'no_lineage': data['items']=[{**HUMAN,'thread_version':None,'body':'DO-NOT-DISPLAY'}]
    return fulfill(route,data)
try:
    with sync_playwright() as playwright:
        browser=playwright.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'],headless=True,args=['--no-sandbox','--disable-dev-shm-usage'])
        for width in (360,768,1280):
            page=browser.new_page(viewport={'width':width,'height':900});errors=[]
            page.on('pageerror',lambda error:errors.append(str(error)))
            page.route('**/api/**',mock)
            page.goto(base+'/dash/cos',wait_until='domcontentloaded')
            page.get_by_role('heading',name='Your inbox',exact=True).wait_for()
            page.get_by_text(HUMAN['body'],exact=True).first.wait_for()
            nav=page.get_by_role('navigation',name='Inbox views')
            assert nav.get_by_role('button').all_text_contents() == ['Your inbox','VP','Historical / prior versions']
            assert page.get_by_role('combobox',name='Inbox filter').count() == 0
            assert page.get_by_role('checkbox',name='Include agent-only mail').count() == 0
            page.get_by_role('button',name='Load older messages',exact=True).click()
            page.get_by_text(HUMAN_OLD['body'],exact=True).first.wait_for()
            nav.get_by_role('button',name='VP',exact=True).click()
            page.get_by_role('heading',name='VP',exact=True).wait_for()
            page.get_by_text(VP['body'],exact=True).first.wait_for()
            page.get_by_text(TAGGED['body'],exact=True).first.wait_for()
            assert page.get_by_text(HUMAN_OLD['body'],exact=True).count() == 0
            assert page.evaluate('document.activeElement.textContent') == 'VP'
            page.get_by_role('button',name='Load older messages',exact=True).click()
            page.get_by_text(VP_OLD['body'],exact=True).first.wait_for()
            nav.get_by_role('button',name='Historical / prior versions',exact=True).click()
            page.get_by_role('heading',name='Historical / prior versions',exact=True).wait_for()
            page.get_by_text(HISTORY['body'],exact=True).first.wait_for()
            page.get_by_text(HISTORY_TAGGED['body'],exact=True).first.wait_for()
            assert page.get_by_text(TAGGED['body'],exact=True).count() == 0
            assert page.evaluate('document.documentElement.scrollWidth <= window.innerWidth')
            if os.environ.get('OWNER_FILTER_SCREENSHOT_DIR'):
                page.screenshot(path=str(Path(os.environ['OWNER_FILTER_SCREENSHOT_DIR'])/f'inbox-views-{width}.png'),full_page=True)
            # A held human page must not populate VP after the view is switched.
            hold_human=True
            nav.get_by_role('button',name='Your inbox',exact=True).click()
            page.wait_for_timeout(250)
            assert pending
            nav.get_by_role('button',name='VP',exact=True).click()
            page.get_by_text(VP['body'],exact=True).first.wait_for()
            for route in pending: fulfill(route,response('human_threads',None))
            pending.clear();hold_human=False
            page.wait_for_timeout(100)
            assert page.get_by_text(HUMAN['body'],exact=True).count() == 0
            assert not errors,errors
            print(json.dumps({'width':width,'three_views':'PASS','late_response':'PASS','page_errors':errors}),flush=True)
            page.close()
        for mode in ('missing_scope','wrong_view','no_lineage'):
            unsafe=mode;page=browser.new_page();page.route('**/api/**',mock)
            page.goto(base+'/dash/cos',wait_until='domcontentloaded')
            page.get_by_role('alert').wait_for()
            assert page.get_by_text('DO-NOT-DISPLAY',exact=True).count() == 0
            assert page.locator('.owner-inbox-row').count() == 0
            page.close()
        unsafe=None;unavailable=True
        page=browser.new_page();page.route('**/api/**',mock);page.goto(base+'/dash/cos',wait_until='domcontentloaded');page.get_by_role('alert').wait_for()
        assert page.locator('.owner-inbox-row').count() == 0
        page.close();browser.close()
        assert all(method == 'GET' for method,path,_ in calls if path != '/api/viewport'),calls
finally: server.shutdown()
print('three Inbox views passed: recorded lineage, historical preservation, scoped cursors, late response, no unsafe fallback, GET-only')
