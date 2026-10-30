#!/usr/bin/env python3
"""Compare writer input, raw SQLite bytes, API bodies, and rendered message text.

Run against an isolated Vite build, through the shared heavy-job wrapper. The
browser uses this process's temporary server and synthetic APIs exclusively.
Reuses the storage/DOM boundaries established by the t66 PR78 byte receipts.
"""
import argparse
from contextlib import ExitStack
import functools
import hashlib
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import importlib
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile
from threading import Thread
from types import SimpleNamespace
from unittest.mock import patch
from urllib.parse import urlparse

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
FIXTURE = 'Cost $901, **bold** and [brackets]. One paragraph stays one paragraph.'
EMPHASIS_TEXT = FIXTURE.replace('**', '')


class FidelityError(AssertionError):
    pass


def same_bytes(actual, expected, boundary):
    if actual.encode('utf-8') != expected.encode('utf-8'):
        raise FidelityError(f'{boundary}: UTF-8 bytes changed')


def rendering_table(stored, visible, paragraphs, strong_texts, hard_breaks):
    """Four adjudicable assertions for the unescaped agent Markdown fixture.

    Composer escaping has its own input-restoration check. Do not apply a global
    "no backslashes" rule to arbitrary Markdown or compare SQL and DOM for equality.
    """
    rules = [
        ('literal_brackets', 'Unescaped stored brackets stay visible without an added backslash',
         '[brackets]' in stored and '\\[' not in stored and '\\]' not in stored and
         '[brackets]' in visible and '\\[' not in visible and '\\]' not in visible),
        ('dollar_amount', 'The literal dollar amount remains exactly $901',
         re.findall(r'\$\d+(?:\.\d+)?', stored) == ['$901'] and
         re.findall(r'\$\d+(?:\.\d+)?', visible) == ['$901']),
        ('paragraph_structure', 'One paragraph with no inserted hard break; viewport wrapping is allowed',
         len(paragraphs) == 1 and paragraphs[0] == visible and '\n' not in visible and hard_breaks == 0),
        ('emphasis_control', 'Markdown emphasis becomes a strong element; this byte difference must PASS',
         '**bold**' in stored and '**bold**' not in visible and strong_texts == ['bold'] and stored != visible),
    ]
    return [dict(assertion=name, expected=expected, status='PASS' if passed else 'FAIL',
                 stored_text=stored, stored_utf8_hex=stored.encode('utf-8').hex(),
                 dom_text=visible, dom_utf8_hex=visible.encode('utf-8').hex())
            for name, expected, passed in rules]


class StorageProbe:
    """Actual application writers, isolated SQLite, and synthetic transport."""
    def __init__(self, legacy_zero_attempt=False):
        self.legacy_zero_attempt = legacy_zero_attempt

    def __enter__(self):
        self.stack = ExitStack()
        try:
            folder = Path(self.stack.enter_context(tempfile.TemporaryDirectory()))
            self.server = importlib.import_module('server.app')
            # Authentication is outside this probe; never read or create a real credential.
            auth = folder / 'auth.json'
            auth.write_text('{}')
            for name, value in [('DB', folder / 'messages.sqlite3'), ('AUTH', auth),
                                ('CREDENTIALS', folder / 'credentials')]:
                self.stack.enter_context(patch.object(self.server, name, value))
            self.server._initialize()
            self.records = {}
            self.ready = False
            self.active = None
            self.stack.enter_context(patch.object(self.server, '_panes', return_value=[{
                'pane_id': '%2', 'agent': 'pi',
                'tab_label': self.server.CHANNEL_INDEX['cos']['match']}]))
            self.stack.enter_context(patch.object(self.server, '_send_to_pane', side_effect=self._send))
            self.stack.enter_context(patch.object(self.server, '_flock',
                return_value=SimpleNamespace(call=self._receipts)))
            return self
        except BaseException:
            self.stack.close()
            raise

    def __exit__(self, *args):
        return self.stack.__exit__(*args)

    def raw(self, message_id):
        with self.server._db() as conn:
            row = conn.execute(
                'SELECT id,role,body,hex(CAST(body AS BLOB)) AS body_hex,delivery,'
                'transport_attempt_at,delivery_detail,delivered_at,delivery_latency_s '
                'FROM messages WHERE id=?', (message_id,)).fetchone()
        result = dict(row)
        # SQLite hex() is uppercase; normalize presentation, not the stored bytes.
        result['body_hex'] = result['body_hex'].lower()
        return result

    def _record_raw(self, record, boundary, message_id):
        raw = self.raw(message_id)
        record[boundary] = raw
        same_bytes(raw['body'], record['input_body'], boundary)
        if raw['body_hex'] != record['input_utf8_hex']:
            raise FidelityError(f'{boundary}: SQLite hex differs from writer input')
        return raw

    def _send(self, pane, prompt):
        with self.server._db() as conn:
            message_id = conn.execute('SELECT max(id) FROM messages').fetchone()[0]
        record = self.records[self.active]
        raw = self._record_raw(record, 'raw_before_transport', message_id)
        if raw['role'] != 'owner' or raw['delivery'] != 'pending' or raw['transport_attempt_at'] != 100:
            raise FidelityError('owner attempt was not persisted before transport')
        if not prompt.startswith(record['input_body'] + '\n\n[VP web message #'):
            raise FidelityError('transport prompt changed the message body')
        return dict(outcome='stored', push_id=message_id, pane=pane,
                    route='owner_push', detail='synthetic push stored')

    def _receipts(self, command, **args):
        if command != 'owner_receipts':
            raise FidelityError(f'unexpected transport operation: {command}')
        return {'receipts': [dict(id=i, pane_id='%2', created_at=100.5,
                                 taken_at=102.25 if self.ready else None)
                             for i in args['push_ids']]}

    def _new(self, name, body):
        record = dict(input_body=body, input_utf8_hex=body.encode('utf-8').hex())
        self.records[name] = record
        return record

    def owner(self, body, name='owner_plain_api'):
        record = self._new(name, body)
        self.active = name
        with patch.object(self.server.time, 'time', return_value=100):
            item = self.server.post_message(self.server.MessageIn(channel='cos', body=body), 'owner')
        record['id'] = item['id']
        if self.legacy_zero_attempt:
            # Explicit fixture for a pre-timing-schema owner row, not a real send.
            with self.server._db() as conn:
                conn.execute('UPDATE messages SET transport_attempt_at=0 WHERE id=?', (item['id'],))
            record['legacy_fixture_attempt_reset'] = True
        self._record_raw(record, 'raw_after_push', item['id'])
        same_bytes(item['body'], body, 'owner API after push')
        if item['delivery'] != 'stored':
            raise FidelityError('synthetic owner push did not reach stored state')
        return item

    def agent(self, body=FIXTURE):
        record = self._new('agent_brief', body)
        item = self.server.post_brief(self.server.BriefIn(channel='cos', body=body), 'api')
        record['id'] = item['id']
        raw = self._record_raw(record, 'raw_after_write', item['id'])
        same_bytes(item['body'], body, 'agent API')
        if raw['delivery'] != 'posted' or raw['transport_attempt_at'] != 0:
            raise FidelityError('agent brief acquired owner transport state')
        return item

    def messages(self):
        return self.server.messages('cos', 'fixture')

    def finish(self):
        self.ready = True
        items = {item['id']: item for item in self.messages()['items']}
        for name, record in self.records.items():
            item = items[record['id']]
            same_bytes(item['body'], record['input_body'], 'API after reconciliation')
            record.update(api_body=item['body'], api_utf8_hex=item['body'].encode('utf-8').hex())
            if name.startswith('owner_'):
                raw = self._record_raw(record, 'raw_after_pickup', item['id'])
                expected_latency = None if self.legacy_zero_attempt else 2.25
                if raw['delivery'] != 'delivered' or raw['delivered_at'] != 102.25 or raw['delivery_latency_s'] != expected_latency:
                    raise FidelityError('pickup timestamp or elapsed time differs from the fixture expectation')
        return self.records


def browser_probe(dist, output, chromium=None, legacy_zero_attempt=False):
    from playwright.sync_api import sync_playwright
    dist, output = Path(dist).resolve(), Path(output).resolve()
    if not (dist / 'index.html').is_file():
        raise ValueError('--dist must contain an isolated Vite build')
    output.mkdir(parents=True, exist_ok=True)
    evidence = {
        'status': 'RUNNING', 'fixture': FIXTURE, 'cases': [],
        'hex_encoding': 'lowercase UTF-8; raw SQLite hex normalized for comparison',
        'scope': 'actual Python writers in temporary SQLite; isolated static server; synthetic browser APIs and receipt transport',
        'legacy_zero_attempt_fixture': legacy_zero_attempt,
        'not_verified': ['live owner storage/delivery/rendering', 'authentication or deployed RPC',
                         'other browsers/devices', 'full accessibility'],
        'source_head': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
        'source_sha256': {name: hashlib.sha256((ROOT / name).read_bytes()).hexdigest() for name in
                          ['server/app.py', 'web/src/main.jsx', 'web/src/delivery-label.js', 'tools/raw_storage_probe.py']},
        'built_files_sha256': {str(p.relative_to(dist)): hashlib.sha256(p.read_bytes()).hexdigest()
                               for p in sorted(dist.rglob('*')) if p.is_file()},
    }
    class Handler(SimpleHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            if self.path.startswith('/dash/'):
                self.path = '/index.html'
            super().do_GET()

    server = ThreadingHTTPServer(('127.0.0.1', 0), functools.partial(Handler, directory=str(dist)))
    Thread(target=server.serve_forever, daemon=True).start()
    origin = f'http://127.0.0.1:{server.server_port}'
    try:
        chrome = Path(chromium) if chromium else sorted(
            (Path.home() / '.cache/ms-playwright').glob('chromium-*/chrome-linux64/chrome'))[-1]
        with sync_playwright() as p:
            browser = p.chromium.launch(executable_path=str(chrome), args=['--no-sandbox'])
            evidence['browser'] = browser.version
            for width, height in [(1440, 900), (390, 844)]:
                with StorageProbe(legacy_zero_attempt) as storage:
                    storage.agent()
                    storage.owner(FIXTURE)
                    context = browser.new_context(viewport=dict(width=width, height=height),
                                                  device_scale_factor=1, reduced_motion='reduce',
                                                  service_workers='block')
                    page = context.new_page()
                    errors = []
                    page.on('pageerror', lambda error: errors.append(str(error)))

                    def route(r):
                        path = urlparse(r.request.url).path
                        if not r.request.url.startswith(origin + '/'):
                            r.abort(); return
                        if not path.startswith('/api/'):
                            r.continue_(); return
                        if path == '/api/viewport' and r.request.method == 'POST':
                            r.fulfill(status=204); return
                        if path == '/api/channels' and r.request.method == 'GET':
                            r.fulfill(json={'channels': [dict(id='cos', name='Synthetic CoS', pane_id='%2',
                                                             status='idle', can_message=True)]}); return
                        if path == '/api/messages' and r.request.method == 'GET':
                            r.fulfill(json=storage.messages()); return
                        if path == '/api/pane-send' and r.request.method == 'POST':
                            r.fulfill(json=storage.owner(r.request.post_data_json['text'], 'owner_composer_wire')); return
                        r.fulfill(status=404, json={})

                    page.route('**/*', route)
                    page.goto(origin + '/dash/cos', wait_until='networkidle')
                    page.locator('#message-1 .markdown').wait_for()
                    editor = page.get_by_label('Message text', exact=True)
                    editor.fill(FIXTURE)
                    same_bytes(editor.text_content(), FIXTURE, 'editor visible input')
                    page.get_by_role('button', name='Send message', exact=True).click()
                    page.locator('#message-3 .markdown').wait_for()
                    records = storage.finish()
                    case = dict(viewport=[width, height], records=records)
                    evidence['cases'].append(case)
                    expected_label = 'delivered · timing unavailable' if legacy_zero_attempt else 'delivered in 2.3s'
                    page.wait_for_function("label => document.querySelector('#message-3 .delivery')?.textContent===label", arg=expected_label)
                    if legacy_zero_attempt:
                        page.get_by_text('Last delivered · timing unavailable', exact=True).wait_for()
                    for name, record in records.items():
                        expected = FIXTURE if name == 'owner_composer_wire' else EMPHASIS_TEXT
                        node = page.locator(f"#message-{record['id']} .markdown")
                        actual = node.text_content()
                        if name == 'agent_brief':
                            case['rendering_table'] = rendering_table(
                                record['raw_after_write']['body'], actual,
                                node.locator('p').all_text_contents(),
                                node.locator('strong').all_text_contents(), node.locator('br').count())
                            failed = [row['assertion'] for row in case['rendering_table'] if row['status'] != 'PASS']
                            if failed:
                                raise FidelityError('rendering assertions failed: ' + ', '.join(failed))
                        same_bytes(actual, expected, name + ' visible DOM')
                        if node.locator('p').count() != 1:
                            raise FidelityError(name + ': paragraph structure changed')
                        record.update(dom_text=actual, dom_utf8_hex=actual.encode('utf-8').hex(),
                                      expected_dom_text=expected, expected_dom_utf8_hex=expected.encode('utf-8').hex())
                    if page.locator('#message-1 .delivery').count():
                        raise FidelityError('agent reply displays owner delivery timing')
                    if errors or page.locator('[role=dialog],dialog[open],.modal-backdrop').count():
                        raise FidelityError('page errors or open dialog invalidate capture')
                    screenshot = f'fidelity-{width}x{height}.png'
                    page.screenshot(path=str(output / screenshot), full_page=True)
                    case.update(screenshot=screenshot,
                                screenshot_sha256=hashlib.sha256((output / screenshot).read_bytes()).hexdigest())
                    context.close()
            browser.close()
        evidence['status'] = 'PASS'
    except Exception as exc:
        evidence['status'] = 'FAIL'
        evidence['error'] = str(exc)
        raise
    finally:
        server.shutdown()
        server.server_close()
        (output / 'receipt.json').write_text(json.dumps(evidence, indent=2) + '\n')
        table = ['| Viewport | Assertion | Expected | Stored UTF-8 hex | DOM UTF-8 hex | Result |',
                 '| --- | --- | --- | --- | --- | --- |']
        for case in evidence['cases']:
            for row in case.get('rendering_table', []):
                cells = ['x'.join(map(str, case['viewport'])), row['assertion'], row['expected'],
                         row['stored_utf8_hex'], row['dom_utf8_hex'], row['status']]
                table.append('| ' + ' | '.join(cells) + ' |')
        (output / 'rendering-table.md').write_text('\n'.join(table) + '\n')
    return evidence


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--dist', required=True, type=Path, help='isolated prebuilt Vite directory')
    parser.add_argument('--output', required=True, type=Path, help='ignored/scratch receipt and screenshot directory')
    parser.add_argument('--chromium', type=Path)
    parser.add_argument('--legacy-zero-attempt', action='store_true', help='simulate legacy owner rows with unknown send time; require pickup with no duration')
    args = parser.parse_args()
    result = browser_probe(args.dist, args.output, args.chromium, args.legacy_zero_attempt)
    print(json.dumps(dict(status=result['status'], viewports=len(result['cases']),
                         receipt=str(args.output / 'receipt.json'), not_verified=result['not_verified'])))


if __name__ == '__main__':
    main()
