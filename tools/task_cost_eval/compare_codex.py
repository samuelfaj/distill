#!/usr/bin/env python3
"""Run one isolated benchmark cell using ChatGPT subscription models only."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from .evaluate import _sha256_file, _sha256_tree
from .runner import _terminate_owned_group

ROOT = Path(__file__).resolve().parent
# Standard subscription credits / 1M tokens. Retrieved 2026-09-30:
# https://learn.chatgpt.com/docs/pricing#token-rates
CREDIT_RATES = {'gpt-6.1-sol': (50, 2.5, 250), 'gpt-6-luna': (2.5, .25, 12.5)}


def credits(model, incoming, cached, outgoing):
    model = model.removeprefix('chatgpt/')
    if model not in CREDIT_RATES:
        return None
    rates = CREDIT_RATES[model]
    return ((incoming - cached) * rates[0] + cached * rates[1] + outgoing * rates[2]) / 1_000_000


class Collector(ThreadingHTTPServer):
    """Receive native Codex OTLP logs; this never forwards inference requests."""
    daemon_threads = True

    def __init__(self, output):
        super().__init__(('127.0.0.1', 0), CollectorHandler)
        self.output = output
        self.lock = threading.Lock()
        output.touch()


class CollectorHandler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        value = json.loads(self.rfile.read(int(self.headers.get('Content-Length', '0'))))
        rows = []
        for resource in value.get('resourceLogs', []):
            for scope in resource.get('scopeLogs', []):
                for log in scope.get('logRecords', []):
                    fields = {}
                    for attribute in log.get('attributes', []):
                        key = attribute['key']
                        if (key in {'event.name', 'event_name', 'event.kind', 'event_kind',
                                    'event', 'model', 'model_provider', 'reasoning_effort',
                                    'conversation.id', 'conversation_id', 'session.id', 'session_id',
                                    'response_id', 'request_id', 'http.status_code', 'status',
                                    'is_warmup', 'warmup', 'prewarm', 'attempt', 'success'}
                                or ('token' in key.lower() and not key.startswith('auth.'))
                                or key.endswith('.id')):
                            fields[key] = next(iter(attribute.get('value', {}).values()), None)
                    rows.append({'time': log.get('timeUnixNano'), 'fields': fields,
                                 'field_names': [a['key'] for a in log.get('attributes', [])]})
        with self.server.lock, self.server.output.open('a') as stream:
            for row in rows:
                stream.write(json.dumps(row) + '\n')
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.end_headers()
        self.wfile.write(b'{}')


def distill_config():
    # No third-party model credentials: auto effort retains the native fallback
    # when the optional Jev decision service is unavailable.
    return '''[cli]
use_leader = false
[models]
default = "chatgpt/gpt-6.1-sol"
worker = "chatgpt/gpt-6-luna"
worker_effort = "auto"
session_summary = "chatgpt/gpt-6.1-sol"
[jev]
effort_auto = true
api_key_env = "DISTILL_BENCH_NO_EXTERNAL_MODEL_KEY"
[features]
telemetry = false
[managed_mcps]
enabled = false
[compat.cursor]
mcps = false
[compat.claude]
mcps = false
'''


def distill_accounting(home, session):
    files = list((home / 'sessions').rglob('usage.json'))
    root_file = next((p for p in files if p.parent.name == session), None)
    if root_file is None:
        return {'accounting_complete': False, 'reason': 'missing root usage ledger'}
    ledger = json.loads(root_file.read_text())['session']
    attributions = ledger.get('attributions', [])
    root_ids = {row['attempt_id'] for row in attributions}
    child_ids = set()
    for path in files:
        if path == root_file:
            continue
        child = json.loads(path.read_text())['session']
        child_ids.update(row['attempt_id'] for row in child.get('attributions', []))
    rows = []
    for attribution in attributions:
        usage = attribution.get('usage')
        if not usage:
            rows.append({'attempt': attribution['attempt_id'], 'complete': False})
            continue
        incoming = usage['prompt_tokens']
        outgoing = usage['completion_tokens']
        cached = usage.get('cached_prompt_tokens', 0)
        rows.append({'attempt': attribution['attempt_id'], 'model': attribution['model_id'],
                     'role': attribution['role'], 'effort': attribution.get('applied_effort'),
                     'input_tokens': incoming, 'cached_input_tokens': cached, 'output_tokens': outgoing,
                     'reasoning_tokens': usage.get('reasoning_tokens', 0),
                     'complete': attribution.get('usage_complete', False),
                     'credits': credits(attribution['model_id'], incoming, cached, outgoing)})
    complete = (bool(rows) and not ledger.get('usageIsIncomplete', False)
                and len(root_ids) == len(rows) == ledger['modelCalls']
                and child_ids <= root_ids and all(r['complete'] and r.get('credits') is not None for r in rows))
    return {'accounting_complete': complete, 'calls': len(rows), 'call_usage': rows,
            'unfolded_child_attempts': sorted(child_ids - root_ids),
            'input_tokens': ledger['inputTokens'], 'output_tokens': ledger['outputTokens'],
            'cached_input_tokens': ledger['cachedReadTokens'], 'reasoning_tokens': ledger['reasoningTokens'],
            'credit_estimate': sum(r['credits'] for r in rows) if complete else None}


def codex_accounting(events, stdout):
    # Native response-completed telemetry is per model request, including child
    # conversations. CLI turn totals alone cannot prove complete child billing.
    logs = [json.loads(line)['fields'] for line in events.read_text().splitlines()]
    rows = []
    for log in logs:
        if log.get('event.name') != 'codex.sse_event' or log.get('event.kind') != 'response.completed':
            continue
        required = ('input_token_count', 'cached_token_count', 'output_token_count')
        if not all(key in log for key in required):
            rows.append({'complete': False, 'conversation': log.get('conversation.id')})
            continue
        incoming, cached, outgoing = (int(log[key]) for key in required)
        model = log['model']
        rows.append({'model': model, 'conversation': log.get('conversation.id'), 'complete': True,
                     'input_tokens': incoming, 'cached_input_tokens': cached, 'output_tokens': outgoing,
                     'reasoning_tokens': int(log.get('reasoning_token_count', 0)),
                     'credits': credits(model, incoming, cached, outgoing)})
    failures = any(json.loads(line).get('type') in {'error', 'turn.failed'}
                   for line in stdout.read_text().splitlines())
    complete = bool(rows) and not failures and all(r['complete'] and r.get('credits') is not None for r in rows)
    return {'accounting_complete': complete, 'calls': len(rows), 'call_usage': rows,
            'telemetry_events': len(logs),
            **{key: sum(row.get(key, 0) for row in rows)
               for key in ('input_tokens', 'cached_input_tokens', 'output_tokens', 'reasoning_tokens')},
            'credit_estimate': sum(r['credits'] for r in rows) if complete else None}


def run_cell(args):
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    cohort = json.loads((ROOT / 'cohort-v1.json').read_text())
    case = next(c for c in cohort['cases'] if c['id'] == args.case)
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
    (output / 'prompt.txt').write_text(prompt)
    executable = args.distill.resolve() if args.variant == 'distill' else args.codex.resolve()
    version = subprocess.check_output([str(executable), '--version'], text=True).strip()
    home = output / 'distill-home'
    home.mkdir()
    collector = Collector(output / 'telemetry.jsonl')
    thread = threading.Thread(target=collector.serve_forever, daemon=True)
    thread.start()
    session = str(uuid.uuid4())
    environment = {k: v for k, v in os.environ.items()
                   if not k.endswith('_API_KEY') and k != 'DISTILL_BENCH_NO_EXTERNAL_MODEL_KEY'}
    environment['PATH'] = str(args.codex.expanduser().parent) + os.pathsep + environment['PATH']
    environment['GROK_MANAGED_MCPS_ENABLED'] = 'false'
    environment['GROK_MANAGED_MCP_GATEWAY_TOOLS_ENABLED'] = 'false'
    if args.variant == 'distill':
        auth_source = args.distill_profile.expanduser() / 'codex-auth.json'
        auth = json.loads(auth_source.read_text())
        if not auth.get('tokens') or auth.get('OPENAI_API_KEY'):
            raise ValueError('Distill must be signed in with ChatGPT OAuth, not an API key')
        auth_target = home / 'codex-auth.json'
        shutil.copyfile(auth_source, auth_target)
        auth_target.chmod(0o600)
        config = distill_config()
        (home / 'config.toml').write_text(config)
        environment['DISTILL_HOME'] = environment['GROK_HOME'] = str(home)
        command = [str(executable), '--no-leader', '--always-approve', '--disable-web-search',
                   '--cwd', str(worktree), '--session-id', session, '--prompt-file',
                   str(output / 'prompt.txt'), '--output-format', 'streaming-json',
                   '--model', 'chatgpt/gpt-6.1-sol']
    else:
        endpoint = f'http://127.0.0.1:{collector.server_port}/v1/logs'
        config = 'model="gpt-6.1-sol"\nmodel_reasoning_effort="high"\nforced_login_method="chatgpt"\n'
        command = [str(executable), 'exec', '--yolo', '--ignore-user-config', '--ephemeral',
                   '--json', '-C', str(worktree), '-m', 'gpt-6.1-sol',
                   '-c', 'model_reasoning_effort="high"', '-c', 'forced_login_method="chatgpt"',
                   '-c', 'web_search="disabled"',
                   '-c', f'otel.exporter={{otlp-http={{endpoint="{endpoint}",protocol="json"}}}}',
                   '-c', 'otel.log_user_prompt=false', '-']
    (output / 'config.toml').write_text(config)
    (output / 'command.json').write_text(json.dumps(command, indent=2) + '\n')
    started = time.monotonic()
    timed_out = False
    try:
        with (output / 'stdout.jsonl').open('w') as stdout, (output / 'stderr.txt').open('w') as stderr:
            process = subprocess.Popen(command, env=environment, cwd=worktree, stdin=subprocess.PIPE,
                                       stdout=stdout, stderr=stderr, start_new_session=True, text=True)
            try:
                process.communicate(prompt if args.variant == 'codex' else None, timeout=args.timeout)
            except subprocess.TimeoutExpired:
                timed_out = True
                _terminate_owned_group(process)
    finally:
        elapsed = time.monotonic() - started
        collector.shutdown()
        collector.server_close()
        thread.join()
        if args.variant == 'distill':
            auth_target.unlink(missing_ok=True)
    grade = subprocess.run(['python3', str(ROOT / case['grader']['script_ref']), str(worktree)],
                           text=True, capture_output=True)
    (output / 'grader.txt').write_text(grade.stdout + grade.stderr)
    diff = subprocess.check_output(['git', 'diff', '--no-ext-diff'], cwd=worktree, text=True)
    (output / 'diff.patch').write_text(diff)
    accounting = (distill_accounting(home, session) if args.variant == 'distill'
                  else codex_accounting(output / 'telemetry.jsonl', output / 'stdout.jsonl'))
    summary = {'variant': args.variant, 'case': args.case, 'session': session,
               'provider': 'chatgpt_subscription', 'version': version,
               'executable_sha256': _sha256_file(executable),
               'source_sha': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
               'prompt_sha256': hashlib.sha256(prompt.encode()).hexdigest(),
               'fixture_sha256': _sha256_tree(ROOT / case['fixture_ref']),
               'grader_sha256': _sha256_file(ROOT / case['grader']['script_ref']),
               'config_sha256': hashlib.sha256(config.encode()).hexdigest(),
               'wall_time_s': round(elapsed, 6), 'exit_code': process.returncode,
               'timed_out': timed_out, 'grader_passed': grade.returncode == 0,
               'cost_basis': 'published_subscription_credit_rates', **accounting}
    (output / 'result.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps({k: v for k, v in summary.items() if k != 'call_usage'}), flush=True)
    return 0 if process.returncode == 0 and grade.returncode == 0 and accounting['accounting_complete'] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--variant', choices=('distill', 'codex'), required=True)
    parser.add_argument('--case', required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--distill', type=Path, default=Path('target/release/distill'))
    parser.add_argument('--codex', type=Path, default=Path(shutil.which('codex')))
    parser.add_argument('--distill-profile', type=Path, default=Path.home() / '.grok')
    parser.add_argument('--timeout', type=float, default=300)
    return run_cell(parser.parse_args())


if __name__ == '__main__':
    raise SystemExit(main())
