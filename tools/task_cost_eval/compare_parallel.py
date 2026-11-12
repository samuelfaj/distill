#!/usr/bin/env python3
"""Compare a baseline and a candidate Distill build on the parallel cohort."""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import random
import re
import shutil
import subprocess
import tempfile
import time
import tomllib
import urllib.error
import urllib.request
import uuid
from collections import Counter
from pathlib import Path

from . import decision_quality
from .acp_driver import EXECUTION_MODES, OwnedTree, run_acp
from .compare_codex import LOCAL_TOOLS, distill_accounting, distill_config, subscription_account
from .evaluate import _sha256_file, _sha256_tree
from .gates import aa_check
from .parallel_verdict import verdict
from .runner import _terminate_owned_group
from .ultrafast_verdict import charged_completion_s, verdict as ultrafast_verdict
from .ultrasave_verdict import verdict as ultrasave_verdict
from .auto_verdict import verdict as auto_verdict
from .modes_verdict import arm_labels, arm_mode, normal_verdict, verdict as modes_verdict

ROOT = Path(__file__).resolve().parent
MODEL = 'chatgpt/gpt-6.1-sol'
VARIANTS = ('baseline', 'candidate')
AA_VARIANT = 'baseline_aa'
TOKEN_FIELDS = ('input_tokens', 'cached_input_tokens', 'output_tokens', 'reasoning_tokens')
UTILITY_MODEL = 'chatgpt/gpt-6-luna'
CREDIT_BASIS = ('credits: published subscription credit rates over LLM calls only; Jev (role "jev") calls are '
                'billed in native USD ticks and reported separately as jev_usd, never converted into credits')
CAPACITY_ENV = ('GROK_MAX_CONCURRENT_SUBAGENTS', 'GROK_SUBAGENT_SAMPLING_LIMIT',
                'GROK_WORKFLOW_MAX_CONCURRENT_AGENTS')
# Mirrors distill-workspace jev/client.rs with the default OpenRouter Decisions provider.
JEV_DECISIONS_URL = 'https://openrouter.ai/api/alpha/decisions'
JEV_MODEL = '~typesafe/jev-latest'
# Never copied with a kept failing run folder.
CREDENTIAL_FILES = ('codex-auth.json', 'openrouter-auth.json')
WORKSPACE_PROFILES = ('plain', 'realistic')
REALISTIC_PROFILE = ROOT / 'profiles' / 'realistic'
REALISTIC_MCP_NAME = 'team_notes'


def plan_runs(cohort, repetitions, order_seed=None, variants=VARIANTS):
    """Keep arms of one case adjacent and rotate their order across repetitions.

    Two arms reverse on every other repetition (and across tasks when seeded);
    more arms rotate one step per case group, so over the whole campaign each
    arm leads as often as the others (exactly when groups divide evenly).
    """
    cases = list(cohort['cases'])
    if order_seed is not None:
        random.Random(order_seed).shuffle(cases)
    planned = []
    for rep in range(1, repetitions + 1):
        for index, case in enumerate(cases):
            if len(variants) > 2:
                shift = ((rep - 1) * len(cases) + index) % len(variants)
            else:
                shift = (rep - 1 + (index if order_seed is not None else 0)) % len(variants)
            planned.extend((variant, case, rep) for variant in variants[shift:] + variants[:shift])
    return planned


def jev_preflight(key, timeout=30):
    """One minimal Jev decision with the staged key; returns {'ok', 'status', 'error'}.

    Same endpoint, bearer header and typed envelope as the Rust client, with a
    single yes/no question. Tests replace this function; it is never retried.
    """
    body = json.dumps({'state': {'purpose': 'benchmark preflight'}, 'model': JEV_MODEL,
                       'questions': {'ready': {'type': 'noul',
                                               'instructions': 'Is this a benchmark preflight request?'}}}).encode()
    request = urllib.request.Request(JEV_DECISIONS_URL, data=body, method='POST', headers={
        'Authorization': f'Bearer {key}', 'Content-Type': 'application/json'})
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            response.read()
            return {'ok': 200 <= response.status < 300, 'status': response.status, 'error': None}
    except urllib.error.HTTPError as error:
        return {'ok': False, 'status': error.code, 'error': None}
    except (urllib.error.URLError, OSError, TimeoutError) as error:
        return {'ok': False, 'status': None, 'error': type(error).__name__}


def binary_commit(binary, declared=None):
    """Source commit of a build: from `<binary> --version`, else the declared argument, else unknown."""
    try:
        with tempfile.TemporaryDirectory() as home:
            environment = {**{k: v for k, v in os.environ.items() if not k.endswith('_API_KEY')},
                           'DISTILL_HOME': home, 'GROK_HOME': home}
            output = subprocess.run([str(binary), '--version'], capture_output=True, text=True, timeout=15,
                                    env=environment, stdin=subprocess.DEVNULL).stdout
    except (OSError, subprocess.SubprocessError):
        output = ''
    found = re.search(r'\(([0-9a-f]{7,40})\)', output)
    if found:
        record = {'source_commit': found.group(1), 'commit_source': 'version'}
        if declared and declared != found.group(1):
            record['declared_commit'] = declared
        return record
    if declared:
        return {'source_commit': declared, 'commit_source': 'argument'}
    return {'source_commit': 'unknown', 'commit_source': 'unknown'}


def keep_failed_run(run_dir, destination):
    """Copy a failing run folder for later review; credential copies are never kept."""
    def ignore(_directory, names):
        return [name for name in names if name in CREDENTIAL_FILES]
    target = destination / Path(run_dir).name
    shutil.copytree(run_dir, target, symlinks=True, ignore=ignore)
    return str(target)


def campaign_aa_check(control_rows, aa_rows):
    """A/A on credits and charged completion time (failures charged the timeout)."""
    def credits(row):
        return row['credits'] if row.get('accounting_complete') and row.get('credits') is not None else None

    def seconds(row):
        return charged_completion_s(row) if row.get('grader') and row.get('settings') else row['wall_time_s']
    return aa_check(control_rows, aa_rows, credits, seconds)


def mark_aa(report, check):
    """An out-of-band A/A makes the campaign INVALID; an unmeasurable one leaves it INCOMPLETE."""
    if check['valid']:
        return
    report['reasons'].extend(check['reasons'])
    measured = check['credit_ratio'] is not None and check['time_ratio'] is not None
    report['verdict'] = 'INVALID' if measured else 'INCOMPLETE'


def arms_from_argument(parser, args):
    """Validate an --arms list and return its labels (the second normal is the A/A arm)."""
    arms = [arm.strip() for arm in args.arms.split(',') if arm.strip()]
    others = [arm for arm in arms if arm != 'normal']
    if (len(arms) < 2 or any(arm not in EXECUTION_MODES for arm in arms) or arms.count('normal') not in (1, 2)
            or len(set(others)) != len(others)):
        parser.error(f'--arms takes 2 or more of {", ".join(EXECUTION_MODES)}: normal once (twice adds the '
                     'A/A arm), every other mode at most once')
    paired_flags = [flag for flag in ('baseline_binary', 'candidate_binary', 'baseline_mode', 'candidate_mode',
                                      'baseline_max_depth', 'candidate_max_depth', 'baseline_commit')
                    if getattr(args, flag) is not None]
    if (paired_flags or args.aa_arm or args.baseline_ultracode or args.candidate_ultracode
            or args.compare_versions):
        parser.error('--arms runs one --binary; it does not take paired --baseline-*/--candidate-* options, '
                     '--aa-arm or --compare-versions (use --candidate-commit for the binary commit)')
    if args.verdict == 'parallel':
        args.verdict = 'modes'
    if args.verdict != 'modes':
        parser.error('--arms uses --verdict modes (Auto and UltraCode are judged inside it)')
    if 'ultracode' not in arms and (args.substantial or args.control):
        parser.error('--substantial/--control need an ultracode arm')
    if args.execute:
        missing = [flag for flag, value in (('--binary', args.binary), ('--work-root', args.work_root),
                                            ('--output', args.output)) if value is None]
        if missing or args.transport != 'acp':
            parser.error('an executed --arms campaign needs --transport acp, --binary, --work-root and --output')
    return arm_labels(arms)


def call_summary(calls):
    """Per-run time and round split from per-call usage rows."""
    durations = [call.get('api_duration_ms') for call in calls]
    jev = [call for call in calls if call.get('role') == 'jev']
    return {
        'model_time_ms': sum(call.get('api_duration_ms') or 0 for call in calls if call.get('role') != 'jev'),
        'jev_time_ms': sum(call.get('api_duration_ms') or 0 for call in jev),
        'calls_without_duration': sum(value is None for value in durations),
        'root_rounds': sum(call.get('agent') == 'main' and call.get('role') in ('main', 'main_retry') for call in calls),
        'child_calls': sum(call.get('agent') == 'worker' and call.get('role') != 'jev' for call in calls),
        'jev_failed_calls': sum(call.get('status', 'completed') != 'completed' for call in jev),
    }


def case_hashes(case):
    hashes = (_sha256_tree(ROOT / case['fixture_ref']), _sha256_file(ROOT / case['grader']['script_ref']),
              _sha256_file(ROOT / case['prompt_ref']))
    if case.get('followup_prompt_ref'):
        hashes += (_sha256_file(ROOT / case['followup_prompt_ref']),)
    return hashes


def case_timeout(case, default):
    """A case's own timeout_s (long tasks) wins over the campaign --timeout."""
    return case.get('timeout_s', default)


def cohort_timeout_error(cases):
    """Verdicts require one timeout over all runs, so a cohort may not mix case budgets."""
    values = {case.get('timeout_s') for case in cases}
    for value in values - {None}:
        if type(value) not in (int, float) or not math.isfinite(value) or value <= 0:
            return f'timeout_s must be a positive finite number, got {value!r}'
    if len(values) > 1:
        return 'every selected case must declare the same timeout_s (or none): verdicts compare one timeout'
    return None


def seed_realistic_profile(home, worktree):
    """Synthetic prefix of a typical user: home skills, project AGENTS.md, one local MCP stub.

    AGENTS.md is written before the frozen commit so it is task input, not an
    agent change; the returned TOML is appended to the run config.
    """
    shutil.copytree(REALISTIC_PROFILE / 'skills', home / 'skills')
    shutil.copyfile(REALISTIC_PROFILE / 'AGENTS.md', worktree / 'AGENTS.md')
    server = home / 'mcp' / 'notes_server.py'
    server.parent.mkdir()
    shutil.copyfile(REALISTIC_PROFILE / 'notes_server.py', server)
    return (f'[mcp_servers.{REALISTIC_MCP_NAME}]\ncommand = "python3"\n'
            f'args = [{json.dumps(str(server))}]\nstartup_timeout_sec = 10\n')


def comparison_hashes(cohort, binaries, cohort_path=None, workspace_profile='plain'):
    # Freeze the small harness module set and whole grader source directories.
    # This covers local helper imports without executing graders to discover them.
    sources = set(Path(__file__).resolve().parent.glob('*.py'))
    if workspace_profile == 'realistic':
        sources.update(path for path in REALISTIC_PROFILE.rglob('*') if path.is_file())
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
            execution_mode=None, max_concurrent=None, enable_jev=False, utility_model=UTILITY_MODEL,
            ultrasave_model=None, workspace_profile='plain'):
    if execution_mode is not None and (execution_mode not in EXECUTION_MODES or ultracode):
        raise ValueError('Use one execution_mode or the legacy ultracode flag')
    if case.get('followup_prompt_ref') and transport != 'acp':
        raise ValueError('A case with followup_prompt_ref needs the ACP transport (one session, two prompts)')
    if workspace_profile not in WORKSPACE_PROFILES:
        raise ValueError(f'workspace_profile must be one of {", ".join(WORKSPACE_PROFILES)}')
    mode = execution_mode or ('ultracode' if ultracode else 'normal')
    output = work_root / f'{variant}-{case["id"]}-{repetition}'
    output.mkdir(parents=True, exist_ok=False)
    worktree = output / 'worktree'
    target = worktree / 'tools/task_cost_eval' / case['fixture_ref']
    target.parent.mkdir(parents=True)
    shutil.copytree(ROOT / case['fixture_ref'], target)
    home = output / 'distill-home'
    home.mkdir()
    profile_config = seed_realistic_profile(home, worktree) if workspace_profile == 'realistic' else ''
    subprocess.run(['git', 'init', '-q', str(worktree)], check=True)
    subprocess.run(['git', 'add', '.'], cwd=worktree, check=True)
    subprocess.run(['git', '-c', 'user.name=Benchmark', '-c', 'user.email=benchmark@localhost',
                    'commit', '-qm', 'Frozen task input'], cwd=worktree, check=True)
    prompt = (ROOT / case['prompt_ref']).read_text()
    prompt += '\nWork only in this repository. Finish the code change autonomously. Do not commit.'
    prompt += '\nThe available Python interpreter is python3.'
    (output / 'prompt.txt').write_text(prompt)
    followup = None
    if case.get('followup_prompt_ref'):
        followup = (ROOT / case['followup_prompt_ref']).read_text()
        (output / 'followup-prompt.txt').write_text(followup)
    auth = home / 'codex-auth.json'
    config_text = distill_config(main_model=model, worker_model=worker_model, worker_effort=worker_effort,
                                 utility_effort=utility_effort, enable_jev=enable_jev, utility_model=utility_model,
                                 ultrasave_model=ultrasave_model) + profile_config
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
    elif mode == 'auto':
        command.extend(['--mode', 'auto'])
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
                'configured_modes': configured.get('modes'),
                'order_seed': order_seed, 'timeout_s': timeout,
                'followup_prompt': followup is not None, 'workspace_profile': workspace_profile,
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
                          timeout=timeout, output=output, followup_prompt=followup)
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
    # Decision and outcome events of the root session, passing runs too; read only after a verified cleanup.
    timeline = (acp or {}).get('effective_mode_timeline') or [None if mode == 'auto' else mode]
    decisions = decision_quality.run_record(
        decision_quality.session_events_path(home, session) if clean else None, initial_mode=timeline[0])
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
            activation.get('choice', activation.get('mode')) == execution_mode if execution_mode is not None
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
            **call_summary(accounting.get('call_usage', [])),
            **{key: (acp or {}).get(key) for key in ('effective_mode_mix', 'mode_switches', 'effective_mode_timeline')},
            'accounting_evidence': evidence,
            'call_usage': accounting.get('call_usage', []), **decisions,
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
        main_efforts = [[(call.get('effort') or 'unknown').removeprefix('effort:')
                         for call in (run.get('call_usage') or []) if call.get('role') == 'main'] for run in rows]
        mix = Counter(label for labels in main_efforts for label in labels)
        modes = Counter()
        for run in rows:
            modes.update(run.get('effective_mode_mix') or {})
        accepted = sum(1 for r in rows if r['passed'])
        complete = all(r['accounting_complete'] for r in rows)
        credits = sum(r['credits'] for r in rows) if complete else None
        out[name] = {'runs': len(rows), 'accepted': accepted, 'accounting_complete': complete,
                     'credits': credits, 'credits_per_accepted': credits / accepted if credits is not None and accepted else None,
                     'wall_time_s': sum(r['wall_time_s'] for r in rows),
                     'jev_usd': sum(r['jev_usd'] for r in rows) if all(r.get('jev_usd') is not None for r in rows) else None,
                     **{f: sum(r[f] for r in rows) if all(r.get(f) is not None for r in rows) else None
                        for f in ('model_calls', *TOKEN_FIELDS)},
                     'main_effort_mix': dict(sorted(mix.items())),
                     'main_effort_switches': sum(a != b for labels in main_efforts for a, b in zip(labels, labels[1:])),
                     'effective_mode_mix': dict(sorted(modes.items())),
                     'mode_switches': sum(r.get('mode_switches') or 0 for r in rows),
                     **{f: sum(r.get(f) or 0 for r in rows)
                        for f in ('model_time_ms', 'jev_time_ms', 'root_rounds', 'child_calls', 'jev_failed_calls')},
                     'decision_quality': decision_quality.quality_block(rows)}
    return out


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline-binary', type=Path, help='paired campaigns: required')
    parser.add_argument('--candidate-binary', type=Path, help='paired campaigns: required')
    parser.add_argument('--arms', help='N-arm campaign on one --binary, e.g. normal,normal,ultrasave,ultrafast,ultracode,'
                                       'auto; a second normal is the A/A arm; replaces --baseline-*/--candidate-*')
    parser.add_argument('--binary', type=Path, help='the one binary of an --arms campaign (needed with --execute)')
    parser.add_argument('--ultrasave-model',
                        help='tiered profile: write [modes.ultrasave] model into every run config; without it the '
                             'campaign is the single-model profile')
    parser.add_argument('--substantial', action='append', help='--arms with ultracode: substantial case ID (repeatable)')
    parser.add_argument('--control', action='append', help='--arms with ultracode: control case ID (repeatable)')
    parser.add_argument('--transport', choices=('cli', 'acp'), default='cli',
                        help='common transport for both binaries; ACP supports builds without --ultracode')
    parser.add_argument('--order-seed', type=int, help='shuffle tasks once; alternate AB/BA across tasks and repetitions')
    parser.add_argument('--worker-effort', default='auto', help='[models].worker_effort for both variants')
    parser.add_argument('--worker-model',
                        help='[models].worker for every arm; default chatgpt/gpt-6-luna for paired campaigns and '
                             'no worker (empty) for --arms')
    parser.add_argument('--utility-effort', default='auto', help='[jev.local].effort for both variants')
    parser.add_argument('--utility-model', default=UTILITY_MODEL,
                        help='[jev.local].model and [models].session_summary for both variants')
    parser.add_argument('--enable-jev', action='store_true',
                        help='stage <distill-profile>/openrouter-auth.json into each run home and enable Jev; '
                             'Jev calls are reported as jev_usd, not credits')
    parser.add_argument('--compare-versions', action='store_true',
                        help='head-to-head release comparison: baseline and candidate may use the same execution '
                             'mode with different binaries; requires an ultrafast/ultrasave/auto/normal verdict')
    parser.add_argument('--max-concurrent', type=int, help='common task, sampling, and workflow capacity')
    parser.add_argument('--verdict', choices=('parallel', 'ultrafast', 'ultrasave', 'auto', 'normal', 'modes'),
                        default='parallel',
                        help='paired ACP gates vs Normal on ratios of totals: ultrafast completion <=0.60; ultrasave '
                             'credits <=0.45 (both <=0.85 with --compare-versions); auto credits and completion '
                             '<=1.0 with one <=0.85 vs Normal or a previous Auto version; '
                             'Auto vs specialists reports tradeoffs without a performance gate; Auto accepts at least '
                             'the baseline minus 1 in 60 runs; other verdicts allow at most 1 quality regression; '
                             'fewer than 3 repetitions report SMOKE, not a verdict; normal (with --compare-versions) '
                             'keeps old-vs-new Normal ratios inside 1.10; modes is the --arms verdict')
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
    parser.add_argument('--work-root', type=Path, help='required except for an --arms dry run')
    parser.add_argument('--output', type=Path, help='required except for an --arms dry run')
    parser.add_argument('--distill-profile', type=Path, default=Path.home() / '.grok')
    parser.add_argument('--timeout', type=float, default=300,
                        help='per-run budget in seconds; a case with its own timeout_s uses that instead')
    parser.add_argument('--workspace-profile', choices=WORKSPACE_PROFILES, default='plain',
                        help='realistic seeds each run with a synthetic skills catalog in the isolated home, an '
                             'AGENTS.md at the workspace root and one local stdio MCP stub, so prompts carry the '
                             'prefix real users have; plain (default) seeds nothing')
    parser.add_argument('--aa-arm', action='store_true',
                        help='add an A/A arm (the baseline binary and settings again); the campaign is INVALID '
                             'when its ratio of totals leaves 0.90-1.10 on credits or time')
    for variant in VARIANTS:
        parser.add_argument(f'--{variant}-commit',
                            help=f'source commit of the {variant} binary when its --version does not carry one')
    parser.add_argument('--keep-failed-runs', type=Path,
                        help='copy each failing run folder here (credential copies excluded)')
    parser.add_argument('--execute', action='store_true',
                        help='run the models (spends credits); without it only the plan is printed')
    args = parser.parse_args(argv)
    if args.repetitions < 1:
        parser.error('--repetitions must be at least 1')
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error('--timeout must be finite and greater than zero')
    if args.max_concurrent is not None and args.max_concurrent < 1:
        parser.error('--max-concurrent must be at least 1')
    labels = None
    if args.arms is not None:
        labels = arms_from_argument(parser, args)
    else:
        missing = [flag for flag, value in (('--baseline-binary', args.baseline_binary),
                                            ('--candidate-binary', args.candidate_binary),
                                            ('--work-root', args.work_root), ('--output', args.output)) if value is None]
        if missing:
            parser.error(f'the following arguments are required: {", ".join(missing)}')
        if args.binary is not None or args.substantial or args.control or args.verdict == 'modes':
            parser.error('--binary, --substantial, --control and --verdict modes belong to --arms campaigns')
    if args.verdict == 'normal' and not args.compare_versions:
        parser.error('--verdict normal compares Normal old against new: pass --compare-versions')
    worker_model = args.worker_model if args.worker_model is not None else (
        '' if labels else 'chatgpt/gpt-6-luna')
    paired_mode = args.verdict in ('ultrafast', 'ultrasave', 'auto', 'normal')
    baseline_modes = {'ultrafast': ('normal', 'ultracode'), 'auto': ('normal', 'ultrafast', 'ultrasave', 'ultracode')}.get(
        args.verdict, ('normal',))
    if args.compare_versions:
        if not paired_mode:
            parser.error('--compare-versions requires --verdict ultrafast, ultrasave, auto, or normal')
        baseline_modes += (args.verdict,)
    if paired_mode and not labels and (
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
    timeout_error = cohort_timeout_error(cohort['cases'])
    if timeout_error:
        parser.error(timeout_error)
    if args.execute and args.transport != 'acp':
        two_prompt = [c['id'] for c in cohort['cases'] if c.get('followup_prompt_ref')]
        if two_prompt:
            parser.error(f'two-prompt cases need --transport acp: {", ".join(two_prompt)}')
    split = {'substantial': args.substantial or [], 'control': args.control or []}
    if set(split['substantial'] + split['control']) - {c['id'] for c in cohort['cases']}:
        parser.error('--substantial/--control name cases outside the selected cohort')
    common = {'max_concurrent': args.max_concurrent,
              'model': args.model, 'effort': args.effort, 'transport': args.transport,
              'worker_model': worker_model, 'worker_effort': args.worker_effort,
              'utility_effort': args.utility_effort,
              'utility_model': args.utility_model, 'enable_jev': args.enable_jev,
              'order_seed': args.order_seed, 'ultrasave_model': args.ultrasave_model,
              'workspace_profile': args.workspace_profile}
    settings = {variant: {'ultracode': getattr(args, f'{variant}_ultracode'),
                          'execution_mode': getattr(args, f'{variant}_mode'),
                          'max_depth': getattr(args, f'{variant}_max_depth'), **common} for variant in VARIANTS}
    binaries = {'baseline': args.baseline_binary, 'candidate': args.candidate_binary}
    declared_commits = {'baseline': args.baseline_commit, 'candidate': args.candidate_commit}
    variants = VARIANTS
    if labels:
        variants = tuple(labels)
        settings = {label: {'ultracode': False, 'execution_mode': arm_mode(label), 'max_depth': None, **common}
                    for label in labels}
        binaries = {label: args.binary for label in labels}
        declared_commits = {label: args.candidate_commit for label in labels}
    elif args.aa_arm:
        variants = (*VARIANTS, AA_VARIANT)
        settings[AA_VARIANT] = settings['baseline']
        binaries[AA_VARIANT] = binaries['baseline']
        declared_commits[AA_VARIANT] = args.baseline_commit
    planned = (plan_runs(cohort, args.repetitions, args.order_seed) if variants == VARIANTS
               else plan_runs(cohort, args.repetitions, args.order_seed, variants))
    if not args.execute:
        for variant, case, rep in planned:
            extra = f' timeout_s={case_timeout(case, args.timeout):g}'
            if case.get('followup_prompt_ref'):
                extra += f' followup={case["followup_prompt_ref"]}'
            print(f'plan: {variant} {case["id"]} ({case["kind"]}) rep {rep} -> '
                  f'{binaries[variant] or "<--binary>"} {json.dumps(settings[variant], sort_keys=True)}{extra}')
        if labels:
            print(f'dry run: {len(planned)} runs planned over {len(labels)} arms ({", ".join(labels)}), '
                  f'model profile {"tiered" if args.ultrasave_model else "single"}, nothing executed; '
                  'pass --execute to run')
        else:
            print(f'dry run: {len(planned)} runs planned, nothing executed; pass --execute to run')
        return 0
    for binary in binaries.values():
        if not binary.expanduser().resolve().is_file():
            parser.error(f'binary not found: {binary}')
    binaries = {variant: binary.expanduser().resolve() for variant, binary in binaries.items()}
    frozen = comparison_hashes(cohort, binaries, args.cohort, args.workspace_profile)
    if paired_mode and not args.compare_versions and len(set(frozen['binaries'].values())) != 1:
        parser.error(f'{args.verdict} verdict requires the same binary SHA-256 for both variants')
    if frozen['support_files'][str(args.cohort.resolve())] != hashlib.sha256(cohort_bytes).hexdigest():
        raise RuntimeError('Frozen cohort changed before execution')
    profile = args.distill_profile.expanduser()
    subscription_account(profile / 'codex-auth.json')
    preflight = None
    if args.enable_jev:
        if not (profile / 'openrouter-auth.json').is_file():
            parser.error(f'--enable-jev needs {profile / "openrouter-auth.json"}')
        key = json.loads((profile / 'openrouter-auth.json').read_text()).get('key')
        if not isinstance(key, str) or not key:
            parser.error('OpenRouter auth file has no key')
        # One decision with the staged key before any run: a dead key must not
        # spend a whole campaign on Normal arms while every Jev call fails.
        preflight = jev_preflight(key)
        if not preflight['ok']:
            status = f'HTTP {preflight["status"]}' if preflight['status'] is not None else preflight['error']
            parser.error(f'Jev preflight failed ({status}); campaign aborted before the first run')
    commits = {variant: binary_commit(binary, declared_commits.get(variant)) for variant, binary in binaries.items()}
    work_root = args.work_root.resolve()
    work_root.mkdir(parents=True, exist_ok=True)
    runs = []
    for variant, case, rep in planned:
        if comparison_hashes(cohort, binaries, args.cohort, args.workspace_profile) != frozen:
            raise RuntimeError('Frozen comparison inputs or binaries changed before execution')
        run = run_one(binaries[variant], variant, case, rep, work_root, profile,
                      case_timeout(case, args.timeout), frozen_inputs=frozen['inputs'][case['id']],
                      binary_hash=frozen['binaries'][variant], **settings[variant])
        if comparison_hashes(cohort, binaries, args.cohort, args.workspace_profile) != frozen:
            raise RuntimeError('Frozen comparison inputs or binaries changed after execution')
        if not run['passed'] and args.keep_failed_runs is not None and run.get('output_dir'):
            args.keep_failed_runs.mkdir(parents=True, exist_ok=True)
            run['kept_run_dir'] = keep_failed_run(run['output_dir'], args.keep_failed_runs)
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
               'jev_preflight': preflight, 'credit_basis': CREDIT_BASIS,
               'model_profile': 'tiered' if args.ultrasave_model else 'single',
               'ultrasave_model': args.ultrasave_model,
               'runs': runs, 'aggregates': aggregate(runs),
               'failed_run_dirs': [r.get('kept_run_dir') or r.get('output_dir') for r in runs if not r['passed']],
               'binaries': {v: {'path': str(b), 'sha256': frozen['binaries'][v], **commits[v]}
                            for v, b in binaries.items()}}
    if labels:
        results['arms'] = labels
        if split['substantial'] or split['control']:
            results['ultracode_cases'] = split
        results['verdict'] = modes_verdict(results)
    else:
        paired = {**results, 'runs': [r for r in runs if r['variant'] in VARIANTS]}
        results['verdict'] = {'parallel': verdict, 'ultrafast': ultrafast_verdict, 'ultrasave': ultrasave_verdict,
                              'auto': auto_verdict, 'normal': normal_verdict}[args.verdict](paired)
    if args.aa_arm:
        results['aa_check'] = campaign_aa_check([r for r in runs if r['variant'] == 'baseline'],
                                                [r for r in runs if r['variant'] == AA_VARIANT])
        mark_aa(results['verdict'], results['aa_check'])
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
