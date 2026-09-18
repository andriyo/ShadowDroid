#!/usr/bin/env python3
"""Run actual Android boundary fixtures, including repaired and independently defective cases."""
import argparse
import json
import os
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--device', required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--cli', type=Path, default=Path(__file__).resolve().parents[1] / 'cli/target/debug/shadowdroid')
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[1]
    source = repo / 'samples/shadowdroid-test-app'
    out = args.out.resolve()
    out.mkdir(parents=True, mode=0o700)
    env = dict(os.environ, SHADOWDROID_QUIET='1')
    env.pop('SHADOWDROID_SESSION', None)
    prefix = [str(args.cli.resolve()), '--device', args.device, '--authority-dir', str(out / 'authority')]
    package = 'io.github.andriyo.shadowdroid.sample'
    cases = {'outbound_intent': 'outboundIntentExtras', 'media_session': 'mediaSessionReleasedOnStop',
             'widget_update': 'widgetUpdateReachesHost', 'picture_in_picture': 'pictureInPictureEntryObserved'}
    test_class = package + '.PlatformBoundaryTest'
    install = subprocess.run(prefix + ['app', 'install', str(source / 'app/build/outputs/apk/debug/app-debug.apk')], env=env, text=True, capture_output=True, timeout=90)
    (out / 'install.json').write_text(json.dumps({'code': install.returncode, 'stdout': install.stdout, 'stderr': install.stderr}))
    assert install.returncode == 0, install.stdout
    for variant in ('correct', 'broken'):
        test = {'package': package, 'min_api': 29, 'max_api': 36,
                'argv': ['./gradlew', '--no-daemon', ':app:connectedDebugAndroidTest', '--rerun',
                         '-Pandroid.injected.androidTest.leaveApksInstalledAfterRun=true',
                         '-Pandroid.testInstrumentationRunnerArguments.class=' + test_class,
                         '-Pandroid.testInstrumentationRunnerArguments.fixtureVariant=' + variant],
                'cwd': '.', 'timeout_ms': 240000, 'reports': ['app/build/outputs/androidTest-results/connected/debug'],
                'selection': test_class, 'contracts': [{'boundary': boundary, 'class': test_class, 'test': name, 'observation': 'actual'} for boundary, name in cases.items()]}
        plan = {'schema_version': 1, 'task': 'Verify real platform boundary behavior', 'source_root': str(source),
                'requirements': [{'id': boundary, 'text': name, 'source': 'seeded boundary specification', 'checks': ['platform']} for boundary, name in cases.items()],
                'checks': [{'id': 'platform', 'adapter': {'kind': 'platform_test', 'test': test}}]}
        path = out / (variant + '.json')
        path.write_text(json.dumps(plan, indent=2))
        result = subprocess.run(prefix + ['verify', 'run', str(path), '--out', str(out / variant)], env=env, text=True, capture_output=True, timeout=300)
        (out / (variant + '-output.json')).write_text(json.dumps({'code': result.returncode, 'stdout': result.stdout, 'stderr': result.stderr}))
        assert (result.returncode == 0) == (variant == 'correct'), result.stdout
        evidence = json.loads((out / variant / 'checks/platform/result.json').read_text())['evidence']
        statuses = [contract['status'] for contract in evidence['boundaries']['contracts']]
        assert statuses == (['passed'] * 4 if variant == 'correct' else ['failed'] * 4), evidence
        print(json.dumps({'variant': variant, 'statuses': statuses}), flush=True)
    print(json.dumps({'status': 'passed', 'evidence': str(out), 'scope': 'instrumented Android boundaries on selected API/device'}))


if __name__ == '__main__':
    main()
