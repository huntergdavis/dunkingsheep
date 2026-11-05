"""Isolated actual-Python handler oracle; no daemon, stores, terminal or host writes."""
import ast
import copy
import functools
import json
from pathlib import Path
import sys
import types
import dunk_dispatch
import dunk_roles

class DunkError(Exception):
    retryable = False
class RetryableDunkError(DunkError):
    retryable = True
class TaskError(Exception):
    pass
class InboxError(Exception):
    pass

source = ast.parse(Path('dunk_api.py').read_text())
core = ast.parse(Path('dunk_core.py').read_text())
ns = dict(DunkError=DunkError, TaskError=TaskError, InboxError=InboxError,
          functools=functools, dunk_roles=dunk_roles)
exec(compile(ast.Module(body=[n for n in core.body if isinstance(n, ast.FunctionDef) and n.name == '_as_dunk_error'], type_ignores=[]), 'dunk_core.py', 'exec'), ns)
wanted = {'_bool', 'h_dispatch', 'h_allocation', 'h_shutdown', 'dispatch'}
exec(compile(ast.Module(body=[n for n in source.body if isinstance(n, ast.FunctionDef) and n.name in wanted], type_ignores=[]), 'dunk_api.py', 'exec'), ns)
ns['SUPERUSER_ONLY'] = next(ast.literal_eval(n.value) for n in source.body if isinstance(n, ast.Assign) and any(isinstance(t, ast.Name) and t.id == 'SUPERUSER_ONLY' for t in n.targets))
commands = {}
for n in ast.walk(source):
    if isinstance(n, ast.Call) and isinstance(n.func, ast.Name) and n.func.id == 'Command' and n.args and isinstance(n.args[0], ast.Constant) and n.args[0].value in ('dispatch','allocation','shutdown'):
        name = n.args[0].value
        params = next(k.value for k in n.keywords if k.arg == 'params')
        commands[name] = types.SimpleNamespace(handler=ns[n.args[2].id], required=[], params={k.value: {} for k in params.keys})
ns['COMMANDS_BY_NAME'] = commands
original_derive = dunk_dispatch.derive_board_items
original_summary = dunk_dispatch.summary

outputs = []
for config in json.load(sys.stdin):
    calls = []
    state = dict(last_tick=123,last_result={'cached':17},wakes=0,stops=0)
    def call(name, args, default=None):
        calls.append([name,copy.deepcopy(args)])
        if config.get('fail') == name:
            kind = config.get('kind','RuntimeError')
            cls = dict(DunkError=DunkError,RetryableDunkError=RetryableDunkError,TaskError=TaskError,InboxError=InboxError,TypeError=TypeError,OSError=OSError).get(kind,RuntimeError)
            raise cls(name+' failed')
        return copy.deepcopy(config.get('responses',{}).get(name,default))
    class Queue:
        def __init__(self): call('queue',[])
        def add(self, brief, task_id=None, lane=None): return call('add',[brief,task_id,lane],{'id':7})
        def items(self): return call('items',[],[])
        def remove(self, id): return call('remove',[id],{'removed':True})
    class Store:
        def status(self): return call('status',[],{'workers':[],'offers':[]})
        def register(self,p,po,r,s,c,enabled=True): return call('register',[p,po,r,s,c,enabled],{'registered':True})
        def offer(self,t,p,r,c): return call('offer',[t,p,r,c],{'offered':True})
        def review(self,t,r,b,p,req,c): return call('review',[t,r,b,p,req,c],{'reviewed':True})
    class Allocator:
        @property
        def store(self): call('store',[]); return Store()
        @property
        def last_result(self): return call('last_result',[],state['last_result'])
        @property
        def last_tick(self): return state['last_tick']
        @last_tick.setter
        def last_tick(self,v): call('reset',[]); state['last_tick']=v
    class Wake:
        def set(self): call('wake',[]); state['wakes']=1
    class Tasks:
        def get(self,t): return call('task_get',[t],{'id':'t9'})
        def leases_for(self,t): return call('leases',[t],[])
    class Flock:
        allocator=Allocator()
        tasks=Tasks()
        _next_job_wake=Wake()
        inbox=object()  # callbacks are passed to the explicitly staged tick boundary
        def caller_role(self,c): return call('role',[c],config.get('role','owner'))
        def require_superuser(self,c,a): call('superuser',[c,a])
        def require_turn_caller(self,c,p): call('turn',[c,p])
        def resolve_lanes(self): return call('lanes',[],[{'pane_id':'%7','agent_status':'idle'}])
        def task_list(self,include_done,limit): return call('tasks',[include_done,limit],{'tasks':[]})
        def _authorize_task_complete(self,t,c,operator=False): call('authorize',[t,c,operator])
        def stop_all(self): call('stop_all',[]); state['stops']=1
    ns['_dispatch_allowlist'] = lambda flock,args,lanes: set(call('allow',[args,lanes],['%7']))
    dunk_dispatch.Queue=Queue
    dunk_dispatch._load_state=lambda:call('state',[],{})
    dunk_dispatch.summary=original_summary
    dunk_dispatch.derive_board_items=lambda tasks,dispatched_task_ids:original_derive(tasks,dispatched_task_ids=dispatched_task_ids,now=25000)
    # Inject deterministic time only inside this isolated subprocess.
    import time
    time.time=lambda:25000.0
    sys.modules['dunk_panes']=types.SimpleNamespace(is_retired=lambda p:call('retired',[p],False),all_retired=lambda:list(range(call('all_retired',[],2))))
    def tick(queue,panes,derived,delivery,unread,send,dry_run,lanes):
        assert isinstance(queue,Queue)
        # The leaf must pass its exact caller and sorted allowlist to this producer.
        result=call('tick',[panes,derived,sorted(lanes),dry_run,'%7'],{'dispatched':[],'marker':'fixture tick'})
        # Record the expired-only collection produced by the real unread callback contract.
        # Capture by name, not compiler-dependent closure ordering.
        cells=dict(zip(unread.__code__.co_freevars,[c.cell_contents for c in unread.__closure__]))
        cells['expired'].extend(['%9','%7','%9'])
        return result
    dunk_dispatch.tick=tick
    try:
        result=ns['dispatch'](Flock(),config['command'],config['args'],self_pane_id='%7')
        outcome={'result':result}
    except DunkError as error:
        outcome={'error':str(error),'retryable':error.retryable}
    except Exception as error:
        outcome={'error':'internal error: '+str(error),'retryable':False}
    outputs.append(dict(outcome=outcome,calls=calls,state=state))
print(json.dumps(outputs))
