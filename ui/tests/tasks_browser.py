"""Read-only Tasks matrix with >200 private tasks; no live daemon, cookies or terminals."""
import json, os, re, threading
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit
from playwright.sync_api import expect, sync_playwright

DIST = Path(__file__).resolve().parents[1] / 'dist'
class Handler(SimpleHTTPRequestHandler):
    def translate_path(self, path):
        return str(DIST / ('index.html' if path.startswith('/dash/') else path.lstrip('/')))
    def log_message(self, *args): pass
server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = f'http://127.0.0.1:{server.server_port}'
now = 1794700000
closed = [dict(id=f't{1000+i}', title=f'CLOSED-FIXTURE-{i} '+('long private fixture label '*9 if i == 0 else ''), status='done' if i%2==0 else 'cancelled', priority='high' if i%2==0 else 'low', assignees=['%4' if i%3==0 else '%8'], created_at=now-90000-i, block_reason=None) for i in range(235)]
active = [dict(id=f't{2000+i}', title=f'ACTIVE-FIXTURE-{i}', status=['open','in_progress','blocked'][i%3], priority='high', assignees=['%4'], created_at=now-3600, block_reason='Waiting for private fixture approval' if i%3==2 else None) for i in range(17)]
legacy = False
hold_active = False
metrics_failure = False
failure = False
unsafe = False
pending, calls = [], []
def fulfill(route, value, status=200):
    route.fulfill(status=status, content_type='application/json', body=json.dumps(value))
def mock(route):
    parsed=urlsplit(route.request.url); query=parse_qs(parsed.query); path=parsed.path
    calls.append((route.request.method,path,query))
    if path == '/api/tasks/metrics':
        if metrics_failure: return fulfill(route,{'detail':'private metrics outage'},502)
        return fulfill(route,{'current':{'by_status':{'open':6,'in_progress':6,'blocked':5}},'throughput':{'completed':31,'claimed':48},'window':{'since':now-86400,'until':now},'generated_at':now})
    if path != '/api/tasks': return fulfill(route,{'ok':True,'channels':[]})
    assert route.request.method == 'GET'
    view=query['view'][0]
    if failure: return fulfill(route,{'detail':'private tasks outage'},502)
    if hold_active and view=='active': pending.append(route);return
    rows=closed if view=='closed' else active
    for field in ['status','priority','assignee']:
        if field in query:
            value=query[field][0]
            rows=[row for row in rows if value in row['assignees']] if field=='assignee' else [row for row in rows if row[field]==value]
    offset=int(query.get('cursor',['fixture:0'])[0].split(':')[1])
    batch=rows[offset:offset+50]
    if unsafe: batch=[{**active[0],'title':'DO-NOT-DISPLAY'}] if view=='closed' else [{**closed[0],'title':'DO-NOT-DISPLAY'}]
    return fulfill(route,{'scope':'global','view':view,'tasks':batch,'total':None if legacy else len(rows),'next_cursor':None if legacy or offset+50>=len(rows) else f'fixture:{offset+50}','checked_at':now,'legacy_limited':legacy})
try:
    with sync_playwright() as playwright:
        browser=playwright.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'],headless=True,args=['--no-sandbox','--disable-dev-shm-usage'])
        for width in (390,768,1280):
            legacy=hold_active=metrics_failure=failure=unsafe=False
            page=browser.new_page(viewport={'width':width,'height':900}); errors=[]
            page.on('pageerror',lambda error:errors.append(str(error)))
            page.route('**/api/**',mock)
            page.goto(base+'/dash/tasks',wait_until='domcontentloaded')
            page.get_by_role('heading',name='Tasks',exact=True).wait_for()
            page.get_by_text(active[0]['title'],exact=True).wait_for()
            page.get_by_text('Waiting for private fixture approval',exact=True).first.wait_for()
            assert page.get_by_role('button',name=re.compile(r'^(Active|Closed)$')).count()==0
            status=page.get_by_label('Status',exact=True)
            expect(status).to_have_value('active')
            status.select_option('done')
            page.get_by_text(closed[0]['title'],exact=True).wait_for()
            status.select_option('cancelled')
            page.get_by_text(closed[1]['title'],exact=True).wait_for()
            status.select_option('open')
            page.get_by_text(active[0]['title'],exact=True).wait_for()
            assert page.get_by_role('button',name='Next page',exact=True).is_disabled()
            page.get_by_label('Status',exact=True).select_option('closed')
            page.get_by_text(closed[0]['title'],exact=True).wait_for()
            assert page.evaluate('document.documentElement.scrollWidth <= innerWidth'),width
            if os.environ.get('TASKS_SCREENSHOT_DIR'):
                page.screenshot(path=str(Path(os.environ['TASKS_SCREENSHOT_DIR'])/f'tasks-{width}.png'))
            # 235 rows at 50 per page is 5 pages. Wait on each page's own state rather than reading
            # the Next button while a page is still loading (a slow runner stopped early at page 2).
            seen=[]; cells=page.locator('.tasks-table tbody tr td:first-child strong'); next_button=page.get_by_role('button',name='Next page',exact=True)
            for number in range(1,6):
                if number>1:
                    previous_first=cells.first.text_content()
                    expect(next_button).to_be_enabled()
                    next_button.click()
                    page.get_by_text(re.compile(rf'^Page {number} ·')).wait_for()
                    expect(cells.first).not_to_have_text(previous_first)
                expect(cells).to_have_count(50 if number<5 else 35)
                seen.extend(cells.all_text_contents())
            expect(next_button).to_be_disabled()
            assert len(seen)==235 and len(set(seen))==235,(width,len(seen))
            assert number==5
            page.get_by_role('button',name='Previous page',exact=True).click()
            page.get_by_text(re.compile(r'^Page 4 ·')).wait_for()
            page.get_by_label('Priority',exact=True).select_option('high')
            page.get_by_text(re.compile(r'^Page 1 ·')).wait_for()
            page.get_by_label('Assignee',exact=True).fill('%4')
            page.get_by_role('button',name='Apply assignee',exact=True).click()
            page.get_by_text(re.compile('40 matching')).wait_for()
            assert page.get_by_role('button',name='Previous page',exact=True).is_disabled()
            page.get_by_label('Assignee',exact=True).fill('%999')
            page.get_by_role('button',name='Apply assignee',exact=True).click()
            page.get_by_text('No tasks match these filters.',exact=True).wait_for()
            page.get_by_role('button',name='Clear assignee',exact=True).click()
            page.get_by_label('Priority',exact=True).select_option('')
            page.get_by_label('Status',exact=True).select_option('active')
            page.get_by_text(active[0]['title'],exact=True).wait_for()
            legacy=True
            page.get_by_role('button',name='Refresh',exact=True).click()
            page.get_by_text(re.compile('This daemon shows the first page only')).wait_for()
            assert page.get_by_role('button',name='Next page',exact=True).is_disabled()
            legacy=False;hold_active=True
            with page.expect_request(lambda request:'/api/tasks?view=active' in request.url):
                page.get_by_role('button',name='Refresh',exact=True).click()
            page.get_by_text('Loading tasks…',exact=True).wait_for()
            assert pending
            page.get_by_label('Status',exact=True).select_option('closed')
            page.get_by_text(closed[0]['title'],exact=True).wait_for()
            for route in pending:
                try: fulfill(route,{'scope':'global','view':'active','tasks':active,'total':17,'next_cursor':None,'checked_at':now,'legacy_limited':False})
                except Exception: pass  # An aborted request may already be canceled by Chromium.
            pending.clear();hold_active=False
            assert page.get_by_text(active[0]['title'],exact=True).count()==0
            metrics_failure=True
            page.get_by_role('button',name='Refresh',exact=True).click()
            page.get_by_text(re.compile('Metrics unavailable:')).wait_for()
            page.get_by_text(closed[0]['title'],exact=True).wait_for()
            unsafe=True
            page.get_by_role('button',name='Refresh',exact=True).click()
            page.get_by_role('alert').wait_for()
            assert page.get_by_text('DO-NOT-DISPLAY',exact=True).count()==0
            unsafe=False;failure=True
            page.get_by_role('button',name='Refresh',exact=True).click()
            page.get_by_role('alert').filter(has_text='private tasks outage').wait_for()
            assert not errors,errors
            print(json.dumps({'width':width,'closed_rows_once':len(seen),'paging_filters_empty':'PASS','legacy':'PASS','late_response':'PASS','errors_and_metrics_independence':'PASS','horizontal_overflow':False,'page_errors':errors}),flush=True)
            page.close()
        assert all(method=='GET' for method,path,_ in calls if path.startswith('/api/tasks'))
        browser.close()
finally:
    server.shutdown();server.server_close()
