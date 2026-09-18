#!/usr/bin/env python3
"""Operator-owned instrumentation; only the candidate Activity is accepted input."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import xml.etree.ElementTree as ET

candidate, result, device = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3]
root = Path(__file__).resolve().parent
evaluated = result.parent / 'judged'
shutil.copytree(root / 'fixtures/state-and-route', evaluated)
relative = Path('app/src/main/java/example/verification/MainActivity.java')
source = candidate / relative
assert not source.is_symlink() and source.stat().st_size < 1024 * 1024
shutil.copy2(source, evaluated / relative)
test = evaluated / 'app/src/androidTest/java/example/verification/AcceptanceTest.java'
test.parent.mkdir(parents=True)
shutil.copy2(root / 'acceptance/AcceptanceTest.java', test)
env = dict(os.environ, ANDROID_SERIAL=device)
command = ['./gradlew', '--no-daemon', ':app:connectedDebugAndroidTest',
           '-Pandroid.testInstrumentationRunnerArguments.class=example.verification.AcceptanceTest']
execution = subprocess.run(command, cwd=evaluated, env=env)
names = {'draft': 'draftSurvivesRecreation', 'theme': 'darkThemeReflectsSystem',
         'route': 'secondaryRoute', 'regression': 'titleAndEditingRegression'}
files = list((evaluated / 'app/build/outputs/androidTest-results/connected/debug').rglob('*.xml'))
cases = [case for path in files for case in ET.parse(path).iter('testcase')]
rows = []
for requirement, name in names.items():
    matches = [case for case in cases if case.get('classname') == 'example.verification.AcceptanceTest' and case.get('name') == name]
    status = 'blocked'
    if len(matches) == 1:
        case = matches[0]
        status = 'failed' if case.find('failure') is not None or case.find('error') is not None else ('blocked' if case.find('skipped') is not None else 'passed')
    rows.append({'id': requirement, 'status': status, 'test': name})
result.write_text(json.dumps({'schema_version': 1, 'requirements': rows, 'exit_code': execution.returncode,
                             'scope': 'actual emulator instrumentation; frozen build and independent tests',
                             'reports': [str(p) for p in files]}, indent=2))
sys.exit(0 if execution.returncode == 0 and all(r['status'] == 'passed' for r in rows) else 1)
