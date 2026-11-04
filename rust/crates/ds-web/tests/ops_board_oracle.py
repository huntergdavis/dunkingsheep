import ast,asyncio,json,sys,types
from pathlib import Path
from typing import Annotated
from fastapi import FastAPI,Depends,HTTPException,Header
calls=[]
def auth(x_fail:Annotated[str|None,Header()]=None):
 calls.append('auth')
 if x_fail:raise HTTPException(401,'Sign in required')
 return 'owner'
board=types.ModuleType('ops_board');board._client=lambda:None
board.defer_question=lambda client,task_id:(calls.append(['defer',task_id]) or {'id':task_id,'priority':'low'})
sys.modules['ops_board']=board
app=FastAPI();src=Path('web/app.py').read_text();node=next(n for n in ast.parse(src).body if isinstance(n,ast.FunctionDef) and n.name=='not_now_owner_question')
exec(compile(ast.Module(body=[node],type_ignores=[]),'web/app.py','exec'),dict(app=app,Annotated=Annotated,Depends=Depends,require_auth=auth,HTTPException=HTTPException))
async def main():
 rows=[]
 cases=[('missing',b'',None),('null',b'null','application/json'),('array',b'[]','application/json'),('string',b'"x"','application/json'),('invalid',b'{','application/json'),('trailing',b'{"task_id":1}x','application/json'),('text',b'{"task_id":"t9"}','text/plain'),('no_type',b'{"task_id":"t9"}',None),('suffix',b'{"task_id":"t9"}','application/problem+json'),('list',b'{"task_id":["t9",true]}','application/json'),('map',b'{"task_id":{"z":1,"a":false}}','application/json'),('unicode_strip',json.dumps({'task_id':'\x1ct9\x1f'}).encode(),'application/json')]
 for name,value in [('false',False),('zero',0),('empty_list',[]),('empty_map',{}),('float',1e-7),('large_integer',2**100),('nested',{'quote': ["a'b",None,True]}),('nan',float('nan')),('infinity',float('inf')),('negative_infinity',-float('inf'))]:
  cases.append((name,json.dumps({'task_id':value}).encode(),'application/json'))
 for name,value in [('invalid_nan',float('nan')),('invalid_inf',float('inf')),('invalid_nested',[float('nan')])]:
  cases.append((name,json.dumps(value).encode(),'application/json'))
 for name,body,ct in cases:
  for fail in [False,True]:
   calls.clear();headers=[] if ct is None else [(b'content-type',ct.encode())]
   if fail:headers.append((b'x-fail',b'1'))
   out=[]
   async def receive():return {'type':'http.request','body':body,'more_body':False}
   async def send(msg):out.append(msg)
   scope={'type':'http','asgi':{'version':'3.0'},'http_version':'1.1','method':'POST','scheme':'http','path':'/api/ops-board/not-now','raw_path':b'/api/ops-board/not-now','query_string':b'','root_path':'','headers':headers,'client':('127.0.0.1',1),'server':('test',80)}
   try: await app(scope,receive,send)
   except ValueError:
    assert any(m.get('status')==500 for m in out), out
   status=next(m['status'] for m in out if m['type']=='http.response.start')
   raw=b''.join(m.get('body',b'') for m in out if m['type']=='http.response.body')
   try:payload=json.loads(raw)
   except ValueError:payload=raw.decode()
   rows.append(dict(name=name,body=body.decode(),content_type=ct,fail_auth=fail,status=status,json=payload,calls=list(calls)))
 print(json.dumps({'versions':{'fastapi':__import__('fastapi').__version__,'starlette':__import__('starlette').__version__},'cases':rows},indent=2,ensure_ascii=False))
asyncio.run(main())
