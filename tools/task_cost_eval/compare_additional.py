#!/usr/bin/env python3
"""Measure isolated Claude and DeepSeek cells against frozen external graders."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import time
import uuid

from .acp_driver import EXECUTION_MODES, OwnedTree
from .compare_codex import (ROOT, LOCAL_TOOLS, allowed_models, distill_accounting, distill_config,
                            mode_activated, mode_flags)
from .evaluate import _sha256_file, _sha256_tree
from .runner import _terminate_owned_group

SONNET = 'claude/claude-sonnet-5-5'
OPUS = 'claude/claude-opus-5-5'
HAIKU = 'claude/claude-haiku-5-5'
DEEPSEEK = 'deepseek-v4.1-flash'
FREE_WORKER = 'openrouter-free'
NATIVE_VARIANTS = ('claude-native', 'opus-native')


def config(main, *, ablation=False, worker=None, enable_jev=None):
    worker = worker or main
    enable_jev = False if enable_jev is None else enable_jev
    text = distill_config(main_model=main, worker_model=worker,
                          worker_effort='low' if ablation else 'auto', enable_jev=enable_jev)
    text = text.replace('session_summary = "chatgpt/gpt-6-luna"', f'session_summary = "{worker}"')
    text = text.replace('model = "chatgpt/gpt-6-luna"', f'model = "{worker}"')
    if ablation:
        text = text.replace('enabled = true', 'enabled = false').replace('effort_auto = true', 'effort_auto = false')
    if worker == FREE_WORKER:
        text += '''
[model."openrouter-free"]
model = "openrouter/free"
base_url = "https://openrouter.ai/api/v1"
api_backend = "chat_completions"
env_key = "OPENROUTER_API_KEY"
context_window = 200000
max_completion_tokens = 16384
supports_reasoning_effort = true
reasoning_effort = "low"
supports_backend_search = false
stream_tool_calls = false
max_retries = 2
'''
    if main == DEEPSEEK:
        text += '''
[model."deepseek-v4.1-flash"]
model = "deepseek/deepseek-v4.1-flash"
base_url = "https://openrouter.ai/api/v1"
api_backend = "chat_completions"
env_key = "OPENROUTER_API_KEY"
context_window = 1000000
max_completion_tokens = 131072
supports_reasoning_effort = true
reasoning_effort = "max"
reasoning_efforts = [{id="max",value="max",label="Max",default=true},{id="high",value="high",label="High"},{id="low",value="low",label="Low"},{id="none",value="none",label="None"}]
supports_backend_search = false
stream_tool_calls = false
max_retries = 2
'''
    return text


def native_usage(path):
    try:
        data = json.loads(path.read_text())
        models = data.get('modelUsage', {})
        keys = ('inputTokens', 'outputTokens', 'cacheReadInputTokens', 'cacheCreationInputTokens')
        rows = [{'model': model, **row} for model, row in models.items()]
        complete = bool(rows) and all(all(isinstance(r.get(k), int) and r[k] >= 0 for k in keys) for r in rows)
        cost = data.get('total_cost_usd')
        for row in rows:
            row['usage'] = {'prompt_tokens': row.get('inputTokens', 0) + row.get('cacheReadInputTokens', 0) + row.get('cacheCreationInputTokens', 0),
                            'cached_prompt_tokens': row.get('cacheReadInputTokens', 0),
                            'cache_creation_prompt_tokens': row.get('cacheCreationInputTokens', 0),
                            'completion_tokens': row.get('outputTokens', 0)}
        return {'accounting_complete': complete, 'reference_cost_usd': comparison_cost(rows) if complete else None, 'call_usage': rows,
                'cost_usd': cost if complete and isinstance(cost, (int, float)) and cost >= 0 else None,
                'credit_estimate': None,
                'observed_models': list(models), 'native_is_error': data.get('is_error', True),
                'reason': None if complete else 'native modelUsage missing or incomplete'}
    except (OSError, ValueError, TypeError) as error:
        return {'accounting_complete': False, 'cost_usd': None, 'call_usage': [], 'reason': str(error)}


def comparison_cost(rows):
    """Return reference API cost, or None when any call lacks a defensible rate."""
    from .compare_codex import credits

    total = 0.0
    for row in rows:
        model = row.get('model', '')
        native_list_cost = row.get('costUSD')
        if (str(model).startswith('claude-') and row.get('costBasis') == 'list'
                and isinstance(native_list_cost, (int, float)) and not isinstance(native_list_cost, bool)
                and 0 <= native_list_cost < float('inf')):
            total += native_list_cost
            continue
        usage = row.get('usage') or {}
        ticks = row.get('cost_usd_ticks')
        if model.startswith(('jev/', 'typesafe/jev', '~typesafe/jev')) or model.endswith(':free') or model == 'openrouter/free':
            if type(ticks) is not int or ticks < 0:
                return None
            if (model.endswith(':free') or model == 'openrouter/free') and ticks != 0:
                return None
            total += ticks / 1e10
            continue
        if not usage:
            usage = {'prompt_tokens': row.get('input_tokens'), 'cached_prompt_tokens': row.get('cached_input_tokens', 0),
                     'completion_tokens': row.get('output_tokens'), 'cache_creation_prompt_tokens': row.get('cache_creation_tokens', 0)}
        prompt = usage.get('prompt_tokens')
        cached = usage.get('cached_prompt_tokens', 0)
        output = usage.get('completion_tokens')
        if not all(type(v) is int and v >= 0 for v in (prompt, cached, output)) or cached > prompt:
            return None
        model_id = model.removeprefix('claude/').removeprefix('chatgpt/')
        if model_id in ('gpt-6-astra', 'gpt-6.1-sol', 'gpt-6-luna'):
            estimate = credits(model_id, prompt, cached, output)
            if estimate is None:
                return None
            total += estimate * .04
            continue
        rates = {
            'claude-sonnet-5-5': (2, .1, 2.5, 4, 10),
            'claude-opus-5-5': (4, .2, 5, 8, 20),
            'claude-haiku-5-5': (.1, .01, .125, .2, .5),
            'deepseek/deepseek-v4.1-flash': (.036, .0054, 0, 0, .6435),
        }.get(model_id)
        if rates is None:
            return None
        write_5m = usage.get('cache_creation_prompt_tokens', 0)
        write_1h = usage.get('cache_creation_1h_prompt_tokens', 0)
        if not all(type(v) is int and v >= 0 for v in (write_5m, write_1h)) or write_1h > write_5m or write_5m > prompt - cached:
            return None
        base_rate, read_rate, write_rate, hour_rate, output_rate = rates
        if model_id == 'claude-haiku-5-5' and prompt > 100_000:
            base_rate, read_rate, write_rate, hour_rate, output_rate = (.5, .05, .625, 1, 2.5)
        uncached_base = prompt - cached - write_5m
        total += (uncached_base * base_rate + cached * read_rate
                  + (write_5m - write_1h) * write_rate + write_1h * hour_rate
                  + output * output_rate) / 1e6
    return total


def provider_usage(home, session):
    result = distill_accounting(home, session)
    rows = result.get('call_usage', [])
    ledgers = list((home / 'sessions').rglob('usage.json'))
    root = next((p for p in ledgers if p.parent.name == session), None)
    ledger = json.loads(root.read_text())['session'] if root else {}
    known = {'claude-sonnet-5-5', 'claude-opus-5-5', 'claude-haiku-5-5',
             'deepseek/deepseek-v4.1-flash', 'gpt-6-astra', 'gpt-6.1-sol', 'gpt-6-luna'}
    complete = (bool(rows) and result.get('ledger_totals_match', False)
                and (not ledger.get('usageIsIncomplete', False) or result.get('incomplete_explained', False))
                and len({r['attempt'] for r in rows}) == len(rows) == ledger.get('modelCalls')
                and not result.get('unfolded_child_attempts')
                and all(r.get('complete') and r.get('status') in ('completed', 'rejected') and r.get('owner')
                        and (r.get('model') in known or r.get('model', '').startswith(('jev/', 'typesafe/jev', '~typesafe/jev')) or r.get('model', '').endswith(':free')) for r in rows))
    attributions = ledger.get('attributions', [])
    by_attempt_id = {r.get('attempt_id'): r for r in attributions}
    for row in rows:
        attribution = by_attempt_id.get(row['attempt'], {})
        row['cost_usd_ticks'] = attribution.get('cost_usd_ticks')
        row['cost_basis'] = attribution.get('cost_basis')
        row['usage'] = attribution.get('usage')
    reference_cost = comparison_cost(rows) if complete else None
    costs = [r.get('cost_usd_ticks') for r in rows]
    cost_complete = complete and all(isinstance(c, int) and c >= 0 for c in costs)
    result.update(accounting_complete=bool(complete), credit_estimate=None,
                  cost_usd=sum(costs) / 1e10 if cost_complete else None,
                  reference_cost_usd=reference_cost,
                  cache_creation_tokens=ledger.get('cacheCreationTokens'),
                  cost_bases=sorted({r.get('cost_basis', 'unknown') for r in rows}))
    result['observed_models'] = sorted({r['model'] for r in rows})
    result['cost_basis'] = 'native per-attempt USD ticks (1e10 per dollar)' if cost_complete else ('unavailable; missing native per-attempt cost' if result['cost_usd'] is None else 'native per-attempt USD ticks (1e10 per dollar)')
    return result


def run(args):
    mode = getattr(args, 'mode', None)
    if mode is not None and args.variant in NATIVE_VARIANTS:
        raise ValueError('--mode applies only to Distill variants')
    case = next(r for r in json.loads((ROOT / 'cohort-v1.json').read_text())['cases'] if r['id'] == args.case)
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    home, work = out / 'isolated-home', out / 'worktree'
    home.mkdir(mode=0o700)
    target = work / 'tools/task_cost_eval' / case['fixture_ref']
    target.parent.mkdir(parents=True)
    shutil.copytree(ROOT / case['fixture_ref'], target)
    subprocess.run(['git', 'init', '-q', str(work)], check=True)
    subprocess.run(['git', 'add', '.'], cwd=work, check=True)
    subprocess.run(['git', '-c', 'user.name=Benchmark', '-c', 'user.email=benchmark@localhost', 'commit', '-qm', 'Frozen task input'], cwd=work, check=True)
    frozen = {'fixture_sha256': _sha256_tree(ROOT / case['fixture_ref']),
              'grader_sha256': _sha256_file(ROOT / case['grader']['script_ref'])}
    prompt = (ROOT / case['prompt_ref']).read_text()
    prompt += '\nWork only in this repository. Finish the code change autonomously. Do not commit.'
    prompt += '\nThe available Python interpreter is python3.'
    (out / 'prompt.txt').write_text(prompt)
    session = str(uuid.uuid4())
    native = args.variant in NATIVE_VARIANTS
    ablation = args.variant == 'deepseek-same-agent-ablation'
    model = {'sonnet-distill': SONNET, 'claude-native': SONNET,
             'opus-distill': OPUS, 'opus-native': OPUS,
             'deepseek-distill': DEEPSEEK,
             'deepseek-free-distill': DEEPSEEK,
             'deepseek-same-agent-ablation': DEEPSEEK}[args.variant]
    worker = FREE_WORKER if args.variant == 'deepseek-free-distill' else (HAIKU if args.variant == 'opus-distill' else model)
    executable = Path(args.claude if native else args.distill).resolve()
    version = subprocess.check_output([str(executable), '--version'], text=True).strip()
    result = {'variant': args.variant, 'case': args.case, 'session': session, 'model': model,
              'worker_model': worker if not native else None, 'requested_effort': 'max' if args.variant == 'opus-native' else ('medium' if native else 'auto'),
              'execution_mode': mode, 'execution_mode_activation_confirmed': None,
              'jev_enabled': bool(args.enable_jev and not native), 'version': version, 'executable_sha256': _sha256_file(executable),
              'harness_sha256': _sha256_file(Path(__file__)),
              'prompt_sha256': hashlib.sha256(prompt.encode()).hexdigest(), **frozen,
              'accounting_complete': False, 'cost_usd': None, 'wall_time_s': 0,
              'exit_code': None, 'timed_out': False}
    env = {k: v for k, v in os.environ.items()
           if not k.endswith('_API_KEY') and not k.startswith(('GROK_', 'DISTILL_', 'CLAUDE_', 'ANTHROPIC_'))}
    env.update(HOME=str(home), DISTILL_HOME=str(home), GROK_HOME=str(home),
               CLAUDE_CONFIG_DIR=str(home / 'claude'), GROK_MANAGED_MCPS_ENABLED='false',
               GROK_MANAGED_MCP_GATEWAY_TOOLS_ENABLED='false')
    proc = None
    tree = None
    result['cleanup_complete'] = False
    started = None
    credentials = []
    try:
        if native:
            auth = json.loads(args.claude_auth.read_text())
            env['CLAUDE_CODE_OAUTH_TOKEN'] = auth['access_token']
            command = [str(executable), '-p', '--output-format', 'json', '--model', model.removeprefix('claude/'),
                       '--effort', 'max' if args.variant == 'opus-native' else 'medium', '--dangerously-skip-permissions', '--setting-sources', '',
                       '--strict-mcp-config', '--mcp-config', '{"mcpServers":{}}',
                       '--no-session-persistence', '--tools', 'Bash,Read,Edit,Write,Glob,Grep,Agent', '--', prompt]
        else:
            provider_auth = 'claude-auth.json' if model in (SONNET, OPUS) else 'openrouter-auth.json'
            names = ['auth.json', provider_auth, 'campaigns_state.json']
            if args.enable_jev and 'openrouter-auth.json' not in names:
                names.append('openrouter-auth.json')
            for name in names:
                source = args.claude_auth if name == 'claude-auth.json' else args.distill_profile / name
                if source.exists():
                    credentials.append(home / name)
                    shutil.copyfile(source, home / name)
                    (home / name).chmod(0o600)
            if args.enable_jev:
                env['OPENROUTER_API_KEY'] = json.loads((args.distill_profile / 'openrouter-auth.json').read_text())['key']
            text = config(model, ablation=ablation, worker=worker, enable_jev=args.enable_jev)
            (home / 'config.toml').write_text(text)
            result['config_sha256'] = hashlib.sha256(text.encode()).hexdigest()
            command = [str(executable), '--no-leader', '--always-approve', '--disable-web-search',
                       '--cwd', str(work), '--session-id', session, '--prompt-file', str(out / 'prompt.txt'),
                       '--output-format', 'streaming-json', '--model', model,
                       '--tools', 'run_terminal_cmd' if ablation else LOCAL_TOOLS]
            if ablation:
                command.extend(['--effort', 'low'])
            command.extend(mode_flags(mode))
        (out / 'command.json').write_text(json.dumps(command, indent=2) + '\n')
        started = time.monotonic()
        with (out / 'stdout.txt').open('w') as stdout, (out / 'stderr.txt').open('w') as stderr:
            proc = subprocess.Popen(command, cwd=work, env=env, stdout=stdout, stderr=stderr,
                                    stdin=subprocess.DEVNULL, start_new_session=True)
            tree = OwnedTree(proc)
            try:
                while proc.poll() is None:
                    tree.refresh()
                    if time.monotonic() - started >= args.timeout:
                        result['timed_out'] = True
                        break
                    time.sleep(.02)
            finally:
                result['cleanup_complete'] = tree.stop()
            if not result['cleanup_complete']:
                raise RuntimeError('Inference process tree did not terminate')
        result.update(wall_time_s=round(time.monotonic() - started, 6), exit_code=proc.returncode,
                      execution_mode_activation_confirmed=mode_activated(out / 'stderr.txt', mode, session))
        usage = native_usage(out / 'stdout.txt') if native else provider_usage(home, session)
        result.update(usage)
        expected = 'deepseek/deepseek-v4.1-flash' if model == DEEPSEEK else model.removeprefix('claude/')
        if native:
            observed = result.get('observed_models', [])
            result['model_verified'] = expected in observed
        else:
            main_calls = [r for r in result.get('call_usage', []) if r.get('agent') == 'main' and r.get('role') in ('main', 'main_retry')]
            children = [r for r in result.get('call_usage', []) if r.get('agent') == 'worker' and r.get('role') != 'jev']
            # The free router has no single model id: any zero-cost `:free` model is the worker.
            free_worker = worker == FREE_WORKER
            root_models, child_models = allowed_models(
                mode, expected, None if free_worker else worker.removeprefix('claude/'))

            def on(models, row):
                is_free = row['model'].endswith(':free') and row.get('cost_usd_ticks') == 0
                return row['model'] in models or (free_worker and None in models and is_free)
            result['model_verified'] = bool(main_calls) and all(on(root_models, r) for r in main_calls)
            result['worker_used'] = bool(children)
            result['worker_verified'] = all(on(child_models, r) for r in children)
            result['model_verified'] = result['model_verified'] and result['worker_verified']
    except Exception as error:
        result['error'] = f'{type(error).__name__}: {error}'
    finally:
        if proc is not None and proc.poll() is None:
            _terminate_owned_group(proc)
        for path in credentials:
            path.unlink(missing_ok=True)
        if started is not None and result['wall_time_s'] == 0:
            result['wall_time_s'] = round(time.monotonic() - started, 6)
    if not result['cleanup_complete']:
        result['passed'] = False
        result['grader_passed'] = False
        (out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        return 1
    grade = subprocess.run(['python3', str(ROOT / case['grader']['script_ref']), str(work)], capture_output=True, text=True, timeout=60)
    (out / 'grader.txt').write_text(grade.stdout + grade.stderr)
    (out / 'diff.patch').write_text(subprocess.check_output(['git', 'diff', '--no-ext-diff'], cwd=work, text=True))
    result['grader_passed'] = grade.returncode == 0
    result['frozen_inputs_unchanged'] = frozen == {'fixture_sha256': _sha256_tree(ROOT / case['fixture_ref']), 'grader_sha256': _sha256_file(ROOT / case['grader']['script_ref'])}
    result['passed'] = (result['exit_code'] == 0 and not result['timed_out'] and result['grader_passed']
                        and result.get('model_verified', False) and result['frozen_inputs_unchanged']
                        and not result.get('native_is_error', False)
                        and result['execution_mode_activation_confirmed'] is not False)
    (out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps({k:v for k,v in result.items() if k != 'call_usage'}), flush=True)
    return 0 if result['passed'] and result['accounting_complete'] and result.get('reference_cost_usd') is not None else 1


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--variant', choices=('sonnet-distill', 'claude-native', 'deepseek-distill', 'deepseek-same-agent-ablation', 'opus-distill', 'opus-native', 'deepseek-free-distill'), required=True)
    parser.add_argument('--enable-jev', action='store_true')
    parser.add_argument('--case', required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--distill', type=Path, default=Path(shutil.which('distill')))
    parser.add_argument('--claude', type=Path, default=Path(shutil.which('claude')))
    parser.add_argument('--distill-profile', type=Path, default=Path.home() / '.distill')
    parser.add_argument('--claude-auth', type=Path, default=Path.home() / '.distill/claude-auth.json')
    parser.add_argument('--timeout', type=float, default=300)
    parser.add_argument('--mode', choices=EXECUTION_MODES,
                        help='Distill variants only: pin this execution mode; omitted, no mode flag is passed')
    args = parser.parse_args(argv)
    if args.mode is not None and args.variant in NATIVE_VARIANTS:
        parser.error('--mode applies only to Distill variants')
    return run(args)


if __name__ == '__main__':
    raise SystemExit(main())
