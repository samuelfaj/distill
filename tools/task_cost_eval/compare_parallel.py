#!/usr/bin/env python3
"""Compare a baseline and a candidate Distill build on the parallel cohort."""
from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import time
import uuid
from pathlib import Path

from .compare_codex import LOCAL_TOOLS, distill_accounting, distill_config, subscription_account
from .evaluate import _sha256_file, _sha256_tree
from .parallel_verdict import verdict
from .runner import _terminate_owned_group

ROOT = Path(__file__).resolve().parent
MODEL = 'chatgpt/gpt-6.1-sol'
VARIANTS = ('baseline', 'candidate')
TOKEN_FIELDS = ('input_tokens', 'cached_input_tokens', 'output_tokens', 'reasoning_tokens')


def plan_runs(cohort, repetitions):
    """Interleave variants per case and repetition so time-of-day drift hits both equally."""
    return [(variant, case, rep) for rep in range(1, repetitions + 1)
            for case in cohort['cases'] for variant in VARIANTS]


def run_one(binary, variant, case, repetition, work_root, profile, timeout):
    output = work_root / f'{variant}-{case["id"]}-{repetition}'
    output.mkdir(parents=True, exist_ok=False)
    frozen = (_sha256_tree(ROOT / case['fixture_ref']), _sha256_file(ROOT / case['grader']['script_ref']))
    worktree = output / 'worktree'
    target = worktree / 'tools/task_cost_eval' / case['fixture_ref']
    target.parent.mkdir(parents=True)
    shutil.copytree(ROOT / case['fixture_ref'], target)
    subprocess.run(['git', 'init', '-q', str(worktree)], check=True)
    subprocess.run(['git', 'add', '.'], cwd=worktree, check=True)
    subprocess.run(['git', '-c', 'user.name=Benchmark', '-c', 'user.email=benchmark@localhost',
                    'commit', '-qm', 'Frozen task input'], cwd=worktree, check=True)
    prompt = (ROOT / case['prompt_ref']).read_text()
    prompt += '\nWork only in this repository. Finish the code change autonomously. Do not commit.'
    prompt += '\nThe available Python interpreter is python3.'
    (output / 'prompt.txt').write_text(prompt)
    home = output / 'distill-home'
    home.mkdir()
    auth = home / 'codex-auth.json'
    shutil.copyfile(profile / 'codex-auth.json', auth)
    auth.chmod(0o600)
    (home / 'config.toml').write_text(distill_config())
    session = str(uuid.uuid4())
    environment = {k: v for k, v in os.environ.items()
                   if not k.endswith('_API_KEY') and k != 'DISTILL_BENCH_NO_EXTERNAL_MODEL_KEY'}
    environment.update(GROK_MANAGED_MCPS_ENABLED='false', GROK_MANAGED_MCP_GATEWAY_TOOLS_ENABLED='false',
                       DISTILL_HOME=str(home), GROK_HOME=str(home))
    command = [str(binary), '--no-leader', '--always-approve', '--disable-web-search',
               '--cwd', str(worktree), '--session-id', session, '--prompt-file', str(output / 'prompt.txt'),
               '--output-format', 'streaming-json', '--model', MODEL, '--tools', LOCAL_TOOLS]
    (output / 'command.json').write_text(json.dumps(command, indent=2) + '\n')
    started = time.monotonic()
    process = None
    timed_out = False
    try:
        with (output / 'stdout.jsonl').open('w') as stdout, (output / 'stderr.txt').open('w') as stderr:
            process = subprocess.Popen(command, env=environment, cwd=worktree, stdin=subprocess.DEVNULL,
                                       stdout=stdout, stderr=stderr, start_new_session=True, text=True)
            try:
                process.communicate(timeout=timeout)
            except subprocess.TimeoutExpired:
                timed_out = True
                _terminate_owned_group(process)
    finally:
        wall = time.monotonic() - started
        if process is not None and process.poll() is None:
            _terminate_owned_group(process)
        auth.unlink(missing_ok=True)
    grade = subprocess.run(['python3', str(ROOT / case['grader']['script_ref']), str(worktree)],
                           text=True, capture_output=True)
    (output / 'grader.txt').write_text(grade.stdout + grade.stderr)
    accounting = distill_accounting(home, session)
    unchanged = frozen == (_sha256_tree(ROOT / case['fixture_ref']), _sha256_file(ROOT / case['grader']['script_ref']))
    return {'variant': variant, 'case': case['id'], 'kind': case['kind'], 'repetition': repetition,
            'passed': grade.returncode == 0 and unchanged and not timed_out,
            'wall_time_s': round(wall, 6), 'credits': accounting.get('credit_estimate'),
            **{f: accounting.get(f) for f in TOKEN_FIELDS}, 'model_calls': accounting.get('calls'),
            'accounting_complete': bool(accounting['accounting_complete']),
            'exit_code': process.returncode, 'timed_out': timed_out, 'session': session,
            'output_dir': str(output)}


def aggregate(runs):
    groups = {}
    for run in runs:
        for kind in (run['kind'], 'all'):
            groups.setdefault(f'{run["variant"]}/{kind}', []).append(run)
    out = {}
    for name, rows in groups.items():
        accepted = sum(1 for r in rows if r['passed'])
        complete = all(r['accounting_complete'] for r in rows)
        credits = sum(r['credits'] for r in rows) if complete else None
        out[name] = {'runs': len(rows), 'accepted': accepted, 'accounting_complete': complete,
                     'credits': credits, 'credits_per_accepted': credits / accepted if credits is not None and accepted else None,
                     'wall_time_s': sum(r['wall_time_s'] for r in rows),
                     'model_calls': sum(r['model_calls'] or 0 for r in rows),
                     **{f: sum(r[f] or 0 for r in rows) for f in TOKEN_FIELDS}}
    return out


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline-binary', type=Path, required=True)
    parser.add_argument('--candidate-binary', type=Path, required=True)
    parser.add_argument('--cohort', type=Path, default=ROOT / 'cohort-parallel-v1.json')
    parser.add_argument('--repetitions', type=int, default=3)
    parser.add_argument('--work-root', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--distill-profile', type=Path, default=Path.home() / '.grok')
    parser.add_argument('--timeout', type=float, default=300)
    parser.add_argument('--execute', action='store_true',
                        help='run the models (spends credits); without it only the plan is printed')
    args = parser.parse_args(argv)
    if args.repetitions < 1:
        parser.error('--repetitions must be at least 1')
    cohort = json.loads(args.cohort.read_text())
    binaries = {'baseline': args.baseline_binary, 'candidate': args.candidate_binary}
    planned = plan_runs(cohort, args.repetitions)
    if not args.execute:
        for variant, case, rep in planned:
            print(f'plan: {variant} {case["id"]} ({case["kind"]}) rep {rep} -> {binaries[variant]}')
        print(f'dry run: {len(planned)} runs planned, nothing executed; pass --execute to run')
        return 0
    for binary in binaries.values():
        if not binary.expanduser().resolve().is_file():
            parser.error(f'binary not found: {binary}')
    profile = args.distill_profile.expanduser()
    subscription_account(profile / 'codex-auth.json')
    work_root = args.work_root.resolve()
    work_root.mkdir(parents=True, exist_ok=True)
    runs = []
    for variant, case, rep in planned:
        run = run_one(binaries[variant].expanduser().resolve(), variant, case, rep, work_root, profile, args.timeout)
        runs.append(run)
        print(json.dumps({k: run[k] for k in ('variant', 'case', 'repetition', 'passed', 'credits')}), flush=True)
    results = {'cohort_id': cohort['cohort_id'], 'model': MODEL, 'runs': runs, 'aggregates': aggregate(runs),
               'binaries': {v: {'path': str(b), 'sha256': _sha256_file(b.expanduser().resolve())}
                            for v, b in binaries.items()}}
    results['verdict'] = verdict(results)
    args.output.write_text(json.dumps(results, indent=2) + '\n')
    print(json.dumps(results['verdict']))
    return 0 if results['verdict']['verdict'] == 'PASS' else 1


if __name__ == '__main__':
    raise SystemExit(main())
