"""Private synthetic thread heads/detail/poll/paging regression. No live APIs."""
from inbox_actions import open_thread
import json, os, threading
from datetime import datetime, timezone
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit, parse_qs
from playwright.sync_api import sync_playwright
DIST=Path(__file__).resolve().parents[1]/'dist'
class Handler(SimpleHTTPRequestHandler):
 def translate_path(self,path):return str(DIST/('index.html' if path.startswith('/dash/') else path.lstrip('/')))
 def log_message(self,*args):pass
server=ThreadingHTTPServer(('127.0.0.1',0),Handler);threading.Thread(target=server.serve_forever,daemon=True).start();base=f'http://127.0.0.1:{server.server_port}'
def message(n,root):
 return dict(id=n,message_id=f'i{n}',body=f'FULL BODY {n}',sender='Fixture lane',role='agent',created_at=datetime.fromtimestamp(1794770000+n,timezone.utc).isoformat(),sort_at=1794770000+n,conversation_source='human',audience=None,mailbox='owner',thread_version=1,parent_id=None if n==root else f'i{root}',root_id=f'i{root}',conversation_kind='human',sender_kind='agent',state='queued',kind_labels=[])
def fulfill(route,data,status=200):route.fulfill(status=status,content_type='application/json',body=json.dumps(data))
try:
 with sync_playwright() as p:
  browser=p.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'],headless=True,args=['--no-sandbox','--disable-dev-shm-usage'])
  for width in (360,768,1280):
   rows=[message(n,1 if n<=8 else 9) for n in range(1,16)];mode='normal';pending=[];requests=[];hold_root=None;reload=False;offline_older=False
   def mock(route):
    global reload
    path=urlsplit(route.request.url).path;q=parse_qs(urlsplit(route.request.url).query);requests.append((route.request.method,path,q))
    if path!='/api/owner-thread':return fulfill(route,{'ok':True,'time_zone':'America/Los_Angeles','channels':[]})
    assert route.request.method=='GET' and q.get('thread_mode',[None])[0] in ('heads','messages')
    if mode=='legacy':return fulfill(route,{'items':[{'body':'UNSAFE LEGACY FALLBACK'}]})
    root=q.get('root_id',[None])[0];cursor=q.get('thread_cursor',[None])[0]
    if root and root==hold_root:pending.append(route);return
    key='threads' if q['thread_mode'][0]=='heads' else 'messages'
    data=dict(thread_mode=q['thread_mode'][0],source_filter=q['filter'][0],include_internal=False,view_revision=0,fence_id=9999,next_cursor=None,reload_from_top=False)
    if key=='messages':data['root_id']=root
    if cursor and offline_older:return fulfill(route,{'detail':'Older fixture offline'},503)
    if cursor and reload:reload=False;data.update(reload_from_top=True,message='List changed, reload from top.');data[key]=[];return fulfill(route,data)
    if key=='threads':
     groups={}
     for row in rows:groups.setdefault(row['root_id'],[]).append(row)
     heads=[]
     for thread,items in groups.items():
      latest=max(items,key=lambda r:r['sort_at']);preview={k:v for k,v in latest.items() if k!='body'};preview['preview']=latest['body']
      heads.append(dict(thread_id=thread,latest=preview,message_count=len(items),unread_count=sum(r['state']=='queued' for r in items)))
     heads.sort(key=lambda h:h['latest']['sort_at'],reverse=True);offset=int(cursor.split(':')[1]) if cursor else 0;data['threads']=heads[offset:offset+25]
     if len(heads)>offset+25:data['next_cursor']=f'heads:{offset+25}'
    else:
     items=sorted([r for r in rows if r['root_id']==root],key=lambda r:r['sort_at']);end=int(cursor.split(':')[1]) if cursor else len(items);data['messages']=items[max(0,end-50):end]
     if end>50:data['next_cursor']=f'messages:{end-50}'
    return fulfill(route,data)
   def open_page():
    page=browser.new_page(viewport={'width':width,'height':900});page.route('**/api/**',mock);page.goto(base+'/dash/cos',wait_until='domcontentloaded');
    if mode!='legacy': open_thread(page)
    return page
   page=open_page();errors=[];page.on('pageerror',lambda e:errors.append(str(e)));reader=page.get_by_role('article',name='Selected conversation');reader.get_by_text('FULL BODY 15',exact=True).wait_for()
   assert page.locator('.owner-inbox-row').count()==2;assert reader.locator('.owner-thread-message').count()==7
   open_thread(page, 'FULL BODY 8');reader.get_by_text('FULL BODY 1',exact=True).wait_for();assert reader.locator('.owner-thread-message').count()==8
   draft=page.get_by_role('textbox',name='Reply message');draft.fill('KEEP DRAFT WHILE POLLING');rows.append(message(16,1));reader.get_by_text('FULL BODY 16',exact=True).wait_for(timeout=20000)
   assert page.locator('.owner-inbox-row').count()==2;assert '9 unread' in page.locator('.owner-inbox-row').filter(has_text='FULL BODY 16').inner_text();assert draft.input_value()=='KEEP DRAFT WHILE POLLING'
   assert page.get_by_role('form',name='Reply to message').get_by_text('In reply to i8',exact=False).count()==1
   open_thread(page, 'FULL BODY 15');reader.get_by_text('FULL BODY 15',exact=True).wait_for();hold_root='i1'
   open_thread(page, 'FULL BODY 16');page.wait_for_timeout(150);assert pending
   open_thread(page, 'FULL BODY 15');reader.get_by_text('FULL BODY 15',exact=True).wait_for()
   for route in pending:fulfill(route,dict(thread_mode='messages',source_filter='human_threads',include_internal=False,view_revision=0,fence_id=9999,next_cursor=None,reload_from_top=False,root_id='i1',messages=[message(1,1)]))
   pending.clear();hold_root=None;page.wait_for_timeout(100);assert reader.get_by_text('FULL BODY 1',exact=True).count()==0
   open_thread(page, 'FULL BODY 16');reader.get_by_text('FULL BODY 16',exact=True).wait_for();assert draft.input_value()=='KEEP DRAFT WHILE POLLING'
   assert page.evaluate('document.documentElement.scrollWidth <= innerWidth');assert not errors,errors
   if os.environ.get('OWNER_INBOX_SCREENSHOT_DIR'):page.screenshot(path=str(Path(os.environ['OWNER_INBOX_SCREENSHOT_DIR'])/f'threads-{width}.png'),full_page=True)
   print(json.dumps(dict(width=width,fifteen_messages_two_heads='PASS',detail_poll_counts_selection_parent_draft='PASS',late_root_response='PASS')),flush=True);page.close()
   if width!=1280:continue
   rows=[message(n,1) for n in range(1,122)];page=open_page();reader=page.get_by_role('article',name='Selected conversation');reader.get_by_text('FULL BODY 121',exact=True).wait_for();assert reader.locator('.owner-thread-message').count()==50
   offline_older=True;reader.get_by_role('button',name='Load earlier messages',exact=True).click();reader.get_by_role('alert').filter(has_text='Older fixture offline').wait_for();assert reader.locator('.owner-thread-message').count()==50
   offline_older=False;reader.get_by_role('button',name='Load earlier messages',exact=True).click();reader.get_by_text('FULL BODY 22',exact=True).wait_for();assert reader.locator('.owner-thread-message').count()==100
   reader.get_by_role('button',name='Load earlier messages',exact=True).click();reader.get_by_text('FULL BODY 1',exact=True).wait_for();assert reader.locator('.owner-thread-message').count()==121
   assert reader.locator('.owner-thread-message h3').all_text_contents()==[f'Fixture lane · i{n}' for n in range(1,122)];page.close()
   page=open_page();reader=page.get_by_role('article',name='Selected conversation');reader.get_by_text('FULL BODY 121',exact=True).wait_for();page.get_by_role('textbox',name='Reply message').fill('KEEP RELOAD DRAFT');reload=True;reader.get_by_role('button',name='Load earlier messages',exact=True).click()
   reader.get_by_text('Conversation changed, reloading from the newest messages.',exact=True).wait_for();reader.get_by_text('FULL BODY 121',exact=True).wait_for();assert reader.locator('.owner-thread-message').count()==50;assert page.get_by_role('textbox',name='Reply message').input_value()=='KEEP RELOAD DRAFT';page.close()
   rows=[message(n,n) for n in range(1,28)];page=open_page();page.locator('.owner-inbox-row').nth(24).wait_for();assert page.locator('.owner-inbox-row').count()==25;page.get_by_role('button',name='Load older conversations',exact=True).click();page.locator('.owner-inbox-row').nth(26).wait_for();assert page.locator('.owner-inbox-row').count()==27;page.close()
   mode='legacy';page=open_page();page.get_by_role('alert').wait_for();assert page.get_by_text('UNSAFE LEGACY FALLBACK',exact=True).count()==0;page.close();assert all(method=='GET' for method,path,q in requests if path=='/api/owner-thread')
   print('Long detail paging/chronology, older failure, clean reload/draft, older heads, legacy refusal, GET-only PASS',flush=True)
  browser.close()
finally:server.shutdown()
