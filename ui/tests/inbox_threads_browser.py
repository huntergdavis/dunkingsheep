"""Synthetic thread/reply workflow; no production daemon, auth, or transport calls."""
from thread_page_fixture import project
import json,os,threading
from pathlib import Path
from http.server import SimpleHTTPRequestHandler,ThreadingHTTPServer
from urllib.parse import urlsplit,parse_qs
from playwright.sync_api import sync_playwright
DIST=Path(__file__).resolve().parents[1]/'dist'
class Handler(SimpleHTTPRequestHandler):
    def translate_path(self,path):return str(DIST/('index.html' if path.startswith('/dash/') else path.lstrip('/')))
    def log_message(self,*args):pass
server=ThreadingHTTPServer(('127.0.0.1',0),Handler);threading.Thread(target=server.serve_forever,daemon=True).start();base=f'http://127.0.0.1:{server.server_port}'
def row(i,body,kind='human',root=1,parent=None,sender='human'):
    return dict(id=i,message_id=f'i{i}',body=body,sender='recorded lane' if sender=='agent' else None,role='owner' if sender=='human' else 'agent',created_at=f'2026-11-14T00:00:{i:02d}Z',conversation_source=kind,audience=None,mailbox='stable-lane' if i==1 else 'owner',thread_version=1,parent_id=f'i{parent}' if parent else None,root_id=f'i{root}',conversation_kind=kind,sender_kind=sender,sender_route_kind='human' if sender=='human' else 'lane')
def fulfill(route,value,status=200):route.fulfill(status=status,content_type='application/json',body=json.dumps(project(route,value)))
try:
 with sync_playwright() as p:
  browser=p.chromium.launch(executable_path=os.environ['CHROMIUM_PATH'],headless=True,args=['--no-sandbox','--disable-dev-shm-usage'])
  for width in (360,768,1280):
   human=[row(1,'ROOT QUESTION'),row(2,'LANE ANSWER',parent=1,sender='agent'),row(3,'HUMAN FOLLOW UP',parent=2)]
   vp=[row(10,'VP ROOT',kind='vp',root=10,sender='agent'),row(11,'VP FOLLOW UP',kind='vp',root=10,parent=10,sender='agent')]
   revision=0;posts=[];pending=[];mode='fail'
   def mock(route):
    global revision,mode,vp
    path=urlsplit(route.request.url).path;q=parse_qs(urlsplit(route.request.url).query)
    if path=='/api/owner-thread':
     view=q['filter'][0];items=human if view=='human_threads' else vp if view=='vp_threads' else [dict(row(20,'LEGACY BODY'),thread_version=None,parent_id=None,root_id=None,conversation_kind=None,sender_kind=None,conversation_source='historical',historical_reason='prior_version',historical_labels=[])]
     return fulfill(route,dict(items=items,next_before=None,source_filter=view,include_internal=False,view_revision=revision))
    if path=='/api/owner-send':
     body=route.request.post_data_json;posts.append(body)
     assert set(body)=={'text','parent_id'} and body['parent_id'] in ('i3','i11'),body
     if mode=='fail':return fulfill(route,{'detail':'Synthetic refused reply'},409)
     if mode=='bad':return fulfill(route,dict(outcome='stored',message_id='i5',thread=dict(thread_version=1,parent_id='i999',root_id='i1',conversation_kind='human',sender_kind='human')))
     if mode=='hold':pending.append(route);return
     assert mode=='promote'
     revision+=1
     promoted=[{**item,'conversation_source':'human','conversation_kind':'human'} for item in vp if item['root_id']=='i10']
     promoted_ids={item['message_id'] for item in promoted}
     # Promotion updates existing stored rows; it cannot duplicate the addressed notice.
     human[:]=[item for item in human if item['message_id'] not in promoted_ids]+promoted
     vp=[item for item in vp if item['root_id']!='i10']
     new=row(12,body['text'],root=10,parent=11);human.append(new)
     return fulfill(route,dict(outcome='stored',message_id='i12',thread=new))
    return fulfill(route,{'channels':[],'ok':True})
   page=browser.new_page(viewport={'width':width,'height':900});errors=[];page.on('pageerror',lambda e:errors.append(str(e)));page.route('**/api/**',mock)
   page.goto(base+'/dash/cos',wait_until='domcontentloaded')
   reader=page.get_by_role('article',name='Selected conversation');reader.get_by_role('heading',name='Conversation i1',exact=True).wait_for()
   for body in ('ROOT QUESTION','LANE ANSWER','HUMAN FOLLOW UP'):assert reader.get_by_text(body,exact=True).count()==1
   assert reader.locator('p').filter(has_text=__import__('re').compile('^Reply to i2$')).count()==1
   form=page.get_by_role('form',name='Reply to message');text=form.get_by_role('textbox',name='Reply message');send=form.get_by_role('button',name='Send reply',exact=True)
   text.fill('KEPT REPLY DRAFT');send.click();form.get_by_role('alert').filter(has_text='Synthetic refused reply').wait_for();assert text.input_value()=='KEPT REPLY DRAFT'
   mode='bad';send.click();form.get_by_role('alert').filter(has_text='receipt could not be verified').wait_for();assert text.input_value()=='KEPT REPLY DRAFT'
   mode='hold';send.click();form.get_by_role('button',name='Sending…',exact=True).wait_for();assert len(posts)==3;assert text.is_disabled()
   new=row(5,'KEPT REPLY DRAFT',parent=3);human.append(new);fulfill(pending.pop(),dict(outcome='stored',message_id='i5',thread=new));mode='promote'
   form.get_by_role('status').filter(has_text='Reply stored as i5').wait_for();assert text.input_value()==''
   reader.get_by_text('KEPT REPLY DRAFT',exact=True).wait_for()
   # An addressed agent row appears alone; opening its recorded link loads the VP history.
   human.append({**vp[1],'conversation_source':'human','recipient_route_kind':'human'})
   vp.append({**row(13,'SYSTEM ALERT EVENT',kind='vp',root=13,sender='system'),'audience':'agent','kind_labels':['System','Alert','Agent-only']})
   nav=page.get_by_role('navigation',name='Inbox views');nav.get_by_role('button',name='VP',exact=True).click();nav.get_by_role('button',name='Your inbox',exact=True).click()
   reader=page.get_by_role('article',name='Selected conversation');reader.get_by_role('heading',name='Conversation i10',exact=True).wait_for()
   assert reader.get_by_text('VP FOLLOW UP',exact=True).count()==1
   assert reader.get_by_text('VP ROOT',exact=True).count()==0
   reader.get_by_role('button',name='Open VP thread i10',exact=True).click()
   page.get_by_role('heading',name='Conversation i10',exact=True).wait_for()
   assert reader.get_by_text('VP ROOT',exact=True).count()==1
   page.locator('.owner-inbox-row').filter(has_text='SYSTEM ALERT EVENT').click()
   reader.get_by_text('System · Agent-only · Alert',exact=True).first.wait_for()
   page.locator('.owner-inbox-row').filter(has_text='VP FOLLOW UP').click()
   form=page.get_by_role('form',name='Reply to message');form.get_by_role('textbox',name='Reply message').fill('HUMAN JOINS VP THREAD');form.get_by_role('button',name='Send reply',exact=True).click()
   reader.get_by_text('SYSTEM ALERT EVENT',exact=True).wait_for();assert page.get_by_text('VP ROOT',exact=True).count()==0
   nav.get_by_role('button',name='Your inbox',exact=True).click();reader=page.get_by_role('article',name='Selected conversation');reader.get_by_text('HUMAN JOINS VP THREAD',exact=True).wait_for();assert reader.get_by_text('VP ROOT',exact=True).count()==1
   if os.environ.get('INBOX_THREAD_SCREENSHOT_DIR'):page.screenshot(path=str(Path(os.environ['INBOX_THREAD_SCREENSHOT_DIR'])/f'inbox-thread-{width}.png'),full_page=True)
   nav.get_by_role('button',name='Historical / prior versions',exact=True).click();page.get_by_text('LEGACY BODY',exact=True).first.wait_for();assert page.get_by_role('form',name='Reply to message').count()==0
   assert page.evaluate('document.documentElement.scrollWidth <= innerWidth');assert not errors,errors
   print(json.dumps(dict(width=width,thread_chain='PASS',draft_failure_and_bad_receipt='PASS',single_flight='PASS',vp_promotion_cache_invalidation='PASS',historical_no_reply='PASS',addressed_agent_no_promotion_and_vp_link='PASS',system_alert_agent_only_labels='PASS')),flush=True);page.close()
  browser.close()
finally:server.shutdown()
