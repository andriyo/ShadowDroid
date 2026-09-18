#!/usr/bin/env python3
"""Verify seeded good/bad Android fixtures on an explicitly selected disposable device."""
import argparse
import json
import os
import signal
import time
from pathlib import Path
import subprocess
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--device', required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--exercise-recovery', action='store_true', help='Interrupt a run and reboot this disposable device to verify recovery')
    parser.add_argument('--cli', type=Path, default=Path(__file__).resolve().parents[1] / 'cli/target/debug/shadowdroid')
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[1]
    args.out.mkdir(parents=True, mode=0o700, exist_ok=False)
    source = args.out / 'source'
    source.mkdir()
    (source / 'fixture.txt').write_text('Seeded state, secondary route, theme and persistence contracts v1')
    env = dict(os.environ, SHADOWDROID_QUIET='1')
    env.pop('SHADOWDROID_SESSION', None)
    prefix = [str(args.cli.resolve()), '--device', args.device, '--authority-dir', str(args.out / 'authority')]
    seq = 0
    token = None

    def call(*words, failure=False):
        nonlocal seq
        argv = prefix + (['--session', token] if token else []) + list(words)
        result = subprocess.run(argv, env=env, text=True, capture_output=True, timeout=180)
        seq += 1
        record = {'argv': argv, 'code': result.returncode, 'stdout': result.stdout, 'stderr': result.stderr}
        (args.out / f'command-{seq:03}.json').write_text(json.dumps(record, indent=2))
        lines = result.stdout.strip().splitlines()
        assert len(lines) == 1, record
        value = json.loads(lines[0])
        assert (result.returncode != 0) == failure, record
        return value

    package = 'io.github.andriyo.shadowdroid.sample'
    target = lambda name: {'by': 'rid', 'value': package + ':id/verification_' + name}
    assertion = lambda name, **kw: dict(action='assert', target=target(name), **kw)
    text = 'runtime-' + uuid.uuid4().hex

    def verify(name, steps, passed, destinations=('form',), extra_checks=()):
        plan = {'schema_version': 1, 'task': name, 'inputs': ['fixture.txt'],
                'requirements': [{'id': 'requirement', 'text': name, 'source': 'seeded fixture contract', 'checks': ['journey']}],
                'checks': [{'id': 'journey', 'adapter': {'kind': 'journey', 'journey': {'package': package, 'destinations': list(destinations), 'steps': steps}}}]}
        for check in extra_checks:
            plan['checks'].append(check)
            plan['requirements'][0]['checks'].append(check['id'])
        path = source / (name + '.json')
        path.write_text(json.dumps(plan, indent=2))
        out = args.out / name
        call('verify', 'run', str(path), '--out', str(out), failure=not passed)
        report = call('verify', 'report', str(out))
        assert report['requirements_satisfied_at_run'] == passed, report
        return report

    token = call('session', 'open', '--agent', 'verification-fixture')['session']
    try:
        call('session', 'observe', '--subscription', 'fixture')
        call('--apk', str(repo / 'server/app/build/outputs/apk/androidTest/debug/app-debug-androidTest.apk'), 'connect')
        call('app', 'install', str(repo / 'samples/shadowdroid-test-app/app/build/outputs/apk/debug/app-debug.apk'))
        call('app', 'clear', package)
        start = lambda broken: {'action': 'start', 'activity': '.BrokenVerificationFixtureActivity' if broken else '.VerificationFixtureActivity'}
        form = assertion('title', text='Verification form', destination='form')
        fill = {'action': 'text', 'target': target('name'), 'value': text}
        rotation = {'action': 'configure', 'configuration': {'rotation': 1}}
        verify('correct-state', [start(False), form, fill,
            {'action': 'remember', 'target': target('instance'), 'name': 'instance'}, rotation,
            assertion('name', text=text), {'action': 'compare', 'target': target('instance'), 'memory': 'instance', 'different': True},
            {'action': 'configure', 'configuration': {'night': True, 'font_scale': 1.3}},
            assertion('theme', text='Dark'),
            {'action': 'lifecycle', 'mode': 'background_resume', 'resume_activity': '.VerificationFixtureActivity'}, assertion('name', text=text),
            {'action': 'lifecycle', 'mode': 'background_kill_restore', 'resume_activity': '.VerificationFixtureActivity'}, assertion('name', text=text),
            {'action': 'tap', 'target': target('next')}, assertion('detail', text='Secondary route ready', destination='detail'),
            {'action': 'key', 'name': 'back'}, assertion('name', text=text),
            {'action': 'capture', 'name': 'final'}], True, ('form', 'detail'))
        call('app', 'clear', package)
        verify('broken-state', [start(True), form, fill, rotation, assertion('name', text=text)], False)
        call('app', 'clear', package)
        verify('broken-theme', [start(True), form, {'action': 'configure', 'configuration': {'night': True}}, assertion('theme', text='Dark')], False)
        call('app', 'clear', package)
        report = verify('broken-secondary', [start(True), form, {'action': 'tap', 'target': target('next')}, assertion('detail', text='Secondary route ready', destination='detail')], False, ('form', 'detail'))
        assert report['check_statuses']['journey'] != 'passed'
        for broken in (False, True):
            call('app', 'clear', package)
            sql = {'id': 'database', 'depends_on': ['journey'], 'adapter': {'kind': 'sqlite', 'package': package, 'database': 'databases/verification.db',
                'query': {'sql': 'SELECT value FROM records WHERE value=?1 ORDER BY id', 'parameters': [text], 'expected_rows': [[text]]}}}
            verify('broken-database' if broken else 'correct-database', [start_activity for start_activity in [start(broken), form, fill,
                {'action': 'tap', 'target': target('save')}, assertion('saved', text='Saved: ' + text)]], not broken, extra_checks=[sql])
        call('app', 'clear', package)
        matrix = {'id': 'matrix', 'depends_on': ['journey'], 'adapter': {'kind': 'matrix', 'reset_app_data': True,
            'journey': {'package': package, 'destinations': ['form'], 'steps': [start(False), form, assertion('theme', text='Dark')]},
            'cells': [{'id': 'portrait', 'configuration': {'night': True, 'rotation': 0, 'font_scale': 1.0}},
                      {'id': 'large-landscape', 'configuration': {'night': True, 'rotation': 1, 'font_scale': 1.8}}]}}
        verify('configuration-matrix', [start(False), form], True, extra_checks=[matrix])
        if args.exercise_recovery:
            def adb(*words):
                return subprocess.run(['adb', '-s', args.device, *words], check=True, capture_output=True, text=True, timeout=15).stdout.strip()
            before = adb('shell', 'settings', 'get', 'system', 'font_scale')
            plan = {'schema_version': 1, 'task': 'interruption recovery', 'inputs': ['fixture.txt'],
                'requirements': [{'id': 'r', 'text': 'wait for a missing element', 'source': 'fixture', 'checks': ['journey']}],
                'checks': [{'id': 'journey', 'adapter': {'kind': 'journey', 'journey': {'package': package, 'destinations': [],
                    'steps': [{'action': 'configure', 'configuration': {'font_scale': 1.4}}, assertion('never-exists')]}}}]}
            path = source / 'interruption.json'; path.write_text(json.dumps(plan))
            out = args.out / 'interruption'
            worker = subprocess.Popen(prefix + ['--session', token, 'verify', 'run', str(path), '--out', str(out)], env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            deadline = time.monotonic() + 20
            journal = out / 'checks/journey/configuration-journal.json'
            while not journal.exists() and worker.poll() is None and time.monotonic() < deadline:
                time.sleep(.05)
            assert journal.exists(), 'configuration never applied'
            time.sleep(.3)
            worker.send_signal(signal.SIGINT)
            stdout, stderr = worker.communicate(timeout=20)
            (args.out / 'interruption-process.json').write_text(json.dumps({'code': worker.returncode, 'stdout': stdout, 'stderr': stderr}))
            assert worker.returncode == 130, (stdout, stderr)
            call('verify', 'recover', str(out), '--external-workers-stopped')
            assert adb('shell', 'settings', 'get', 'system', 'font_scale') == before
            adb('reboot')
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline:
                probe = subprocess.run(['adb', '-s', args.device, 'shell', 'getprop', 'sys.boot_completed'], text=True, capture_output=True, timeout=5)
                if probe.stdout.strip() == '1': break
                time.sleep(.5)
            else: raise AssertionError('device did not reboot')
            call('session', 'recover', '--external-workers-stopped')
            call('session', 'observe', '--subscription', 'after-recovery')
        print(json.dumps({'status': 'passed', 'evidence': str(args.out), 'scope': 'tool contracts, no model/benchmark uplift claim'}))
    finally:
        if token:
            call('disconnect')
            call('session', 'close')


if __name__ == '__main__':
    main()
