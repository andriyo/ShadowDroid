#!/usr/bin/env python3
"""Frozen, sequential coding-agent experiments with operator-owned acceptance.

Agent and evaluator separation is procedural on a local host, not a security
sandbox. Use separate containers/mounts for genuinely hidden holdout evaluation.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import resource
import shutil
import signal
import subprocess
import time


LIMIT = 64 * 1024 * 1024


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def write(path, value):
    temporary = path.with_suffix(path.suffix + '.tmp')
    temporary.write_text(json.dumps(value, indent=2) + '\n')
    temporary.replace(path)


def inventory(root):
    result = {}
    for path in sorted(root.rglob('*')):
        if '.git' in path.relative_to(root).parts:
            continue
        if path.is_symlink():
            raise ValueError('symlink in experiment input: ' + str(path))
        if path.is_file():
            if path.stat().st_size > LIMIT or len(result) >= 10000:
                raise ValueError('experiment input exceeds limits')
            result[str(path.relative_to(root))] = digest(path)
    return result


def process(argv, cwd, env, seconds, out, stdin=None):
    """Keep full bounded logs; kill the entire process group even if leader exits."""
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    started = time.monotonic()
    status = 'completed'
    with open(out.with_suffix('.stdout'), 'wb') as stdout, open(out.with_suffix('.stderr'), 'wb') as stderr:
        try:
            child = subprocess.Popen(argv, cwd=cwd, env=env, stdin=stdin or subprocess.DEVNULL,
                                     stdout=stdout, stderr=stderr, start_new_session=True)
        except OSError as error:
            return {'status': 'launch_failed', 'error': str(error), 'argv': argv, 'elapsed_s': time.monotonic() - started}
        try:
            while child.poll() is None:
                if time.monotonic() - started >= seconds:
                    status = 'timeout'
                    break
                if stdout.tell() + stderr.tell() > LIMIT:
                    status = 'output_limit'
                    break
                time.sleep(.05)
        finally:
            # No detached group member may continue modifying a candidate during judging.
            try:
                os.killpg(child.pid, signal.SIGTERM)
                time.sleep(.2)
                child.poll()  # Reap an exited group leader before probing on macOS.
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait(timeout=5)
        remaining = False
        try:
            os.killpg(child.pid, 0)
            remaining = True
        except ProcessLookupError:
            pass
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    return {'status': status, 'exit_code': child.returncode, 'argv': argv,
            'elapsed_s': time.monotonic() - started,
            'cpu_s': after.ru_utime + after.ru_stime - before.ru_utime - before.ru_stime,
            'process_group_remaining': remaining,
            'stdout_bytes': out.with_suffix('.stdout').stat().st_size,
            'stderr_bytes': out.with_suffix('.stderr').stat().st_size}


def codex_usage(path):
    tokens = {}
    tool_calls = 0
    for line in path.read_text(errors='replace').splitlines():
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if event.get('type') == 'turn.completed':
            for key, value in event.get('usage', {}).items():
                if isinstance(value, int):
                    tokens[key] = tokens.get(key, 0) + value
        if event.get('type') == 'item.completed' and event.get('item', {}).get('type') in ('command_execution', 'mcp_tool_call', 'web_search'):
            tool_calls += 1
    return {'tokens': tokens or None, 'tool_calls': tool_calls, 'cost': None,
            'cost_note': 'No price assumed; incomplete turns may omit usage.'}


def accepted_report(value, expected):
    if not isinstance(value, dict):
        return False
    rows = value.get('requirements', [])
    if not isinstance(rows, list) or not all(isinstance(row, dict) for row in rows):
        return False
    ids = [row.get('id') for row in rows]
    if len(ids) != len(set(ids)) or set(ids) != set(expected):
        return False
    return bool(rows) and all(row.get('status') == 'passed' for row in rows)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('experiment', type=Path)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    config_path = args.experiment.resolve()
    config = json.loads(config_path.read_text())
    assert config['schema_version'] == 1
    assert 1 <= config['attempts_per_variant'] <= 100
    assert 1 <= config['budget_seconds'] <= 3600
    assert config['rerun_policy'] == 'never', 'Record a new experiment instead of silently replacing an attempt'
    root = args.out.resolve()
    root.mkdir(parents=True, mode=0o700)
    resolve = lambda p: (config_path.parent / p).resolve()
    variants = config['variants']
    tasks = config['tasks']
    for collection in (variants, tasks):
        ids = [item['id'] for item in collection]
        assert len(ids) == len(set(ids)) and all(i and all(c.isalnum() or c in '-_' for c in i) for i in ids)
    inputs = {}
    for variant in variants:
        for field in ('binary', 'guidance'):
            path = resolve(variant[field])
            inputs[str(path)] = digest(path)
    for task in tasks:
        assert task['requirement_ids'] and len(set(task['requirement_ids'])) == len(task['requirement_ids'])
        template = resolve(task['template'])
        assert not root.is_relative_to(template), 'experiment output must be outside templates'
        inputs[str(template)] = inventory(template)
        for path in task['evaluator_inputs']:
            path = resolve(path)
            inputs[str(path)] = digest(path)
    schedule = [(task['id'], variant['id'], attempt) for task in tasks for attempt in range(config['attempts_per_variant']) for variant in variants]
    random.Random(config['seed']).shuffle(schedule)
    frozen = {'schema_version': 1, 'config': config, 'config_sha256': digest(config_path),
              'input_hashes': inputs, 'schedule': schedule, 'host': platform.platform(),
              'runner_version': subprocess.run([config['runner'][0], '--version'], capture_output=True, text=True).stdout.strip(),
              'created_unix': time.time(), 'isolation': 'local procedural separation; evaluator is outside candidate',
              'harness_sha256': digest(__file__)}
    write(root / 'experiment.json', frozen)
    results = []
    env = dict(os.environ)
    env.pop('SHADOWDROID_SESSION', None)
    if config.get('device'):
        env['SHADOWDROID_DEVICE'] = config['device']
        env['ANDROID_SERIAL'] = config['device']
    for task_id, variant_id, attempt in schedule:
        task = next(t for t in tasks if t['id'] == task_id)
        variant = next(v for v in variants if v['id'] == variant_id)
        directory = root / f'{task_id}-{variant_id}-{attempt:03}'
        directory.mkdir(mode=0o700)
        candidate = directory / 'candidate'
        shutil.copytree(resolve(task['template']), candidate)
        (candidate / 'SHADOWDROID_GUIDE.md').write_text(resolve(variant['guidance']).read_text())
        (candidate / 'AGENTS.md').write_text('Read SHADOWDROID_GUIDE.md. Work only in this candidate. Do not inspect other attempts, operator artifacts, or acceptance evaluators. Do not spawn agents.\n')
        for command in (['git', 'init', '-q'], ['git', 'add', '.'], ['git', '-c', 'user.name=Evaluation', '-c', 'user.email=evaluation@example.invalid', 'commit', '-qm', 'Frozen task input']):
            subprocess.run(command, cwd=candidate, check=True, capture_output=True)
        binary_dir = directory / 'bin'
        binary_dir.mkdir()
        shutil.copy2(resolve(variant['binary']), binary_dir / 'shadowdroid')
        env['PATH'] = str(binary_dir) + os.pathsep + os.environ['PATH']
        env['SHADOWDROID_AUTHORITY_DIR'] = str(directory / 'authority')
        substitutions = {'workspace': str(candidate), 'result': str(directory / 'acceptance.json'),
                         'final': str(candidate / 'completion.json'), 'device': config.get('device', ''),
                         'experiment_dir': str(config_path.parent), 'attempt_dir': str(directory)}
        prompt = task['prompt'] + '\nUse only the assigned workspace and tools. Your wall-clock budget is ' + str(config['budget_seconds']) + ' seconds. Finish with JSON {"complete":true|false,"summary":"..."}; report incomplete work honestly.\n'
        (directory / 'prompt.txt').write_text(prompt)
        argv = [arg.format(**substitutions) for arg in config['runner']]
        with open(directory / 'prompt.txt', 'rb') as prompt_file:
            agent = process(argv, candidate, env, config['budget_seconds'], directory / 'agent', prompt_file)
        # Record candidate patch and claims even when the runner times out or fails.
        subprocess.run(['git', 'add', '-N', '.'], cwd=candidate, check=True, capture_output=True)
        patch = subprocess.run(['git', 'diff', '--binary', 'HEAD'], cwd=candidate, capture_output=True, check=True).stdout
        (directory / 'candidate.patch').write_bytes(patch)
        claim = None
        try:
            claim = json.loads((candidate / 'completion.json').read_text()).get('complete')
        except (OSError, ValueError, AttributeError):
            pass
        record = {'task': task_id, 'variant': variant_id, 'attempt': attempt, 'agent': agent,
                  'claim_complete': claim, 'usage': codex_usage(directory / 'agent.stdout'),
                  'patch_sha256': digest(directory / 'candidate.patch')}
        write(directory / 'attempt.json', record)
        # Operator-controlled handoff/evaluation. Neither command is selected by the agent.
        if agent.get('process_group_remaining'):
            record['outcome'] = 'infrastructure_failure'
            record['error'] = 'surviving process group; manual recovery required'
        else:
            handoff = process([a.format(**substitutions) for a in config['handoff']], candidate, env, 60, directory / 'handoff') if config.get('handoff') else {'exit_code': 0, 'status': 'completed'}
            record['handoff'] = handoff
            if handoff.get('exit_code') != 0 or handoff.get('process_group_remaining'):
                record['outcome'] = 'infrastructure_failure'
            else:
                evaluation = process([a.format(**substitutions) for a in task['evaluator']], config_path.parent, env, config['evaluation_seconds'], directory / 'evaluator')
                record['evaluator'] = evaluation
                try:
                    acceptance = json.loads((directory / 'acceptance.json').read_text())
                except (OSError, ValueError):
                    acceptance = {}
                record['acceptance'] = acceptance
                complete = accepted_report(acceptance, task['requirement_ids'])
                infrastructure = evaluation.get('status') != 'completed' or evaluation.get('process_group_remaining') or not acceptance.get('requirements')
                record['outcome'] = 'infrastructure_failure' if infrastructure else ('passed' if complete and evaluation.get('exit_code') == 0 else 'failed')
        # Frozen evaluator/template/binary drift invalidates the experiment, not just one score.
        drift = []
        for name, expected in inputs.items():
            path = Path(name)
            actual = inventory(path) if path.is_dir() else digest(path)
            if actual != expected:
                drift.append(name)
        if drift:
            record.update(outcome='infrastructure_failure', input_drift=drift)
        record['false_completion'] = claim is True and record['outcome'] == 'failed'
        write(directory / 'attempt.json', record)
        results.append(record)
        write(root / 'results.json', {'scope': 'local pilot; no official benchmark or causal uplift claim', 'attempts': results,
                                     'attempted': len(results), 'planned': len(schedule),
                                     'passed': sum(r['outcome'] == 'passed' for r in results),
                                     'infrastructure_failures': sum(r['outcome'] == 'infrastructure_failure' for r in results)})
        print(json.dumps({'task': task_id, 'variant': variant_id, 'attempt': attempt, 'outcome': record['outcome']}), flush=True)
        if drift or agent.get('process_group_remaining') or record.get('handoff', {}).get('process_group_remaining') or record.get('evaluator', {}).get('process_group_remaining'):
            raise RuntimeError('Experiment stopped: drift or unresolved external work; remaining schedule preserved')


if __name__ == '__main__':
    main()
