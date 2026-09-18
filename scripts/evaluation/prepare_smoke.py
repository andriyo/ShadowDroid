#!/usr/bin/env python3
"""Freeze the small A/B smoke design before looking at any agent outcome."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--out', type=Path, required=True)
p.add_argument('--cli', type=Path, required=True)
p.add_argument('--device', required=True)
a = p.parse_args()
root = Path(__file__).resolve().parent
repo = root.parents[1]
a.out = a.out.resolve()
a.out.mkdir(parents=True, mode=0o700)
# Snapshot all operator inputs, so later repository edits cannot change the experiment.
shutil.copytree(root / 'fixtures', a.out / 'fixtures')
shutil.copytree(root / 'acceptance', a.out / 'acceptance')
shutil.copy2(root / 'evaluate_android.py', a.out / 'evaluate_android.py')
shutil.copy2(a.cli.resolve(), a.out / 'shadowdroid')
old = subprocess.run(['git', 'show', 'add874bd004e313014889a420abcba555b0df070:cli/src/cmd/skill_body.md'], cwd=repo, capture_output=True, text=True, check=True).stdout
(a.out / 'baseline-guide.md').write_text(old)
(a.out / 'improved-guide.md').write_text(old + '\n\n' + (repo / 'cli/src/cmd/introspect/guides/verification.md').read_text())
prompt = '''Fix app/src/main/java/example/verification/MainActivity.java in this small Android app. Requirements: (draft) arbitrary entered draft text survives activity recreation; (theme) the theme label reflects the actual system night setting, Dark or Light; (route) Details opens the existing DetailActivity and its Details ready text; (regression) preserve the Draft editor heading and editable input. Keep all resource IDs, component names and public signatures. Only this Java source file is submitted for independent acceptance; you may create your own local tests and notes, but do not change build configuration, manifest or resources. Do not read any acceptance evaluator or other attempts. Use the available shadowdroid binary if useful, and read SHADOWDROID_GUIDE.md. The assigned disposable device is ''' + a.device + '''. Only this device is authorized. Do not reboot it. Before finishing, release any ShadowDroid session you opened. Do not spawn agents or commit changes. Independent instrumentation runs after your turn. Use the time budget for a focused fix and verification; do not claim tests ran unless they did.'''
config = {'schema_version': 1, 'purpose': 'Two-minute workflow smoke; no effect-size inference',
          'seed': 20260918, 'attempts_per_variant': 1, 'budget_seconds': 120, 'evaluation_seconds': 180,
          'rerun_policy': 'never', 'device': a.device, 'model': 'Codex CLI default; exact server revision not exposed',
          'runner': ['codex', '-a', 'never', 'exec', '--ignore-user-config', '--ephemeral', '--sandbox', 'workspace-write', '--json', '-C', '{workspace}', '-o', '{final}', '-'],
          'handoff': [str(a.out / 'shadowdroid'), '--device', '{device}', 'disconnect'],
          'variants': [{'id': 'A', 'binary': 'shadowdroid', 'guidance': 'baseline-guide.md'}, {'id': 'B', 'binary': 'shadowdroid', 'guidance': 'improved-guide.md'}],
          'tasks': [{'id': 'state-route', 'template': 'fixtures/state-and-route', 'prompt': prompt,
                     'requirement_ids': ['draft', 'theme', 'route', 'regression'],
                     'evaluator_inputs': ['evaluate_android.py', 'acceptance/AcceptanceTest.java'],
                     'evaluator': ['python3', '{experiment_dir}/evaluate_android.py', '{workspace}', '{result}', '{device}']}]}
(a.out / 'smoke.json').write_text(json.dumps(config, indent=2))
print(a.out / 'smoke.json')
