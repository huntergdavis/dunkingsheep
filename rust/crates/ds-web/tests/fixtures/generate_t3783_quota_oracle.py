"""Generate the t3783 oracle inside a scratch-only isolated sandbox.

Mount the repository read-only at /app and a fresh scratch directory at /work;
use the pinned Python 3.14.4 venv, private tmux directory and a PATH without
pactl. This extracts only the Python summary functions: it does not import the
web application, run startup hooks, contact a daemon, or access live data.
"""
import ast, calendar, datetime, json, math, os, shutil, sqlite3, sys
from pathlib import Path
from zoneinfo import ZoneInfo
assert os.environ['PATH']=='/bin-safe' and os.environ['TMUX_TMPDIR']=='/tmp/tmux-isolation'
assert shutil.which('pactl') is None and not os.access('/usr/bin/pactl',os.X_OK) and not os.access('/bin/pactl',os.X_OK)
assert b'tmux blocked:' in Path('/usr/bin/tmux').read_bytes()
assert not Path('/home/hunter').exists() and not Path('/run/user').exists()
assert all(line.split()[1]!='00000000' for line in Path('/proc/net/route').read_text().splitlines()[1:] if len(line.split())>1)
root=Path('/work/quota-repro');root.mkdir(exist_ok=False)
assert root.resolve().is_relative_to(Path('/work').resolve())
tree=ast.parse(Path('/app/bridges/ops_metrics.py').read_text())
names={'iso','parse_time','_unique_authored_trial','experiment_summary','host_health','opencode_usage','summary'}
nodes=[n for n in tree.body if isinstance(n,(ast.FunctionDef,ast.AsyncFunctionDef)) and n.name in names]
ns={'PROJECT':'poolrad','HOST_WATCH_STATE':root/'missing-host-watch-state.json','OPENCODE_DB':root/'missing-opencode.db','datetime':datetime.datetime,'timedelta':datetime.timedelta,'timezone':datetime.timezone,'calendar':calendar,'math':math,'json':json,'sqlite3':sqlite3,'ZoneInfo':ZoneInfo,'Path':Path}
exec(compile(ast.fix_missing_locations(ast.Module(body=nodes,type_ignores=[])),'<scratch-ops-metrics>','exec'),ns)
now=datetime.datetime.fromisoformat('2026-11-08T12:00:00+00:00');out={}
for name in ('equal','codex_newer','claude_newer','codex_only','claude_only','none','codex_same_provider_tie','claude_same_provider_tie'):
 db=root/(name+'.sqlite3');assert not db.exists()
 c=sqlite3.connect(db);c.row_factory=sqlite3.Row
 c.executescript(Path('/app/rust/crates/ds-web/src/ops_schema.sql').read_text())
 c.executescript(Path('/app/rust/crates/ds-web/tests/fixtures/t3783_quota_seed.sql').read_text())
 if name in ('codex_newer','claude_newer'):
  provider=name.removesuffix('_newer');c.execute("UPDATE quota_observations SET observed_at='2026-11-08T11:30:00Z' WHERE provider=? AND observed_at='2026-11-08T11:00:00Z'",(provider,))
 elif name in ('codex_only','claude_only'):
  keep=name.removesuffix('_only');c.execute('DELETE FROM quota_observations WHERE provider != ?', (keep,))
 elif name=='none': c.execute('DELETE FROM quota_observations')
 elif name.endswith('_same_provider_tie'):
  provider=name.removesuffix('_same_provider_tie');other='claude' if provider=='codex' else 'codex'
  c.execute("UPDATE quota_observations SET observed_at='2026-11-08T10:30:00Z' WHERE provider=? AND observed_at='2026-11-08T11:00:00Z'",(other,))
  c.execute("INSERT INTO quota_observations(project,provider,cohort,pane_id,observed_at,precision,weekly_percent_left,reset_at,evidence_ref) VALUES('poolrad',?,'weekly','%new','2026-11-08T11:00:00Z','second',33,'2026-11-15T00:00:00Z','fixture:tie:newest')",(provider,))
 c.commit();result=ns['summary'](c,now=now);out[name]={'quota':result['quota'],'quotas':result['quotas']};c.close()
(root/'quota-oracle.json').write_text(json.dumps({'python':sys.version.split()[0],'reference_time':now.isoformat(),'cases':out},indent=2,sort_keys=True)+'\n')
print('cases',len(out));print('selected',json.dumps({k:(v['quota'] or {}).get('provider') for k,v in out.items()},sort_keys=True))
