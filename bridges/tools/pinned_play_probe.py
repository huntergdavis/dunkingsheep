#!/usr/bin/env python3
"""Private tagged-state exercise. Run --phase all through poolrad-heavy.sh.

Requires the Poolrad t116 checkpoint tools and owner media in that checkout.
Screens, state bytes and child logs stay under its ignored resources directory.
The transport uses disposable Xvfb children, never the live desk or its tmux.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import subprocess
import sys
import time
import zipfile

DESK = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(DESK))
from server.emulator_build import describe_emulator, pinned_binary
from server.gameplay import GameSession

TAG = "t140 pinned build new snapshot"


def dump(path, data):
    path.write_text(json.dumps(data, indent=2) + "\n")


def checkpoint(driver, final):
    prefix = driver.run / 'checkpoint'
    prefix.with_suffix('.arm').touch()
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        try:
            data = json.loads(prefix.with_suffix('.json').read_text())
            state = Path(f'/proc/{data["pid"]}/stat').read_text().rsplit(')', 1)[1].split()[0]
            if state == 'T':
                break
        except (FileNotFoundError, json.JSONDecodeError):
            pass
        time.sleep(.05)
    else:
        raise RuntimeError('Verified checkpoint arrival did not stop within 30 seconds')
    if not final:
        for extension in ('.json', '.ram', '.vram'):
            prefix.with_suffix(extension).rename(driver.run / ('post-load-baseline' + extension))
        os.killpg(driver.emu.pid, signal.SIGCONT)
        return data
    driver.command(['import', '-window', driver.window, str(driver.run / 'checkpoint.png')])
    rgb = driver.command(['convert', str(driver.run / 'checkpoint.png'), '-alpha', 'off',
                          '-depth', '8', 'rgb:-'], capture_output=True).stdout
    (driver.run / 'checkpoint.rgb').write_bytes(rgb)
    width, height = driver.command(['identify', '-format', '%w %h', str(driver.run / 'checkpoint.png')],
                                   capture_output=True, text=True).stdout.split()
    data['case'] = 'new-tag-restored-then-right'
    data['frame'] = dict(format='RGB24', width=int(width), height=int(height),
                         sha256=hashlib.sha256(rgb).hexdigest(),
                         scope='last presented window while stopped; no new-frame fence')
    dump(prefix.with_suffix('.json'), data)
    os.killpg(driver.emu.pid, signal.SIGTERM)
    os.killpg(driver.emu.pid, signal.SIGCONT)
    driver.emu.wait(timeout=20)
    return data


def run_phase(args):
    from r1_driver import Driver
    repo = args.poolrad_repo
    binary = '/usr/bin/dosbox-x' if args.system_control else pinned_binary()
    identity = describe_emulator(binary)
    if args.system_control:
        if identity['sha256'] != '99d7460153175d13a21036fb4b0e7524e05b6a1e50cd4e59f2470afad12a0df3':
            raise RuntimeError('System control executable changed from the recorded hash')
    elif identity['kind'] != 'checkpoint':
        raise RuntimeError('Installed executable does not match the pinned binary hash')
    for key in list(os.environ):
        if key.startswith(('PR_HYBRID_', 'PR_PARITY_')):
            del os.environ[key]
    os.environ.update(VP_DOSBOX_X=binary, VP_REPLACEMENT_DOSBOX_X=binary,
                      VP_PLAY_BUILD='replacement' if args.phase == 'replacement' else 'original',
                      VP_REPLACEMENT_REGISTRY=str(repo / 'resources/hybrid/registry_all.tsv'),
                      VP_REPLACEMENT_DISPATCH=str(repo / 'resources/hybrid/play-dispatch.txt'),
                      COMBAT_IDLE_S='900')
    name = args.prefix + '-' + args.phase
    run = Path(subprocess.check_output(['tools/oracle/prepare_run.sh', name], text=True).strip())
    (run / 'vp-dosbox-terminal.log').symlink_to('launch.log')
    bindir = run / 'bin'
    bindir.mkdir()
    wrapper = bindir / 'dosbox-x'
    wrapper.write_text('#!/bin/sh\nexec ' + shlex.quote(binary) +
                       ' "$@" -set "dosbox saveslot=1" -set "dosbox saveremark=false"\n')
    wrapper.chmod(0o700)
    os.environ['PATH'] = str(bindir) + os.pathsep + os.environ['PATH']
    session = GameSession(repo, run / 'desk-data')
    session.run_dir = run
    session.display = os.environ['DISPLAY']
    session._env = lambda: os.environ.copy()
    producer = repo / 'resources/oracle-runs' / ((args.source_prefix or args.prefix) + '-producer')
    if args.phase != 'producer':
        evidence = json.loads((producer / 'producer-evidence.json').read_text())
        entry = evidence['entry']
        if entry['tag'] != TAG or entry['slot'] != 1:
            raise RuntimeError('Expected the new probe-produced tagged snapshot')
        source = producer / '.dosbox-saves/1.sav'
        if hashlib.sha256(source.read_bytes()).hexdigest() != entry['sha256']:
            raise RuntimeError('New tagged snapshot changed before restore')
        shutil.copyfile(source, run / '.dosbox-saves/1.sav')
        session._write_states(run, [entry])
        with (run / '.oracle-input-sha256').open('a') as out:
            out.write(entry['sha256'] + '  ./.dosbox-saves/1.sav\n')
        os.environ.update(session._play_environment(run))
        shutil.copyfile(repo / 'resources/hybrid/registry_all.tsv', run / 'registry.tsv')
        os.environ.update(PR_HYBRID_REGISTRY=str(run / 'registry.tsv'),
                          PR_HYBRID_LOG=str(run / 'observer'), PR_PARITY_TARGET='RES_0A40_02FD',
                          PR_PARITY_PREFIX=str(run / 'checkpoint'))
        (run / 'parity-mode.txt').write_text(args.phase + '\n')
        (run / 'parity-dispatch.txt').write_text(os.getenv('PR_HYBRID_DISPATCH', '') + '\n')
        (run / 'parity-command.txt').write_text('pinned_play_probe.py --phase ' + args.phase +
                                                ' --prefix ' + args.prefix + '\n')
        (run / 'parity-build-sha256').write_text(
            hashlib.sha256(Path(binary).read_bytes()).hexdigest() + '  stable/dosbox-x\n' +
            hashlib.sha256(Path(__file__).read_bytes()).hexdigest() + '  tools/pinned_play_probe.py\n')

    class ProbeDriver(Driver):
        def __init__(self):
            route = dict(version=2, name='pinned-play', start='final', terminal='final', font_text=True,
                         navigation_states={'at15_4': [15, 4], 'final': [15, 4]},
                         states=[dict(name='final', gate=['15,4'], source='digits', key=None,
                                      expect_next=[], timeout_s=900, max_visits=1)])
            super().__init__(run, route, 3600)
            for record in self.route.records.values():
                record['timeout_s'] = 900
            self.route.records['at15_4'].update(key='Right', expect_next=['final'])
            self.last_gate = None
            self.loaded = False
            session._game_running = lambda: self.emu is not None and self.emu.poll() is None
            session._focus_game = lambda: self.command(['xdotool', 'windowfocus', '--sync', self.window])
            # The private pipe transport has no tmux terminal startup marker.
            # R1 has already observed the game's code prompt twice before load.
            session._wait_for_game_start = lambda timeout=20: self.last_gate == 'code'

        def log(self, event, **fields):
            super().log(event, **fields)
            if event == 'gate':
                self.last_gate = fields['name']
            if event == 'final':
                if args.phase == 'producer':
                    entry = session.savestate(TAG)
                    path = run / '.dosbox-saves' / (str(entry['slot']) + '.sav')
                    with zipfile.ZipFile(path) as archive:
                        if archive.testzip() is not None:
                            raise RuntimeError('Fresh snapshot ZIP integrity failed')
                    names = [base + suffix for base in ('observer', 'pr_hybrid_observer')
                             for suffix in ('.tsv', '.seq', '.dispatch', '.draws')]
                    outputs = [name for name in names if (run / name).exists()]
                    enabled = any(k.startswith(('PR_HYBRID_', 'PR_PARITY_')) for k in os.environ)
                    dump(run / 'producer-evidence.json', dict(entry=entry,
                         emulator=describe_emulator(binary), pr_environment_present=enabled,
                         observer_outputs=outputs, zip_crc_pass=True))
                    if outputs or enabled:
                        raise RuntimeError('Producer did not satisfy the inactive-hook setup')
                elif args.restore_control:
                    if not self.loaded:
                        raise RuntimeError('Control reached final gate without native restore')
                    dump(run / 'restore-route-evidence.json', dict(
                        restored_then_right_reached=True, frame_sha256=self.final_pixels_sha256))
                else:
                    checkpoint(self, final=True)

        def key(self, key):
            if args.phase != 'producer' and self.last_gate == 'code' and key == 'Return' and not self.loaded:
                result = session.loadstate(1)
                dump(run / 'load-evidence.json', result)
                if not result['verified']:
                    raise RuntimeError('Fresh snapshot load was not confirmed by the native loader')
                self.loaded = True
                self.route.allowed = ['at15_4']
                return
            if (not args.restore_control and args.phase != 'producer' and self.loaded
                    and self.last_gate == 'at15_4' and key == 'Right'):
                checkpoint(self, final=False)
            return super().key(key)

    driver = ProbeDriver()
    try:
        driver.run_route()
    finally:
        if driver.emu and driver.emu.poll() is None:
            os.killpg(driver.emu.pid, signal.SIGCONT)
        driver.close()


def add_phase_evidence(result, runs):
    """Publish only metadata from this exercise's new states and private logs."""
    for phase, run in runs.items():
        events = run / 'r1-events.jsonl'
        if phase not in result['phases'] or not events.exists():
            continue
        rows = [json.loads(line) for line in events.read_text().splitlines()]
        exits = [row for row in rows if row['event'] == 'shutdown']
        if exits:
            result['phases'][phase]['emulator_exit_code'] = exits[-1]['launch_rc']
            result['phases'][phase]['shutdown_method'] = exits[-1]['method']
        detail = result['phases'][phase]
        if phase != 'producer':
            loaded = run / 'load-evidence.json'
            confirmed = loaded.exists() and json.loads(loaded.read_text())['verified']
            detail['restored_route_completed'] = bool(confirmed and any(row['event'] == 'final' for row in rows))
        else:
            detail.pop('restored_route_completed', None)
    evidence = runs['producer'] / 'producer-evidence.json'
    if evidence.exists():
        producer = json.loads(evidence.read_text())
        entry = producer['entry']
        result.update(emulator=producer['emulator'], tagged_snapshot=dict(
            tag=entry['tag'], slot=entry['slot'], size=entry['size'], sha256=entry['sha256'],
            zip_crc_pass=producer['zip_crc_pass']),
            producer_pr_environment_present=producer['pr_environment_present'],
            producer_observer_outputs=producer['observer_outputs'])
    return result


def run_all(args):
    from parity_report import report
    logs = args.poolrad_repo / 'resources/pinned-play-probes' / args.prefix
    logs.mkdir(parents=True, exist_ok=False)
    names = (('original',) if args.source_prefix else ('producer', 'original')) if args.restore_control else ('producer', 'original', 'replacement')
    runs = {phase: args.poolrad_repo / 'resources/oracle-runs' / (args.prefix + '-' + phase)
            for phase in names}
    if args.source_prefix:
        runs['producer'] = args.poolrad_repo / 'resources/oracle-runs' / (args.source_prefix + '-producer')
    phases = {}
    for phase in names:
        with (logs / (phase + '.log')).open('wb') as out:
            completed = subprocess.run(['xvfb-run', '-a', '-s', '-screen 0 1024x768x24',
                                        sys.executable, str(Path(__file__).resolve()),
                                        '--poolrad-repo', str(args.poolrad_repo),
                                        '--prefix', args.prefix, '--phase', phase,
                                        *(['--system-control'] if args.system_control else []),
                                        *(['--pinned-control'] if args.pinned_control else []),
                                        *(['--source-prefix', args.source_prefix] if args.source_prefix else [])],
                                       stdout=out, stderr=subprocess.STDOUT)
        phases[phase] = {'exit_code': completed.returncode}
        load = runs[phase] / 'load-evidence.json'
        if load.is_file():
            phases[phase]['load_verified'] = json.loads(load.read_text())['verified']
        if phase == 'producer' and completed.returncode:
            break
    result = dict(schema=1, case='new-tag-restored-then-right', result='FAIL',
                  comparison_performed=False, phases=phases,
                  not_verified=['Native exercise incomplete; no frame/register/VGA/RAM comparison established.',
                                'Live desk transport/audio/video, old-state migration, full-game parity or replacement credit.'])
    if args.restore_control:
        result.update(case=('system' if args.system_control else 'pinned') + '-new-tag-restore-control',
                      snapshot_reused_from_prior_exercise=bool(args.source_prefix),
                      consumer_emulator=describe_emulator('/usr/bin/dosbox-x' if args.system_control else pinned_binary()),
                      verdict_scope='Native restore survival through the Right gate; not a parity comparison.')
        if len(phases) == len(names) and all(p['exit_code'] == 0 for p in phases.values()):
            producer = json.loads((runs['producer'] / 'producer-evidence.json').read_text())
            entry = producer['entry']
            loaded = json.loads((runs['original'] / 'load-evidence.json').read_text())
            route = json.loads((runs['original'] / 'restore-route-evidence.json').read_text())
            copied = runs['original'] / '.dosbox-saves/1.sav'
            if (not loaded['verified'] or loaded['sha256'] != entry['sha256'] or loaded['tag'] != TAG
                    or hashlib.sha256(copied.read_bytes()).hexdigest() != entry['sha256']
                    or not route['restored_then_right_reached']):
                raise RuntimeError('Control did not establish the same new snapshot and restored route')
            result.update(result='PASS', not_verified=[
                'Frame/register/VGA/RAM parity; this control does not capture checkpoints.',
                'Live desk transport/audio/video, old-state migration or reliability beyond this single restore.'])
    if len(phases) == 3 and all(p['exit_code'] == 0 for p in phases.values()):
        producer = json.loads((runs['producer'] / 'producer-evidence.json').read_text())
        entry = producer['entry']
        for phase in ('original', 'replacement'):
            loaded = json.loads((runs[phase] / 'load-evidence.json').read_text())
            if not loaded['verified'] or loaded['sha256'] != entry['sha256'] or loaded['tag'] != TAG:
                raise RuntimeError('Cases did not verify the same new tagged snapshot')
            if hashlib.sha256((runs[phase] / '.dosbox-saves/1.sav').read_bytes()).hexdigest() != entry['sha256']:
                raise RuntimeError('Snapshot copy changed during exercise')
        result = report(runs['original'], runs['replacement'])
        result['dos_save_file_id'] = result['save_state_id']
        result['save_state_id'] = entry['sha256']
        result.update(comparison_performed=True, phases=phases, emulator=producer['emulator'],
                      private_startup_guard='R1 code-prompt gate; not the live desk terminal marker',
                      tagged_snapshot=dict(tag=TAG, slot=entry['slot'], size=entry['size'],
                                           sha256=entry['sha256'], zip_crc_pass=producer['zip_crc_pass']),
                      producer_pr_environment_present=producer['pr_environment_present'],
                      producer_observer_outputs=producer['observer_outputs'])
        coverage = {}
        for phase in ('original', 'replacement'):
            before = json.loads((runs[phase] / 'post-load-baseline.json').read_text())
            after = json.loads((runs[phase] / 'checkpoint.json').read_text())
            coverage[phase] = dict(routed_returns=after['routed_returns'] - before['routed_returns'],
                                   routed_stops=after['routed_stops'] - before['routed_stops'])
        result['post_load_right_coverage'] = coverage
        if coverage['original']['routed_returns'] != 0 or coverage['replacement']['routed_returns'] <= 0:
            result['result'] = 'FAIL'
            result['coverage_failure'] = 'No valid original/replacement C-return contrast after verified load'
        result['not_verified'][0] = 'Complete machine equality after restore: the receipt compares only the listed captured state.'
        result['not_verified'].extend([
            'Live desk tmux, web UI or audio: the actual save/load methods used disposable private Xvfb transport.',
            'The 14 old owner states: none were read, edited, loaded, archived or migrated by this exercise.',
            'A clean rebuild of all emulator objects; unchanged base objects were reused.'
        ])
    add_phase_evidence(result, runs)
    output = args.output or logs / 'receipt.json'
    dump(output, result)
    print(json.dumps(dict(result=result['result'], comparison_performed=result['comparison_performed'],
                          receipt=str(output))))
    return 0 if result['result'] == 'PASS' else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--poolrad-repo', type=Path, required=True)
    parser.add_argument('--prefix', required=True)
    parser.add_argument('--phase', choices=['all', 'producer', 'original', 'replacement'], default='all')
    parser.add_argument('--output', type=Path)
    controls = parser.add_mutually_exclusive_group()
    controls.add_argument('--system-control', action='store_true',
                        help='Use the hash-pinned system binary for a producer/original restore-survival control')
    controls.add_argument('--pinned-control', action='store_true',
                          help='Use the pinned binary for a restore-survival control without checkpoint capture')
    parser.add_argument('--source-prefix', help='Reuse this prior probe producer snapshot without editing it')
    args = parser.parse_args()
    args.restore_control = args.system_control or args.pinned_control
    if args.restore_control and args.phase == 'replacement':
        parser.error('Restore controls have no replacement phase')
    if args.source_prefix and (not args.restore_control or args.phase == 'producer'):
        parser.error('A prior producer may only be reused by a restore control')
    if args.source_prefix and not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._-]*', args.source_prefix):
        parser.error('Invalid source producer prefix')
    if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._-]*', args.prefix):
        parser.error('Invalid private run prefix')
    args.poolrad_repo = args.poolrad_repo.resolve()
    if args.output:
        args.output = args.output.resolve()
        if args.output.exists():
            parser.error('Output already exists; preserve it and choose a new receipt path')
    os.umask(0o077)
    sys.path.insert(0, str(args.poolrad_repo / 'tools/hybrid'))
    os.chdir(args.poolrad_repo)
    if args.phase != 'all':
        run_phase(args)
        return 0
    logs = args.poolrad_repo / 'resources/pinned-play-probes' / args.prefix
    if logs.exists():
        parser.error('Probe prefix already exists; use a new prefix')
    try:
        return run_all(args)
    except Exception as exc:
        output = args.output or logs / 'receipt.json'
        output.parent.mkdir(parents=True, exist_ok=True)
        dump(output, dict(schema=1, case='new-tag-restored-then-right', result='FAIL',
                          comparison_performed=False, validation_error_type=type(exc).__name__,
                          not_verified=['Native exercise or capture validation did not complete.',
                                        'No frame/register/VGA/RAM parity conclusion is established.']))
        print(json.dumps(dict(result='FAIL', comparison_performed=False, receipt=str(output))))
        return 2


if __name__ == '__main__':
    raise SystemExit(main())
