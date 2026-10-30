#!/usr/bin/env python3
"""Run via poolrad-heavy.sh; evidence is local, synthetic and STATIC-only."""
import argparse
import hashlib
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import re
import subprocess
import sys
import threading

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from tools import theme_contrast as contrast
from tools.theme_wave_checks import OPEN_ICON_CHECK, ADVISORY_WIDTHS
from tools.theme_overlay_bounds import overlay_proof

WIDTHS = (1422, 1067, 960, 800, 700, 390, 320)
NAMED = ('icon-button-border/own-surface', 'icon-button-panel-border/own-surface',
         'ops-reference-ink/bg', 'ops-reference-border/bg',
         'attach-base-border/own-surface', 'attach-base-text/own-surface',
         'attach-border/own-surface', 'attach-text/own-surface')
NOT_COVERED = ['Rendered overlay pixels/phase, effects beyond the declared --overlay, and non-icon controls',
               'Physical zoom/devices, full-page accessibility and font conformance',
               'Unavailable-channel behavior; fixture uses an available synthetic channel',
               'Routes beyond CoS/Operations and live deployed behavior']


def hashes(paths, root):
    return {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in sorted(paths) if p.is_file()}


def source_hashes():
    # Include build configuration and new, untracked theme files, while honoring
    # Git's ignore rules for private inputs, node_modules and generated output.
    names = subprocess.check_output(['git', 'ls-files', '-z', '--cached', '--others',
                                     '--exclude-standard', '--', 'web', 'tools', 'tests'],
                                    cwd=ROOT, text=True).split('\0')
    return hashes({ROOT / name for name in names if name}, ROOT)


def named_rows(report, candidate, css):
    """Missing own tokens fail even when fallback colors happen to pass."""
    own = contrast.theme_tokens(':root { --wave-sentinel:#000; }\n' + css).get(candidate, {})
    result = []
    for theme in sorted({'default', 'crt', candidate}):
        for name in NAMED:
            matches = [r for r in report['pairs'] if r['theme'] == theme and r['pair'] == name]
            row = dict(matches[0]) if len(matches) == 1 else dict(theme=theme, pair=name, status='FAIL', error='Missing or duplicate named row')
            token = name.split('/')[0]
            if theme == candidate and token.startswith('icon-button') and token not in own:
                row.update(status='FAIL', error='Candidate must declare its own --' + token)
            row['contrast_kind'] = 'STATIC'
            result.append(row)
    return result


def expected_cases(themes, widths):
    return {f'{t}-{v}-{w}-text{s}' for t in themes for v in ('cos', 'operations') for w in widths for s in (1, 2)}


def advisory_rows(report, advisory_widths=ADVISORY_WIDTHS):
    return [dict(case=row['id'], width=row['width'], theme=row.get('theme'),
                 view=row['view'], textScale=row.get('textScale'), check=check['name'])
            for row in report.get('cases', []) if row['width'] in advisory_widths
            for check in row.get('checks', []) if not check['passed']]


def browser_failures(report, expected, output, advisory_widths=ADVISORY_WIDTHS):
    failures = []
    rows = report.get('cases', [])
    ids = [r.get('id') for r in rows]
    if len(ids) != len(set(ids)) or set(ids) != expected:
        failures.append('Missing, duplicate or unexpected browser cases')
    for row in rows:
        if row.get('error') or not row.get('completed'):
            failures.append(row['id'] + ': incomplete/error: ' + str(row.get('error')))
        if not any(c['name'] == OPEN_ICON_CHECK for c in row.get('checks', [])):
            failures.append(row['id'] + ': missing open-icon hit-test check')
        for check in row.get('checks', []):
            if not check['passed'] and row['width'] not in advisory_widths:
                failures.append(row['id'] + ': ' + check['name'])
        required = {'closed', 'keyboard-focus'} if row['view'] == 'operations' else {'closed', 'empty', 'focused-empty', 'typed', 'cleared'}
        captures = row.get('captures', [])
        if not required.issubset({c['state'] for c in captures}):
            failures.append(row['id'] + ': missing required capture state')
        for capture in captures:
            file = (output / capture['file']).resolve()
            if not file.is_relative_to(output.resolve()) or not capture.get('dialog_closed') or not capture.get('captured') or not file.is_file():
                failures.append(row['id'] + ': missing/unguarded image ' + capture['file'])
            elif hashlib.sha256(file.read_bytes()).hexdigest() != capture.get('sha256'):
                failures.append(row['id'] + ': image changed ' + capture['file'])
        if not row.get('checks'):
            failures.append(row['id'] + ': missing measured checks')
        extra = []
        opened = row.get('openDialogCapture')
        if not opened or opened.get('dialog_closed') is not False:
            failures.append(row['id'] + ': missing labelled open-dialog evidence')
        else: extra.append(opened)
        if row['view'] == 'cos':
            paints = row.get('paint', [])
            if len(paints) != 3 or {p['state'] for p in paints} != {'empty', 'focused-empty', 'cleared'}:
                failures.append(row['id'] + ': missing paint states')
            for paint in paints:
                if len(paint.get('captures', [])) != 3:
                    failures.append(row['id'] + ': missing paint noise-control image')
                extra.extend(paint.get('captures', []))
        for capture in extra:
            file = (output / capture['file']).resolve()
            if not file.is_relative_to(output.resolve()) or not file.is_file() or hashlib.sha256(file.read_bytes()).hexdigest() != capture.get('sha256'):
                failures.append(row['id'] + ': missing/changed auxiliary image ' + capture['file'])
        if not row.get('openDialogRejected'):
            failures.append(row['id'] + ': missing negative dialog guard proof')
    return failures


def record_overlay_bounds(summary, output, tokens):
    proof = overlay_proof(summary, tokens)
    path = output / 'overlay-bounds.json'
    path.write_text(json.dumps(proof, indent=2) + '\n')
    rows = proof['rows']
    summary['overlay_bounds'] = dict(file=path.name, sha256=hashlib.sha256(path.read_bytes()).hexdigest(),
        passed=proof['passed'], named_rows=proof['source_rows'], computed_rows=proof['computed_panel_rows'],
        min_endpoint_minimum=min((r['endpointMinimum'] for r in rows), default=None),
        min_continuous=min((r['independentContinuousPhaseLowerBound'] for r in rows), default=None))
    if not proof['passed']:
        summary['failures'].append('Analytical icon overlay bounds failed: ' + '; '.join(proof['errors']))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--theme', required=True)
    parser.add_argument('--base', required=True, help='reviewed main commit, an ancestor of this checkout')
    parser.add_argument('--artifacts', type=Path, required=True, help='new directory under ignored data/')
    parser.add_argument('--include-1440', action='store_true')
    parser.add_argument('--completed-task', action='append', default=[])
    args = parser.parse_args(argv)
    if not re.fullmatch(r'[a-z0-9][a-z0-9-]*', args.theme): parser.error('Invalid theme ID')
    output = args.artifacts.resolve()
    if not output.is_relative_to(ROOT / 'data') or output.exists(): parser.error('Use a new directory under data/')
    output.mkdir(parents=True)
    summary = dict(contrast_kind='STATIC', not_covered=list(NOT_COVERED), commands=[], failures=[], advisories=[], candidate=args.theme)
    original = source_hashes()
    server = thread = None
    def command(name, cmd):
        with (output / (name + '.log')).open('w') as log:
            result = subprocess.run(cmd, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT)
        summary['commands'].append(dict(name=name, command=cmd, exit=result.returncode))
        if result.returncode: summary['failures'].append(name + ' exited ' + str(result.returncode))
        return result.returncode
    result_code = 0
    try:
        summary['timestamp'] = subprocess.check_output(['date', '-Iseconds'], text=True).strip()
        summary['head'] = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
        summary['base'] = subprocess.check_output(['git', 'rev-parse', '--verify', args.base + '^{commit}'], cwd=ROOT, text=True).strip()
        subprocess.run(['git', 'merge-base', '--is-ancestor', summary['base'], 'HEAD'], cwd=ROOT, check=True)
        summary['tracked_diff'] = subprocess.check_output(['git', 'diff', '--name-only', 'HEAD'], cwd=ROOT, text=True).splitlines()
        summary['source_hashes'] = original
        summary['widths'] = list(WIDTHS) + ([1440] if args.include_1440 else [])
        summary['advisory_widths'] = list(ADVISORY_WIDTHS)
        themes = sorted({'default', 'crt', args.theme})
        summary['themes'] = themes
        scope = command('scope', ['node', 'web/tools/check-theme-scope.cjs'])
        gate_cmd = [sys.executable, 'tools/theme_contrast.py', '--json']
        for task in args.completed_task: gate_cmd += ['--completed-task', task]
        gate_exit = command('contrast', gate_cmd)
        report = json.loads((output / 'contrast.log').read_text())
        if 'pairs' not in report: raise ValueError('Contrast parser did not produce rows')
        for row in report['pairs']: row['contrast_kind'] = 'STATIC'
        (output / 'contrast.json').write_text(json.dumps(report, indent=2) + '\n')
        summary['completed_tasks'] = args.completed_task
        summary['named_rows'] = named_rows(report, args.theme, (ROOT / 'web/src/themes' / (args.theme + '.css')).read_text())
        for row in summary['named_rows']:
            if row['status'] == 'FAIL': summary['failures'].append('STATIC named ' + row['theme'] + ' ' + row['pair'] + ': ' + row.get('error', 'contrast failure'))
        if scope or gate_exit or summary['failures']: raise ValueError('Static preflight failed; browser not run')
        if command('build', ['npm', '--prefix', 'web', 'run', 'build']): raise ValueError('Build failed; browser not run')
        dist = ROOT / 'web/dist'
        summary['bundle'] = hashes(dist.rglob('*'), dist)
        version = re.search(r'src="([^"]+\.js)"', (dist / 'index.html').read_text())[1]
        class Handler(SimpleHTTPRequestHandler):
            def __init__(self, *a, **kw): super().__init__(*a, directory=str(dist), **kw)
            def do_GET(self):
                if self.path.startswith('/dash/'): self.path = '/index.html'
                super().do_GET()
            def log_message(self, *args): pass
        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
        browser_output = output / 'browser'
        cmd = [sys.executable, 'tests/browser_theme_capture.py', '--base-url', 'http://127.0.0.1:' + str(server.server_port),
               '--output', str(browser_output), '--source-sha', summary['head'], '--version', version]
        for theme in themes: cmd += ['--theme', theme]
        for width in summary['widths']: cmd += ['--width', str(width)]
        command('browser', cmd)
        captured = json.loads((browser_output / 'results.json').read_text())
        summary['browser'] = captured
        summary['advisories'] = advisory_rows(captured, summary['advisory_widths'])
        summary['failures'] += browser_failures(captured, expected_cases(themes, summary['widths']), browser_output, summary['advisory_widths'])
        record_overlay_bounds(summary, output, contrast.theme_tokens(contrast.read_css()))
        if hashes(dist.rglob('*'), dist) != summary['bundle']: summary['failures'].append('Built assets changed during run')
        result_code = int(bool(summary['failures']))
    except Exception as error:
        summary['failures'].append(type(error).__name__ + ': ' + str(error))
        result_code = 2
    finally:
        if server: server.shutdown(); server.server_close(); thread.join()
        changed = source_hashes() != original
        if changed:
            summary['failures'].append('Source inputs changed during run')
        if 'overlay_bounds' not in summary:
            summary['not_covered'].append('Analytical icon overlay bounds: stage not reached')
        summary['passed'] = not summary['failures']
        (output / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
        print(json.dumps({'summary': str(output / 'summary.json'), 'passed': summary['passed'], 'failures': summary['failures'], 'advisories': len(summary['advisories'])}))
        if changed: result_code = 1
    return result_code


if __name__ == '__main__':
    result = main()
    raise SystemExit(result)
