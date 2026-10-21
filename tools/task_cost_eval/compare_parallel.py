#!/usr/bin/env python3
"""Compare a baseline and a candidate Distill build on the parallel cohort."""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import random
import shutil
import subprocess
import time
import tomllib
import uuid
from pathlib import Path

from .acp_driver import EXECUTION_MODES, OwnedTree, run_acp
from .compare_codex import LOCAL_TOOLS, distill_accounting, distill_config, subscription_account
from .evaluate import _sha256_file, _sha256_tree
from .parallel_verdict import verdict
from .runner import _terminate_owned_group
from .ultrafast_verdict import verdict as ultrafast_verdict
from .ultrasave_verdict import verdict as ultrasave_verdict

ROOT = Path(__file__).resolve().parent
MODEL = 'chatgpt/gpt-6.1-sol'
VARIANTS = ('baseline', 'candidate')
TOKEN_FIELDS = ('input_tokens', 'cached_input_tokens', 'output_tokens', 'reasoning_tokens')
UTILITY_MODEL = 'chatgpt/gpt-6-luna'
CREDIT_BASIS = ('credits: published subscription credit rates over LLM calls only; Jev (role "jev") calls are '
                'billed in native USD ticks and reported separately as jev_usd, never converted into credits')
CAPACITY_ENV = ('GROK_MAX_CONCURRENT_SUBAGENTS', 'GROK_SUBAGENT_SAMPLING_LIMIT',
                'GROK_WORKFLOW_MAX_CONCURRENT_AGENTS')


def plan_runs(cohort, repetitions, order_seed=None):
    """Keep pairs adjacent and reverse their order on every other repetition."""
    cases = list(cohort['cases'])
    if order_seed is not None:
        random.Random(order_seed).shuffle(cases)
    return [(variant, case, rep) for rep in range(1, repetitions + 1)
            for index, case in enumerate(cases)
            for variant in (VARIANTS if (rep - 1 + (index if order_seed is not None else 0)) % 2 == 0
                            else VARIANTS[::-1])]


def case_hashes(case):
    return (_sha256_tree(ROOT / case['fixture_ref']), _sha256_file(ROOT / case['grader']['script_ref']),
            _sha256_file(ROOT / case['prompt_ref']))


def comparison_hashes(cohort, binaries, cohort_path=None):
    # Freeze the small harness module set and whole grader source directories.
    # This covers local helper imports without executing graders to discover them.
    sources = set(Path(__file__).resolve().parent.glob('*.py'))
    for case in cohort['cases']:
        sources.update((ROOT / case['grader']['script_ref']).parent.rglob('*.py'))
    if cohort_path is not None:
        sources.add(cohort_path.resolve())
    return {'inputs': {case['id']: case_hashes(case) for case in cohort['cases']},
            'binaries': {variant: _sha256_file(binary) for variant, binary in binaries.items()},
            'support_files': {str(path): _sha256_file(path) for path in sorted(sources)}}


def run_grader(script, worktree, output, timeout=30):
    started = time.monotonic()
    timed_out = False
    with (output / 'grader.txt').open('w') as log:
        process = subprocess.Popen(['python3', str(script), str(worktree)], cwd=worktree,
                                   stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        tree = OwnedTree(process)
        try:
            while process.poll() is None:
                tree.refresh()
                if time.monotonic() - started >= timeout:
                    timed_out = True
                    break
                time.sleep(0.02)
        finally:
            if not tree.stop():
                raise RuntimeError('Grader process tree did not terminate')
    return {'exit_code': process.returncode, 'timed_out': timed_out,
            'wall_time_s': round(time.monotonic() - started, 6), 'timeout_s': timeout}


def run_one(binary, variant, case, repetition, work_root, profile, timeout, *,
            frozen_inputs, binary_hash,
            ultracode=False, max_depth=None, model=MODEL, effort=None, transport='cli',
            worker_model='chatgpt/gpt-6-luna', worker_effort='auto', utility_effort='auto', order_seed=None,
            execution_mode=None, max_concurrent=None, enable_jev=False, utility_model=UTILITY_MODEL):
    if execution_mode is not None and (execution_mode not in EXECUTION_MODES or ultracode):
        raise ValueError('Use one execution_mode or the legacy ultracode flag')
    mode = execution_mode or ('ultracode' if ultracode else 'normal')
    output = work_root / f'{variant}-{case["id"]}-{repetition}'
    output.mkdir(parents=True, exist_ok=False)
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
    config_text = distill_config(main_model=model, worker_model=worker_model, worker_effort=worker_effort,
                                 utility_effort=utility_effort, enable_jev=enable_jev, utility_model=utility_model)
    (home / 'config.toml').write_text(config_text)
    configured = tomllib.loads(config_text)
    session = str(uuid.uuid4()) if transport == 'cli' else None
    environment = {k: v for k, v in os.environ.items()
                   if not k.endswith('_API_KEY') and k != 'DISTILL_BENCH_NO_EXTERNAL_MODEL_KEY'}
    environment.update(GROK_MANAGED_MCPS_ENABLED='false', GROK_MANAGED_MCP_GATEWAY_TOOLS_ENABLED='false',
                       DISTILL_HOME=str(home), GROK_HOME=str(home))
    sanitize = (transport == 'acp' or order_seed is not None or execution_mode is not None or max_concurrent is not None
                or (worker_effort, utility_effort) != ('auto', 'auto')
                or worker_model != 'chatgpt/gpt-6-luna' or utility_model != UTILITY_MODEL)
    removed = []
    if sanitize:
        removed = sorted(k for k in environment if k == 'GROK_AGENT' or (max_concurrent is not None and k in CAPACITY_ENV) or (
            k.startswith(('GROK_', 'DISTILL_', 'JEV_', 'CODEX_', 'OPENAI_'))
            and any(token in k for token in ('DEPTH', 'MODEL', 'EFFORT'))))
        for key in removed:
            environment.pop(key)
    if max_depth is not None:
        environment['GROK_SUBAGENTS_MAX_DEPTH'] = str(max_depth)
    if max_concurrent is not None:
        environment.update({key: str(max_concurrent) for key in CAPACITY_ENV})
    command = [str(binary), '--no-leader', '--always-approve', '--disable-web-search',
               '--cwd', str(worktree), '--session-id', session, '--prompt-file', str(output / 'prompt.txt'),
               '--output-format', 'streaming-json', '--model', model, '--tools', LOCAL_TOOLS]
    if transport == 'acp':
        # AgentArgs owns this flag; the top-level pager flag is not forwarded to stdio.
        command = [str(binary), '--disable-web-search', '--no-auto-update',
                   'agent', '--no-leader', '--always-approve', '--model', model]
        if effort not in (None, 'auto'):
            command.extend(['--effort', effort])
        command.append('stdio')
    elif mode != 'normal':
        command.append(f'--{mode}')
    if transport == 'cli' and effort is not None:
        command.extend(['--effort', effort])
    (output / 'command.json').write_text(json.dumps(command, indent=2) + '\n')
    settings = {'transport': transport, 'ultracode': mode == 'ultracode', 'execution_mode': mode,
                'max_depth': max_depth, 'max_concurrent': max_concurrent,
                'capacity_env': {key: environment.get(key) for key in CAPACITY_ENV},
                'launch_command': command,
                'depth_env': environment.get('GROK_SUBAGENTS_MAX_DEPTH'),
                'model': model, 'effort': effort or 'auto',
                'configured_models': configured['models'],
                'configured_utility': configured['jev']['local'],
                'configured_main_effort_auto': configured['jev']['effort_auto'],
                'jev_enabled': bool(enable_jev), 'utility_model': utility_model,
                'order_seed': order_seed, 'timeout_s': timeout,
                'environment_policy': 'remove inherited model/effort/depth overrides' if sanitize else 'historical inheritance',
                'accounting_audit': 'strict study evidence' if sanitize else 'historical ledger reconciliation',
                'removed_environment_keys': removed,
                'tools': 'native agent toolset' if transport == 'acp' else LOCAL_TOOLS,
                'permissions': 'always-approve',
                'auxiliary_effort_scope': 'worker and jev.local only; call_usage records actual applied policy',
                'binary_sha256': binary_hash}
    (output / 'settings.json').write_text(json.dumps(settings, indent=2) + '\n')
    started = time.monotonic()
    process = None
    tree = None
    acp = None
    timed_out = False
    jev_target = home / 'openrouter-auth.json'
    try:
        shutil.copyfile(profile / 'codex-auth.json', auth)
        auth.chmod(0o600)
        if enable_jev:
            jev_source = profile / 'openrouter-auth.json'
            key = json.loads(jev_source.read_text()).get('key')
            if not isinstance(key, str) or not key:
                raise ValueError('OpenRouter auth file has no key')
            jev_target.touch(mode=0o600)
            jev_target.write_text(jev_source.read_text())
            environment['OPENROUTER_API_KEY'] = key
        if transport == 'acp':
            acp = run_acp(command, cwd=worktree, environment=environment, prompt=prompt,
                          model=model, effort=effort, ultracode=ultracode, execution_mode=execution_mode,
                          timeout=timeout, output=output)
            session, timed_out = acp['session'], acp['timed_out']
            settings['model_receipt'] = acp.get('model_receipt')
            settings['activation_receipt'] = acp.get('activation_receipt')
            (output / 'settings.json').write_text(json.dumps(settings, indent=2) + '\n')
        else:
            with (output / 'stdout.jsonl').open('w') as stdout, (output / 'stderr.txt').open('w') as stderr:
                process = subprocess.Popen(command, env=environment, cwd=worktree, stdin=subprocess.DEVNULL,
                                           stdout=stdout, stderr=stderr, start_new_session=True, text=True)
                try:
                    if sanitize:
                        tree = OwnedTree(process)
                        while process.poll() is None:
                            tree.refresh()
                            if time.monotonic() - started >= timeout:
                                raise subprocess.TimeoutExpired(command, timeout)
                            time.sleep(0.02)
                    else:
                        process.communicate(timeout=timeout)
                except subprocess.TimeoutExpired:
                    timed_out = True
                    _terminate_owned_group(process)
    finally:
        try:
            if tree is not None and not tree.stop():
                raise RuntimeError('Inference process tree did not terminate')
            if process is not None and process.poll() is None:
                _terminate_owned_group(process)
        finally:
            wall = time.monotonic() - started
            auth.unlink(missing_ok=True)
            jev_target.unlink(missing_ok=True)
    clean = acp is None or acp['cleanup_complete']
    grade = (run_grader(ROOT / case['grader']['script_ref'], worktree, output) if clean else
             {'exit_code': None, 'timed_out': False, 'wall_time_s': 0, 'skipped': 'unverified inference cleanup'})
    accounting = (distill_accounting(home, session, strict=True) if clean and session and sanitize else
                  distill_accounting(home, session) if clean and session else
                  {'accounting_complete': False, 'reason': 'no session or unverified inference cleanup'})
    jev_ticks = [call.get('cost_usd_ticks') for call in accounting.get('call_usage', []) if call.get('role') == 'jev']
    jev_priced = all(type(ticks) is int and ticks >= 0 for ticks in jev_ticks)
    jev_usd = sum(jev_ticks) / 1e10 if jev_priced else None
    evidence = accounting.get('accounting_evidence', {
        'complete': False, 'reasons': [accounting.get('reason') or 'strict accounting evidence audit not performed'],
        'artifacts': {}})
    unchanged = frozen_inputs == case_hashes(case)
    binary_unchanged = binary_hash == _sha256_file(binary)
    if acp is None:
        label = {'ultracode': 'UltraCode', 'ultrafast': 'UltraFast', 'ultrasave': 'UltraSave'}.get(mode)
        activation_confirmed = (mode == 'normal' or
            f'{label} enabled for session {session}' in (output / 'stderr.txt').read_text())
        exit_code, transport_ok = process.returncode, True
    else:
        activation = (acp.get('activation_receipt') or {}).get('result')
        activation_confirmed = isinstance(activation, dict) and (
            activation.get('mode') == execution_mode if execution_mode is not None
            else activation.get('enabled') is ultracode)
        exit_code, transport_ok = acp['exit_code'], acp['completed']
    return {'variant': variant, 'case': case['id'], 'kind': case['kind'], 'repetition': repetition,
            'passed': grade['exit_code'] == 0 and not grade['timed_out'] and unchanged and binary_unchanged and not timed_out
                      and exit_code == 0 and transport_ok and activation_confirmed,
            'wall_time_s': round(wall, 6), 'credits': accounting.get('credit_estimate'),
            **{f: accounting.get(f) for f in TOKEN_FIELDS}, 'model_calls': accounting.get('calls'),
            'accounting_complete': bool(accounting['accounting_complete'] and (not sanitize or evidence['complete'])
                                        and (not enable_jev or jev_priced)),
            'jev_usd': jev_usd, 'jev_calls': len(jev_ticks),
            'accounting_evidence': evidence,
            'call_usage': accounting.get('call_usage', []),
            'exit_code': exit_code, 'timed_out': timed_out, 'session': session, 'grader': grade,
            'acp': acp, 'accounting_reason': accounting.get('reason'),
            'settings': settings, 'ultracode_activation_confirmed': mode == 'ultracode' and activation_confirmed,
            'execution_mode_activation_confirmed': activation_confirmed,
            'frozen_inputs_unchanged': unchanged, 'binary_unchanged': binary_unchanged,
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
                     'jev_usd': sum(r['jev_usd'] for r in rows) if all(r.get('jev_usd') is not None for r in rows) else None,
                     **{f: sum(r[f] for r in rows) if all(r.get(f) is not None for r in rows) else None
                        for f in ('model_calls', *TOKEN_FIELDS)}}
    return out


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline-binary', type=Path, required=True)
    parser.add_argument('--candidate-binary', type=Path, required=True)
    parser.add_argument('--transport', choices=('cli', 'acp'), default='cli',
                        help='common transport for both binaries; ACP supports builds without --ultracode')
    parser.add_argument('--order-seed', type=int, help='shuffle tasks once; alternate AB/BA across tasks and repetitions')
    parser.add_argument('--worker-effort', default='auto', help='[models].worker_effort for both variants')
    parser.add_argument('--worker-model', default='chatgpt/gpt-6-luna',
                        help='[models].worker for both variants')
    parser.add_argument('--utility-effort', default='auto', help='[jev.local].effort for both variants')
    parser.add_argument('--utility-model', default=UTILITY_MODEL,
                        help='[jev.local].model and [models].session_summary for both variants')
    parser.add_argument('--enable-jev', action='store_true',
                        help='stage <distill-profile>/openrouter-auth.json into each run home and enable Jev; '
                             'Jev calls are reported as jev_usd, not credits')
    parser.add_argument('--compare-versions', action='store_true',
                        help='head-to-head release comparison: baseline and candidate may use the same execution '
                             'mode with different binaries; requires an ultrafast/ultrasave verdict')
    parser.add_argument('--max-concurrent', type=int, help='common task, sampling, and workflow capacity')
    parser.add_argument('--verdict', choices=('parallel', 'ultrafast', 'ultrasave'), default='parallel',
                        help='paired ACP gates vs Normal: ultrafast median completion ratio <=0.60; ultrasave median credit '
                             'ratio <=0.45 (both <=0.85 with --compare-versions); at most 1 quality regression')
    for variant in VARIANTS:
        mode = parser.add_mutually_exclusive_group()
        mode.add_argument(f'--{variant}-ultracode', action='store_true',
                            help=f'enable the real session mode for {variant}; default off')
        mode.add_argument(f'--{variant}-mode', choices=EXECUTION_MODES,
                          help='explicit mutually exclusive mode; ACP requires the new mode endpoint')
        parser.add_argument(f'--{variant}-max-depth', type=int,
                            help='set GROK_SUBAGENTS_MAX_DEPTH (1 = flat, 2+ = nested); default inherits runtime')
    parser.add_argument('--model', default=MODEL)
    parser.add_argument('--effort', help='explicit main effort for both variants; default keeps configured auto')
    parser.add_argument('--case', action='append', dest='cases',
                        help='existing case ID to run (repeatable); default runs the full cohort')
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
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error('--timeout must be finite and greater than zero')
    if args.max_concurrent is not None and args.max_concurrent < 1:
        parser.error('--max-concurrent must be at least 1')
    paired_mode = args.verdict in ('ultrafast', 'ultrasave')
    baseline_modes = ('normal', 'ultracode') if args.verdict == 'ultrafast' else ('normal',)
    if args.compare_versions:
        if not paired_mode:
            parser.error('--compare-versions requires --verdict ultrafast or ultrasave')
        baseline_modes += (args.verdict,)
    if paired_mode and (
            args.transport != 'acp' or args.candidate_mode != args.verdict or
            args.baseline_mode not in baseline_modes or args.max_concurrent is None or
            args.order_seed is None or args.baseline_max_depth != args.candidate_max_depth):
        parser.error(f'{args.verdict} verdict requires ACP, explicit {"/".join(baseline_modes)} vs {args.verdict} modes, '
                     '--max-concurrent, --order-seed, and identical depth overrides')
    for variant in VARIANTS:
        depth = getattr(args, f'{variant}_max_depth')
        if depth is not None and depth < 1:
            parser.error(f'--{variant}-max-depth must be at least 1')
    cohort_bytes = args.cohort.read_bytes()
    cohort = json.loads(cohort_bytes)
    if args.cases:
        unknown = set(args.cases) - {c['id'] for c in cohort['cases']}
        if unknown:
            parser.error(f'unknown case IDs: {", ".join(sorted(unknown))}')
        cohort = {**cohort, 'cases': [c for c in cohort['cases'] if c['id'] in args.cases]}
    settings = {variant: {'ultracode': getattr(args, f'{variant}_ultracode'),
                          'execution_mode': getattr(args, f'{variant}_mode'),
                          'max_depth': getattr(args, f'{variant}_max_depth'),
                          'max_concurrent': args.max_concurrent,
                          'model': args.model, 'effort': args.effort, 'transport': args.transport,
                          'worker_model': args.worker_model, 'worker_effort': args.worker_effort,
                          'utility_effort': args.utility_effort,
                          'utility_model': args.utility_model, 'enable_jev': args.enable_jev,
                          'order_seed': args.order_seed} for variant in VARIANTS}
    binaries = {'baseline': args.baseline_binary, 'candidate': args.candidate_binary}
    planned = plan_runs(cohort, args.repetitions, args.order_seed)
    if not args.execute:
        for variant, case, rep in planned:
            print(f'plan: {variant} {case["id"]} ({case["kind"]}) rep {rep} -> {binaries[variant]} '
                  f'{json.dumps(settings[variant], sort_keys=True)}')
        print(f'dry run: {len(planned)} runs planned, nothing executed; pass --execute to run')
        return 0
    for binary in binaries.values():
        if not binary.expanduser().resolve().is_file():
            parser.error(f'binary not found: {binary}')
    binaries = {variant: binary.expanduser().resolve() for variant, binary in binaries.items()}
    frozen = comparison_hashes(cohort, binaries, args.cohort)
    if paired_mode and not args.compare_versions and len(set(frozen['binaries'].values())) != 1:
        parser.error(f'{args.verdict} verdict requires the same binary SHA-256 for both variants')
    if frozen['support_files'][str(args.cohort.resolve())] != hashlib.sha256(cohort_bytes).hexdigest():
        raise RuntimeError('Frozen cohort changed before execution')
    profile = args.distill_profile.expanduser()
    subscription_account(profile / 'codex-auth.json')
    if args.enable_jev and not (profile / 'openrouter-auth.json').is_file():
        parser.error(f'--enable-jev needs {profile / "openrouter-auth.json"}')
    work_root = args.work_root.resolve()
    work_root.mkdir(parents=True, exist_ok=True)
    runs = []
    for variant, case, rep in planned:
        if comparison_hashes(cohort, binaries, args.cohort) != frozen:
            raise RuntimeError('Frozen comparison inputs or binaries changed before execution')
        run = run_one(binaries[variant], variant, case, rep, work_root, profile,
                      args.timeout, frozen_inputs=frozen['inputs'][case['id']],
                      binary_hash=frozen['binaries'][variant], **settings[variant])
        if comparison_hashes(cohort, binaries, args.cohort) != frozen:
            raise RuntimeError('Frozen comparison inputs or binaries changed after execution')
        runs.append(run)
        print(json.dumps({k: run[k] for k in ('variant', 'case', 'repetition', 'passed', 'wall_time_s',
                                             'credits', 'accounting_complete')}), flush=True)
        if run.get('acp') is not None and not run['acp']['cleanup_complete']:
            break  # Never start another model while teardown remains unverified.
    results = {'cohort_id': cohort['cohort_id'], 'model': args.model, 'effort': args.effort or 'auto',
               'variant_settings': settings, 'cases': [c['id'] for c in cohort['cases']],
               'frozen_input_hashes': frozen['inputs'], 'frozen_support_hashes': frozen['support_files'],
               'transport': args.transport, 'order_seed': args.order_seed, 'repetitions': args.repetitions,
               'verdict_policy': args.verdict, 'compare_versions': args.compare_versions,
               'jev_enabled': args.enable_jev, 'utility_model': args.utility_model,
               'credit_basis': CREDIT_BASIS,
               'runs': runs, 'aggregates': aggregate(runs),
               'binaries': {v: {'path': str(b), 'sha256': frozen['binaries'][v]}
                            for v, b in binaries.items()}}
    results['verdict'] = {'parallel': verdict, 'ultrafast': ultrafast_verdict,
                          'ultrasave': ultrasave_verdict}[args.verdict](results)
    invalid = [f'{r["variant"]} {r["case"]}#{r["repetition"]}: {r["acp"]["error"] or "ACP incomplete"}'
               for r in runs if r.get('acp') is not None
               and (not r['acp']['setup_confirmed'] or not r['acp']['cleanup_complete'])]
    if invalid:
        results['verdict']['verdict'] = 'INCOMPLETE'
        results['verdict']['reasons'].extend(invalid)
    args.output.write_text(json.dumps(results, indent=2) + '\n')
    print(json.dumps(results['verdict']))
    return 0 if results['verdict']['verdict'] == 'PASS' else 1


if __name__ == '__main__':
    raise SystemExit(main())
