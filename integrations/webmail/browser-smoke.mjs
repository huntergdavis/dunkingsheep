// Offline client contract smoke test; no production sessions or daemon access.
import { createServer } from 'node:http';
import { readFileSync, existsSync } from 'node:fs';
import { resolve, extname } from 'node:path';
import { createRequire } from 'node:module';
const build=resolve(process.env.DS_MAIL_BUILD_DIR || '../jmap-upstream');
const require=createRequire(resolve(build,'package.json'));
const {chromium}=require('playwright');
const core='urn:ietf:params:jmap:core', mail='urn:ietf:params:jmap:mail', submission='urn:ietf:params:jmap:submission';
let archived=false;
const calls=[];
const email=()=>({id:'i1',blobId:'i1',threadId:'i1',mailboxIds:{[archived?'archive':'inbox']:true},keywords:{},size:20,receivedAt:'2026-11-16T12:00:00Z',sentAt:'2026-11-16T12:00:00Z',from:[{name:'Front Desk',email:'desk@dunkingsheep.invalid'}],to:[{name:'You',email:'owner@dunkingsheep.invalid'}],subject:'Archive acceptance fixture',preview:'Preserve delivery state',hasAttachment:false,attachments:[],textBody:[{partId:'text',type:'text/plain'}],htmlBody:[],bodyValues:{text:{value:'Preserve delivery state',isTruncated:false}}});
const server=createServer((req,res)=>{
  let path=decodeURIComponent(new URL(req.url,'http://local').pathname).replace(/^\/webmail/,'');
  if(path.endsWith('/'))path+='index.html';
  let file=resolve(build,'out','.'+path);
  if (!existsSync(file) && path.startsWith('/en/mail/')) file=resolve(build,'out/en/mail/index.html');
  if(!file.startsWith(resolve(build,'out')+'/') || !existsSync(file)){res.writeHead(404);res.end();return;}
  const type={'.html':'text/html','.js':'text/javascript','.css':'text/css','.json':'application/json','.woff2':'font/woff2'}[extname(file)]||'application/octet-stream';
  res.writeHead(200,{'Content-Type':type});res.end(readFileSync(file));
});
await new Promise(r=>server.listen(0,'127.0.0.1',r));
const base=`http://127.0.0.1:${server.address().port}`;
const browser=await chromium.launch({headless:true,executablePath:process.env.DS_CHROMIUM || '/home/hunter/.cache/ms-playwright/chromium-1243/chrome-linux64/chrome',args:['--no-sandbox']});
try {
 const page=await browser.newPage();const errors=[];page.on('pageerror',e=>errors.push(e.message));
 await page.route('**/.well-known/jmap',r=>r.fulfill({json:{capabilities:{[core]:{},[mail]:{},[submission]:{}},accounts:{owner:{name:'Dunking Sheep',isPersonal:true,isReadOnly:false,accountCapabilities:{[mail]:{},[submission]:{}}}},primaryAccounts:{[mail]:'owner',[submission]:'owner'},username:'owner@dunkingsheep.invalid',apiUrl:base+'/api/jmap',downloadUrl:base+'/download/{blobId}',uploadUrl:base+'/upload',eventSourceUrl:base+'/events',state:'1'}}));
 await page.route('**/api/jmap',async route=>{
  const results=[];
  for(const [method,args,id] of route.request().postDataJSON().methodCalls){
   calls.push(method);let value={accountId:'owner',state:String(archived)};let name=method;
   if(method==='Core/echo')value=args;
   else if(method==='Identity/get')value.list=[{id:'owner',name:'You',email:'owner@dunkingsheep.invalid',mayDelete:false}];
   else if(method==='Mailbox/get')value.list=['inbox','archive','drafts','sent'].map(role=>({id:role,role,name:role,sortOrder:0,parentId:null,totalEmails:role===(archived?'archive':'inbox')?1:0,unreadEmails:0,totalThreads:1,unreadThreads:0,isSubscribed:true,myRights:{mayReadItems:true,mayAddItems:true,mayRemoveItems:true,maySetSeen:true,maySetKeywords:true,maySubmit:true}}));
   else if(method==='Email/query')value={...value,ids:args.filter?.inMailbox && args.filter.inMailbox!==(archived?'archive':'inbox')?[]:['i1'],position:0,total:1,queryState:String(archived),canCalculateChanges:false};
   else if(method==='Email/get')value.list=[email()];
   else if(method==='Thread/get')value.list=[{id:'i1',emailIds:['i1']}];
   else if(method==='Email/set'){if(args.update?.i1?.mailboxIds)archived=!!args.update.i1.mailboxIds.archive;value={...value,oldState:'0',newState:'1',updated:{i1:null}};}
   else {name='error';value={type:method.endsWith('/changes')?'cannotCalculateChanges':'unknownMethod'};}
   results.push([name,value,id]);
  }
  await route.fulfill({json:{methodResponses:results,sessionState:'1'}});
 });
 if(process.env.DS_MAIL_API){
  await page.route('**/.well-known/jmap',async route=>{const r=await fetch(process.env.DS_MAIL_API+'/.well-known/jmap');await route.fulfill({status:r.status,body:await r.text(),contentType:'application/json'});});
  await page.route('**/api/jmap',async route=>{const request=route.request().postDataJSON();const r=await fetch(process.env.DS_MAIL_API+'/api/jmap',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(request)});const response=await r.json();
   for(const [method,args,id] of request.methodCalls){calls.push(method);if(method==='Email/set' && args.update?.i1?.mailboxIds && response.methodResponses.some(v=>v[2]===id && Object.hasOwn(v[1].updated||{},'i1')))archived=!!args.update.i1.mailboxIds.archive;}
   await route.fulfill({status:r.status,json:response});
  });
 }
 await page.goto(base+'/webmail/en/mail/' ,{waitUntil:'domcontentloaded'});
 await page.getByText('Archive acceptance fixture',{exact:true}).first().waitFor({timeout:60000});
 await page.getByText('Archive acceptance fixture',{exact:true}).first().click();
 await page.waitForTimeout(1000);
 await page.getByTitle('Archive (E)',{exact:true}).click();
 for(let tries=0;tries<30 && !archived;tries++)await page.waitForTimeout(100);
 if(!archived)throw new Error('Archive did not reach adapter');
 await page.getByRole('button',{name:'Archive',exact:true}).first().click();
 await page.getByText('Archive acceptance fixture',{exact:true}).first().click();
 await page.getByRole('button',{name:'Move to',exact:true}).click();
 await page.getByText('Inbox',{exact:true}).last().click();
 for(let tries=0;tries<30 && archived;tries++)await page.waitForTimeout(100);
 if(archived)throw new Error('Restore did not reach adapter');
 await page.reload({waitUntil:'domcontentloaded'});
 await page.getByText('Archive acceptance fixture',{exact:true}).first().waitFor({timeout:60000});
 console.log(JSON.stringify({visibleMessage:true,archiveAndRestore:true,reload:true,calls,errors}));

 await page.screenshot({path:resolve(build,'ds-mail-smoke.png'),fullPage:true});
 if(errors.length)throw new Error(errors.join('\n'));
} finally {await browser.close();server.close();}
