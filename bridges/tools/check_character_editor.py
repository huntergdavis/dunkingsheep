#!/usr/bin/env python3
"""Isolated browser workflow; never a game-load proof.

Run through poolrad-heavy.sh with --dist pointing at an isolated Vite build.
The browser talks HTTP to the actual ASGI editor router and disk-backed store.
By default the Poolrad CLI is replaced by a labelled synthetic contract double.
--cli plus --reference-source exercises a preserved reference copy with the real
CLI. Neither mode contacts the live desk or controls a game.
"""
import argparse
import functools
import hashlib
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import traceback
from threading import Thread
from urllib.parse import urlsplit
import zipfile

ROOT = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(ROOT), str(ROOT / 'tests')]
from fastapi import FastAPI
from playwright.sync_api import expect, sync_playwright
from server.character_editor import EditorStore, SAVE_NAME, router
from test_character_editor import ASGIFixtureClient, FixtureStore, synthetic_save


MEASURE = """() => {
  const root = document.querySelector('.character-editor');
  const rgb = s => (s.match(/[\\d.]+/g) || []).map(Number);
  const blend = (a,b) => a.slice(0,3).map((v,i)=>v*(a[3]??1)+b[i]*(1-(a[3]??1)));
  const bg = e => { const c=rgb(getComputedStyle(e).backgroundColor);
    return c[3]===1 ? c : blend(c, e.parentElement?bg(e.parentElement):[255,255,255]); };
  const lum = c => c.slice(0,3).map(v=>v/255).map(v=>v<=.04045?v/12.92:((v+.055)/1.055)**2.4)
    .reduce((s,v,i)=>s+v*[.2126,.7152,.0722][i],0);
  const rows=[]; const walk=document.createTreeWalker(root,NodeFilter.SHOW_TEXT);
  while(walk.nextNode()) { const n=walk.currentNode,e=n.parentElement;
    if(!n.textContent.trim() || !e.getClientRects().length || e.closest('button:disabled,select:disabled'))continue;
    const s=getComputedStyle(e),b=bg(e),f=blend(rgb(s.color),b),l=[lum(f),lum(b)].sort((a,b)=>a-b);
    const ratio=(l[1]+.05)/(l[0]+.05), size=parseFloat(s.fontSize);
    const floor=size>=24 || (size>=18.66 && parseInt(s.fontWeight)>=700) ? 3 : 4.5;
    rows.push({text:n.textContent.trim().slice(0,100),foreground:s.color,background:b,
      ratio,floor,pass:ratio>=floor,font:s.fontFamily});
  }
  const controls=[...root.querySelectorAll('button,input,select,a,summary')]
    .filter(e=>e.getClientRects().length && !e.disabled).map(e=>{const r=e.getBoundingClientRect();
      const s=getComputedStyle(e), own=bg(e), outside=bg(e.parentElement);
      const ratio=b=>{const l=[lum(blend(rgb(s.borderTopColor),b)),lum(b)].sort((a,b)=>a-b);return(l[1]+.05)/(l[0]+.05)};
      const border=s.borderTopStyle!=='none' && parseFloat(s.borderTopWidth)>0 ? Math.min(ratio(own),ratio(outside)) : null;
      return {text:e.getAttribute('aria-label')||e.textContent.trim().slice(0,70)||e.id,
        width:r.width,height:r.height,border_ratio:border,pass:r.width>=24 && r.height>=24 && (border===null||border>=3)};});
  return {text:rows,controls,page_overflow:document.documentElement.scrollWidth>innerWidth,
    editor_overflow:root.scrollWidth>root.clientWidth,
    dialog_closed:!document.querySelector('[role="dialog"]')};
}"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--dist', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--cli', type=Path, help='Use a real accepted CLI with --reference-source')
    parser.add_argument('--reference-source', type=Path, help='Preserved private save copy; never a running game folder')
    parser.add_argument('--themes', nargs='+', help='Optional subset of registered theme IDs')
    args = parser.parse_args()
    if bool(args.cli) != bool(args.reference_source):
        parser.error('--cli and --reference-source must be provided together')
    args.output.mkdir(parents=True, exist_ok=True)
    receipt = dict(source_head=subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
                   cases=[], failures=[], not_verified=[
                       'Game load, live desk, current owner save files, physical devices or other browsers',
                       'Overlay pixel contrast, native file chooser, screen reader and full accessibility'])
    receipt['fixture_kind'] = 'preserved reference copy with real CLI' if args.cli else 'synthetic CLI contract double'
    if not args.cli:
        receipt['not_verified'].append('Actual reference edits: browser uses a synthetic CLI contract double')
    names = subprocess.check_output(['git', 'ls-files', '-z', '--cached', '--others', '--exclude-standard',
                                     '--', 'server', 'web', 'tools', 'tests'], cwd=ROOT, text=True).split('\0')
    receipt['source_sha256'] = {n: hashlib.sha256((ROOT/n).read_bytes()).hexdigest() for n in names if n and (ROOT/n).is_file()}
    receipt['build_sha256'] = {str(p.relative_to(args.dist)): hashlib.sha256(p.read_bytes()).hexdigest()
                               for p in args.dist.rglob('*') if p.is_file()}
    themes = sorted(json.loads(p.read_text())['id'] for p in (ROOT/'web/src/themes').glob('*.json'))
    if args.themes:
        if set(args.themes)-set(themes): parser.error('Unknown theme ID')
        themes=args.themes
    input_files = ({p.name:p.read_bytes() for p in args.reference_source.iterdir() if SAVE_NAME.fullmatch(p.name)}
                   if args.reference_source else {'CHRDATA1.SAV':synthetic_save()})
    receipt['input_hashes']={n:hashlib.sha256(b).hexdigest() for n,b in input_files.items()}
    if args.cli:
        receipt['cli_sha256']={p.name:hashlib.sha256(p.read_bytes()).hexdigest()
                               for p in (args.cli,args.cli.with_name('disk_save.py'))}
    with tempfile.TemporaryDirectory() as temp:
        tool = Path(temp)/'synthetic-cli.py'
        tool.write_text('# Synthetic contract double only')
        store = EditorStore(Path(temp)/'store', args.cli) if args.cli else FixtureStore(Path(temp)/'store', tool)
        app = FastAPI()
        app.include_router(router(store, lambda: 'synthetic-auth'))
        client = ASGIFixtureClient(app)
        requests = []
        class Handler(SimpleHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def api(self):
                path = urlsplit(self.path).path
                requests.append(dict(method=self.command, path=path))
                if path.startswith('/api/character-editor/'):
                    result = client.request(self.command, self.path,
                        content=self.rfile.read(int(self.headers.get('Content-Length', 0))),
                        headers=dict(self.headers))
                    self.send_response(result.status_code)
                    for k,v in result.headers.items(): self.send_header(k,v)
                    self.end_headers(); self.wfile.write(result.content)
                else:
                    self.send_response(200 if path=='/api/channels' else 404)
                    self.send_header('Content-Type','application/json'); self.end_headers()
                    self.wfile.write(b'{"channels":[]}' if path=='/api/channels' else b'{}')

            def do_GET(self):
                if self.path.startswith('/api/'): return self.api()
                if self.path.startswith('/dash/'): self.path='/index.html'
                return super().do_GET()

            def do_POST(self):
                return self.api()

        server=ThreadingHTTPServer(('127.0.0.1',0),functools.partial(Handler,directory=str(args.dist.resolve())))
        Thread(target=server.serve_forever,daemon=True).start()
        origin=f'http://127.0.0.1:{server.server_port}'
        try:
            chrome=sorted((Path.home()/'.cache/ms-playwright').glob('chromium-*/chrome-linux64/chrome'))[-1]
            with sync_playwright() as p:
                browser=p.chromium.launch(executable_path=str(chrome),args=['--no-sandbox'])
                receipt['browser']=browser.version
                for theme in themes:
                    for width,height in [(1440,900),(390,844)]:
                        case=dict(theme=theme,viewport=[width,height],captures=[],errors=[])
                        receipt['cases'].append(case)
                        context=browser.new_context(viewport=dict(width=width,height=height),
                            device_scale_factor=1,reduced_motion='reduce',service_workers='block')
                        context.add_init_script('localStorage.setItem("vp-theme",'+json.dumps(theme)+');')
                        page=context.new_page()
                        page.on('pageerror',lambda e:case['errors'].append(str(e)))
                        page.route('**/*',lambda r:r.continue_() if r.request.url.startswith(origin+'/') else r.abort())
                        try:
                            page.goto(origin+'/dash/play?panel=editor',wait_until='networkidle')
                            page.locator('#editor-files').set_input_files([dict(name=n,mimeType='application/octet-stream',buffer=b) for n,b in input_files.items()])
                            page.get_by_role('button',name='Import and inspect',exact=True).click()
                            boxes=page.locator('.editor-profile input')
                            boxes.first.wait_for()
                            expect(boxes.nth(0)).to_be_enabled()
                            expect(boxes.nth(1)).to_be_disabled()
                            boxes.nth(0).check(); expect(boxes.nth(1)).to_be_enabled()
                            boxes.nth(1).check(); boxes.nth(0).uncheck()
                            expect(boxes.nth(1)).not_to_be_checked()
                            expect(boxes.nth(1)).to_be_disabled()
                            boxes.nth(0).check(); boxes.nth(1).check()
                            source_id=page.locator('#editor-source').input_value()
                            original=store.inspect(source_id)['expected_source_hashes']
                            page.locator('#editor-copy-name').fill('Synthetic browser copy')
                            page.get_by_role('button',name='Write edited copy',exact=True).click()
                            page.get_by_role('heading',name='Synthetic browser copy saved',exact=True).wait_for()
                            assert 'UNVERIFIED' in page.locator('.editor-result').inner_text()
                            with page.expect_download() as info:
                                page.get_by_role('link',name='Download save copy').click()
                            with zipfile.ZipFile(info.value.path()) as archive:
                                after=archive.read('CHRDATA1.SAV')
                                assert [i for i,(a,b) in enumerate(zip(input_files['CHRDATA1.SAV'],after)) if a!=b]==[16,152]
                                for name,data in input_files.items():
                                    if name!='CHRDATA1.SAV': assert archive.read(name)==data
                                case['output_character_sha256']=hashlib.sha256(after).hexdigest()
                                exported=json.loads(archive.read('write-receipts.json'))
                                assert exported['game_validation']=='UNVERIFIED' and exported['game_build_loaded'] is None
                                assert temp not in json.dumps(exported)
                            assert original==store.inspect(source_id)['expected_source_hashes']
                            page.locator('.editor-result summary').click()
                            case['measurement']=page.evaluate(MEASURE)
                            for state,selector in [('top','.character-editor h1'),('record','.editor-characters'),('receipt','.editor-result')]:
                                page.locator(selector).scroll_into_view_if_needed()
                                assert not page.locator('[role="dialog"]').count()
                                filename=f'{theme}-{width}-{state}.png'
                                page.screenshot(path=str(args.output/filename))
                                case['captures'].append(filename)
                            # An unknown record is inspectable/exportable, but has no edits.
                            unknown=bytearray(synthetic_save()); unknown[16]=9
                            page.locator('#editor-files').set_input_files(dict(name='CHRDATA2.SAV',mimeType='application/octet-stream',buffer=bytes(unknown)))
                            page.get_by_role('button',name='Import and inspect',exact=True).click()
                            page.get_by_role('heading',name='CHRDATA2.SAV',exact=True).wait_for()
                            for i in range(2): expect(boxes.nth(i)).to_be_disabled()
                            page.get_by_role('button',name='Write unchanged copy',exact=True).click()
                            page.locator('.editor-result').wait_for()
                            with page.expect_download() as info:
                                page.get_by_role('link',name='Download save copy').click()
                            with zipfile.ZipFile(info.value.path()) as archive:
                                assert archive.read('CHRDATA2.SAV')==bytes(unknown)
                            case['workflow_pass']=True
                        except Exception as e:
                            case['errors'].append(traceback.format_exc()); case['workflow_pass']=False
                        finally:
                            context.close()
                        measure=case.get('measurement',{})
                        if case['errors'] or not case.get('workflow_pass') or measure.get('page_overflow') or measure.get('editor_overflow') or any(not row['pass'] for row in measure.get('text',[])+measure.get('controls',[])):
                            receipt['failures'].append(f'{theme}-{width}')
                browser.close()
            receipt['game_requests']=[r for r in requests if r['path'].startswith('/api/game')]
            if receipt['game_requests']: receipt['failures'].append('Unexpected game request')
            if args.reference_source:
                current={p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in args.reference_source.iterdir() if SAVE_NAME.fullmatch(p.name)}
                receipt['reference_source_unchanged']=current==receipt['input_hashes']
                if not receipt['reference_source_unchanged']: receipt['failures'].append('Reference source changed')
        finally:
            server.shutdown(); server.server_close()
            (args.output/'receipt.json').write_text(json.dumps(receipt,indent=2)+'\n')
    print(json.dumps(dict(cases=len(receipt['cases']),failures=receipt['failures'],receipt=str(args.output/'receipt.json'))))
    return bool(receipt['failures'])


if __name__=='__main__':
    raise SystemExit(main())
