"""Cold-cache Inbox payload/interaction benchmark against a private HTTP fixture (uncompressed, like ds-web).
Timing is reported for this machine; INBOX_PERFORMANCE_BUDGET_MS opts into a timing gate.
"""
import gzip, json, math, mimetypes, os, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit, parse_qs
from types import SimpleNamespace
from playwright.sync_api import sync_playwright
from thread_page_fixture import project
DIST=Path(__file__).resolve().parents[1]/'dist'
requests=[];posts=[]
items=[dict(message_id=f'i{n}',body=f'Message from agent {n}\n<script>window.bad=true</script>',sender=f'Agent {n}',role='agent',created_at='2026-11-17T00:00:00Z',sort_at=n,conversation_source='human',audience=None,thread_version=1,parent_id=None,root_id=f'i{n}',conversation_kind='human',sender_kind='agent',mailbox='owner',subject=f'Project update {n}') for n in range(1,26)]
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def do_POST(self):
  value=json.loads(self.rfile.read(int(self.headers.get('Content-Length',0))) or '{}');posts.append((self.path,value))
  response={'ok':True}
  if self.path=='/api/owner-send':
   parent=value['parent_id'];response=dict(outcome='stored',message_id='i99',thread=dict(thread_version=1,parent_id=parent,root_id=parent,conversation_kind='human',sender_kind='human'))
  elif self.path=='/api/pane-inbox':response=dict(outcome='stored',message_id='i98',mailbox=value['pane'],notified=True)
  self.send_json(response)
 def send_json(self,value):self.send_bytes(json.dumps(value).encode(),'application/json')
 def send_bytes(self,data,mime):
  self.send_response(200);self.send_header('Content-Type',mime);self.send_header('Content-Length',str(len(data)));self.send_header('Cache-Control','no-store');self.end_headers();self.wfile.write(data)
 def do_GET(self):
  requests.append(self.path);url=urlsplit(self.path)
  if url.path=='/api/owner-thread':return self.send_json(project(SimpleNamespace(request=SimpleNamespace(url=self.path)),dict(items=items,next_before=None,source_filter='human_threads',include_internal=False,view_revision=0)))
  if url.path=='/api/fleet':return self.send_json({'rows':[{'pane_id':f'%{n}','name':f'Agent {n}','project':'alpha'} for n in [24,25]]})
  if url.path.startswith('/api/'):return self.send_json({'ok':True,'time_zone':'America/Los_Angeles','channels':[]})
  path=DIST/('index.html' if url.path.startswith('/dash/') else url.path.lstrip('/'))
  if not path.is_file():self.send_error(404);return
  self.send_bytes(path.read_bytes(),mimetypes.guess_type(path)[0] or 'application/octet-stream')
server=ThreadingHTTPServer(('127.0.0.1',0),Handler);threading.Thread(target=server.serve_forever,daemon=True).start();base=f'http://127.0.0.1:{server.server_port}'
try:
 with sync_playwright() as p:
  browser=p.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'],headless=True,args=['--no-sandbox','--disable-dev-shm-usage'])
  timings=[];sizes=[];raw_sizes=[];errors=[]
  for run in range(int(os.environ.get('INBOX_PERFORMANCE_RUNS','5'))):
   requests.clear();context=browser.new_context(viewport={'width':1280,'height':900});page=context.new_page();page.on('pageerror',lambda e:errors.append(str(e)))
   cdp=context.new_cdp_session(page);cdp.send('Network.enable');cdp.send('Network.setCacheDisabled',{'cacheDisabled':True});cdp.send('Network.emulateNetworkConditions',{'offline':False,'latency':40,'downloadThroughput':1250000,'uploadThroughput':1250000});cdp.send('Emulation.setCPUThrottlingRate',{'rate':float(os.environ.get('INBOX_CPU_THROTTLE','1'))})
   page.add_init_script("new MutationObserver((_, o) => { if (document.querySelectorAll('.owner-inbox-row').length === 25) { o.disconnect(); requestAnimationFrame(() => requestAnimationFrame(() => { window.inboxReadyMs = performance.now(); })); }}).observe(document, { childList: true, subtree: true });")
   page.goto(base+'/dash/cos?panel=webmail',wait_until='domcontentloaded');page.locator('.owner-inbox-row').nth(24).wait_for()
   page.evaluate('() => new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))')
   page.wait_for_function('window.inboxReadyMs > 0');timings.append(page.evaluate('window.inboxReadyMs'))
   if os.environ.get('INBOX_TRACE') and run == 0: print(page.evaluate("JSON.stringify(performance.getEntriesByType('resource').map(r=>({name:r.name,start:r.startTime,end:r.responseEnd,duration:r.duration})))"),flush=True)
   assert page.url.endswith('/dash/cos')
   assert page.get_by_role('article',name='Selected conversation').count()==0
   assert not [u for u in requests if 'thread_mode=messages' in u or u.startswith('/api/fleet') or '/webmail/' in u],requests
   scripts=page.evaluate("performance.getEntriesByType('resource').filter(r=>new URL(r.name).pathname.endsWith('.js')).map(r=>new URL(r.name).pathname)")
   assert not any(any(name in u for name in ['MessageBody','ConversationView','ProjectTerminal']) for u in scripts),scripts
   size=sum(len(gzip.compress((DIST/u.lstrip('/')).read_bytes())) for u in set(scripts));sizes.append(size)
   raw_size=sum((DIST/u.lstrip('/')).stat().st_size for u in set(scripts));raw_sizes.append(raw_size)
   assert size<=75*1024 and raw_size<=220*1024, (size,raw_size,scripts)
   assert not errors, errors
   context.close()
  for width in (360,768,1280):
   page=browser.new_page(viewport={'width':width,'height':900});page.goto(base+'/dash/cos',wait_until='domcontentloaded')
   for n in (25,24):
    page.locator('.owner-inbox-row').filter(has_text=f'Project update {n}').click()
    page.get_by_role('textbox',name='Reply message').fill(f'Reply for {n}');page.get_by_role('button',name='Send reply',exact=True).click();page.get_by_text('Reply stored as i99',exact=False).wait_for()
    assert posts[-1]==('/api/owner-send',{'text':f'Reply for {n}','parent_id':f'i{n}'})
    page.get_by_role('button',name='Format text',exact=True).click();page.locator('.markdown').wait_for();assert not page.evaluate('window.bad || false')
    assert page.evaluate('document.documentElement.scrollWidth<=innerWidth')
    page.get_by_role('button',name='Back to inbox',exact=True).click()
   page.get_by_role('button',name='Compose message',exact=True).click()
   for n in (25,24):
    page.get_by_role('searchbox',name='Find an agent').fill(f'Agent {n}')
    page.get_by_role('combobox',name='Message recipient').select_option(f'%{n}')
    page.get_by_role('textbox',name='Inbox message').fill(f'Direct {n}')
    page.get_by_role('button',name='Send message',exact=True).click();page.get_by_text('Stored in',exact=False).wait_for()
    assert posts[-1]==('/api/pane-inbox',{'pane':f'%{n}','text':f'Direct {n}'})
   if os.environ.get('INBOX_SCREENSHOT_DIR'): page.screenshot(path=str(Path(os.environ['INBOX_SCREENSHOT_DIR'])/f'inbox-{width}.png'),full_page=True)
   page.close()
  p95=sorted(timings)[math.ceil(len(timings)*.95)-1]
  print(json.dumps({'runs':len(timings),'cold_p95_ms':round(p95,1),'cold_median_ms':round(sorted(timings)[len(timings)//2],1),'initial_js_gzip_bytes':max(sizes),'initial_js_transfer_bytes':max(raw_sizes),'network':'10 Mbps, 40 ms RTT','cpu_throttle':float(os.environ.get('INBOX_CPU_THROTTLE','1')),'transfer':'uncompressed','api':'private synthetic fixture, not live DS','two_agent_send_reply':'PASS','no_initial_bodies_fleet_or_markdown':'PASS'}),flush=True)
  if os.environ.get('INBOX_PERFORMANCE_BUDGET_MS'):assert p95<=float(os.environ['INBOX_PERFORMANCE_BUDGET_MS']),timings
  browser.close()
finally:server.shutdown()
