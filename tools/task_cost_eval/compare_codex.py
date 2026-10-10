#!/usr/bin/env python3
"""Run one isolated benchmark cell using ChatGPT subscription models only."""
from __future__ import annotations

import argparse
import base64
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
# Standard subscription credits / 1M tokens. Retrieved 2026-10-10:
# https://learn.chatgpt.com/docs/pricing#token-rates
CREDIT_RATES = {'gpt-6-astra': (250, 25, 1250), 'gpt-6.1-sol': (50, 2.5, 250),
                'gpt-6-luna': (2.5, .25, 12.5)}
# Existing operator allowlist for this local-file cohort: shell file operations
# plus delegation/lifecycle. Internal IDs precede model-facing renames, and
# children inherit the restriction.
LOCAL_TOOLS = 'run_terminal_cmd,task,get_task_output,kill_task,wait_tasks'


def credits(model, incoming, cached, outgoing):
    model = model.removeprefix('chatgpt/')
    if model not in CREDIT_RATES or min(incoming, cached, outgoing) < 0 or cached > incoming:
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
                                    'is_warmup', 'warmup', 'prewarm', 'attempt', 'success',
                                    'event.timestamp', 'auth_mode', 'auth.mode', 'provider_name',
                                    'model_reasoning_effort', 'http.response.status_code', 'endpoint'}
                                or ('token' in key.lower() and not key.startswith('auth.'))):
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


def distill_config(*, main_model='chatgpt/gpt-6.1-sol', worker_effort='auto', utility_effort='auto'):
    # No third-party model credentials: auto effort retains the native fallback
    # when the optional Jev decision service is unavailable.
    return f'''[cli]
use_leader = false
[models]
default = {json.dumps(main_model)}
worker = "chatgpt/gpt-6-luna"
worker_effort = {json.dumps(worker_effort)}
session_summary = "chatgpt/gpt-6-luna"
[jev]
effort_auto = true
api_key_env = "DISTILL_BENCH_NO_EXTERNAL_MODEL_KEY"
[jev.local]
model = "chatgpt/gpt-6-luna"
effort = {json.dumps(utility_effort)}
[features]
telemetry = false
[managed_mcps]
enabled = false
[plugins]
disabled = ["clangd-lsp", "claude-blog", "stripe", "swift-lsp", "watch", "rust-analyzer-lsp"]
[skills]
ignore = ["~/.agents", "~/.codex"]
[compat.cursor]
mcps = false
rules = false
skills = false
agents = false
hooks = false
[compat.claude]
mcps = false
rules = false
skills = false
agents = false
hooks = false
'''


def distill_accounting(home, session, *, strict=False):
    if strict:
        # Independent persisted evidence, not assistant-message counts: a single
        # response can contain multiple messages, and side calls have no history.
        from collections import Counter

        reasons, artifacts = [], {}

        def read(path, *, lines=False):
            artifacts[str(path)] = path.is_file()
            try:
                value = ([json.loads(line) for line in path.read_text().split('\n') if line.strip()]
                         if lines else json.loads(path.read_text()))
                if not (all(isinstance(row, dict) for row in value) if lines else isinstance(value, dict)):
                    raise ValueError('expected JSON objects')
                return value
            except (OSError, ValueError) as error:
                reasons.append(f'{path}: {error}')
                return [] if lines else {}

        try:
            result = distill_accounting(home, session)
        except (OSError, ValueError, KeyError, TypeError) as error:
            result = {'accounting_complete': False, 'reason': f'invalid usage ledger: {error}'}
        if not result['accounting_complete']:
            reasons.append(result.get('reason', 'canonical ledger reconciliation incomplete'))
        for row in result.get('call_usage', []):
            if row['owner'] is None:
                reasons.append(f'{row["attempt"]}: ambiguous owning session in usage ledgers')
        directories = {p.parent.name: p.parent for name in ('usage.json', 'events.jsonl', 'updates.jsonl')
                       for p in sorted((home / 'sessions').rglob(name))}
        ledgers = {sid: read(path / 'usage.json').get('session', {}) for sid, path in directories.items()}
        attempts = {sid: {row['attempt_id']: row for row in ledger.get('attributions', [])}
                    for sid, ledger in ledgers.items()}
        canonical = attempts.get(session, {})
        logs = read(home / 'logs/unified.jsonl', lines=True)
        logged_sessions = {row.get('sid') for row in logs
                           if str(row.get('msg', '')).startswith('shell.turn.inference_') and row.get('sid')}
        for sid in sorted(logged_sessions - directories.keys()):
            reasons.append(f'{sid}: inference log has no session artifacts/usage ledger')
        updates, parents, metadata, spawned, finished, descendants = {}, {}, set(), set(), set(), []

        def child_record(row, parent, path):
            child = row.get('child_session_id')
            parent = row.get('parent_session_id') or parent
            if not isinstance(child, str) or not child or child == parent:
                reasons.append(f'{path}: missing/invalid child session identity')
                return None
            if child in parents and parents[child] != parent:
                reasons.append(f'{child}: conflicting parent identities')
            parents[child] = parent
            descendants.append({'parent_session_id': parent, 'child_session_id': child,
                                'attempt_id': row.get('attempt_id'), 'status': row.get('status'),
                                'artifact': str(path)})
            return child, row.get('attempt_id')

        for sid, path in directories.items():
            updates[sid] = [row.get('params', {}).get('update', {})
                            for row in read(path / 'updates.jsonl', lines=True)]
            for update in updates[sid]:
                tag = update.get('sessionUpdate')
                if tag in ('subagent_spawned', 'subagent_finished'):
                    identity = child_record(update, sid, path / 'updates.jsonl')
                    if identity:
                        (spawned if tag == 'subagent_spawned' else finished).add(identity)
            for meta_path in sorted((path / 'subagents').glob('*/meta.json')):
                meta = read(meta_path)
                identity = child_record(meta, sid, meta_path)
                if identity:
                    metadata.add(identity[0])
                    if not meta.get('completed_at'):
                        reasons.append(f'{identity[0]}: child metadata has no terminal timestamp')
        for child, parent in parents.items():
            if child not in attempts:
                reasons.append(f'{child}: observed child has no usage ledger')
            if parent not in attempts:
                reasons.append(f'{child}: parent {parent} has no usage ledger')
            if child not in metadata:
                reasons.append(f'{child}: missing child metadata')
        for child, attempt in sorted(spawned - finished, key=str):
            reasons.append(f'{child} ({attempt}): spawn has no finished notification')
        for sid in directories.keys() - parents.keys() - {session}:
            reasons.append(f'{sid}: session artifacts have no parent lifecycle evidence')

        sessions = []
        token_keys = ('prompt_tokens', 'cached_prompt_tokens', 'completion_tokens', 'reasoning_tokens')
        for sid, path in directories.items():
            ledger_rows = attempts[sid]
            for identity, row in ledger_rows.items():
                if canonical.get(identity) != row:
                    reasons.append(f'{sid}: attempt {identity} missing/different in canonical ledger')
            local_ids = {row['attempt'] for row in result.get('call_usage', [])
                         if (row['owner'] or {}).get('session_id') == sid}
            local = {identity: row for identity, row in ledger_rows.items() if identity in local_ids}
            main = {identity: row for identity, row in local.items() if row.get('role') == 'main'}
            retries = {identity for identity, row in local.items() if row.get('role') == 'main_retry'}
            evidence_logs = [row for row in logs if row.get('sid') == sid]
            starts = [row for row in evidence_logs if row.get('msg') == 'shell.turn.inference_start']
            done = [row.get('ctx', {}) for row in evidence_logs if row.get('msg') == 'shell.turn.inference_done']
            retry_ids, failed_ids = set(), set()
            for row in evidence_logs:
                if row.get('msg') not in ('shell.turn.inference_retry', 'shell.turn.inference_failed'):
                    continue
                ctx = row.get('ctx', {})
                request = ctx.get('sampler_request_id')
                if not request:
                    reasons.append(f'{sid}: inference retry/failure lacks sampler_request_id')
                    continue
                identity = f'sampler:{request}'
                if row['msg'] == 'shell.turn.inference_retry':
                    retry_ids.add(f'{identity}:retry:{ctx.get("kind")}:{ctx.get("attempt")}')
                else:
                    failed_ids.add(identity)
            completed = [row for row in main.values() if row.get('status') == 'completed']
            # inference_start is a turn submission, not a physical-attempt ID.
            # Routing can submit more than once; count it only as a lower bound.
            if not starts or len(main) < len(starts) or len(completed) != len(done):
                reasons.append(f'{sid}: foreground submissions/completions disagree with local ledger')
            usage_match = (Counter(tuple(row.get(key) for key in token_keys) for row in done)
                           == Counter(tuple((row.get('usage') or {}).get(key) for key in token_keys)
                                      for row in completed))
            if not usage_match:
                reasons.append(f'{sid}: foreground completion token evidence disagrees with ledger')
            reported_attempts = [row.get('attempts') for row in done]
            if any(type(count) is not int or count < 1 for count in reported_attempts):
                reasons.append(f'{sid}: completion evidence lacks a valid physical-attempt count')
            reported_retries = sum(count - 1 for count in reported_attempts if type(count) is int and count >= 1)
            retry_updates = sum(u.get('sessionUpdate') == 'retry_state' and u.get('type') == 'retrying'
                                for u in updates[sid])
            if retry_ids != retries or reported_retries > len(retry_ids) or retry_updates > len(retry_ids):
                reasons.append(f'{sid}: retry evidence cannot be reconciled with physical retry ledger IDs')
            if not failed_ids <= main.keys():
                reasons.append(f'{sid}: failed request IDs absent from ledger: {sorted(failed_ids - main.keys())}')
            events = read(path / 'events.jsonl', lines=True)
            turns_started = sum(row.get('type') == 'turn_started' for row in events)
            turns_ended = sum(row.get('type') == 'turn_ended' for row in events)
            if not turns_started or turns_started != turns_ended:
                reasons.append(f'{sid}: missing/unclosed turn lifecycle evidence')
            sessions.append({'session_id': sid, 'artifact_dir': str(path),
                             'local_attempt_ids': sorted(local),
                             'ledger_request_ids': sorted({r['request_id'] for r in local.values() if r.get('request_id')}),
                             'inference_starts': len(starts), 'inference_completions': len(done),
                             'ledger_main_calls': len(main), 'completion_tokens_match': usage_match,
                             'reported_retries': reported_retries, 'retry_notifications': retry_updates,
                             'retry_attempt_ids': sorted(retry_ids), 'failed_attempt_ids': sorted(failed_ids),
                             'turns_started': turns_started, 'turns_ended': turns_ended})
        # Neither successful auxiliary requests nor their absence have an
        # independent exhaustive trace in these runtimes. A ledger-only row (or
        # its deletion) cannot prove that coverage. Disclose this separately from
        # the protocol's canonical-ledger plus available-evidence completeness.
        coverage_gaps = ['Runtime lacks an independent exhaustive physical-request trace for auxiliary calls; '
                         'omitted auxiliary attempts cannot be ruled out.']
        evidence = {'complete': not reasons, 'reasons': reasons,
                    'available_evidence_matches': not reasons, 'coverage_gaps': coverage_gaps,
                    'logged_session_ids': sorted(logged_sessions),
                    'canonical_attempt_ids': sorted(canonical), 'sessions': sessions,
                    'descendants': descendants, 'artifacts': artifacts,
                    'ledger_only_attempt_ids': sorted(identity for identity, row in canonical.items()
                                                      if row.get('role') not in ('main', 'main_retry'))}
        result['accounting_evidence'] = evidence
        result['accounting_complete'] = result['accounting_complete'] and evidence['complete']
        if not result['accounting_complete']:
            result['credit_estimate'] = None
        return result

    files = list((home / 'sessions').rglob('usage.json'))
    root_file = next((p for p in files if p.parent.name == session), None)
    if root_file is None:
        return {'accounting_complete': False, 'reason': 'missing root usage ledger'}
    ledger = json.loads(root_file.read_text())['session']
    attributions = ledger.get('attributions', [])
    root_ids = {row['attempt_id'] for row in attributions}
    child_ids = set()
    usage_paths = {session: root_file}
    session_attempts = {session: root_ids}
    for path in files:
        if path == root_file:
            continue
        child = json.loads(path.read_text())['session']
        identities = {row['attempt_id'] for row in child.get('attributions', [])}
        child_ids.update(identities)
        usage_paths[path.parent.name] = path
        session_attempts[path.parent.name] = identities
    metadata = {}
    for path in files:
        for meta_path in sorted((path.parent / 'subagents').glob('*/meta.json')):
            meta = json.loads(meta_path.read_text())
            metadata.setdefault(meta.get('child_session_id'), []).append((meta, meta_path))
    # Native Task results name the child. Join by that ID and
    # toolCallId, never by prompt text, timing, model, or adjacent notifications.
    background_headers = (
        'Subagent started in background.',
        'Subagent took longer than the foreground budget and was moved to the '
        'background to keep the conversation responsive. It is still running.',
        'Subagent took longer than the foreground budget and was moved to the '
        'background to keep the conversation responsive. It is still running — you will be notified when it completes.',
    )
    task_requests = {}
    for sid, path in usage_paths.items():
        updates_path = path.parent / 'updates.jsonl'
        if not updates_path.is_file():
            continue
        requests, results = {}, []
        for line, raw in enumerate(updates_path.read_text().split('\n'), 1):
            if not raw.strip():
                continue
            params = json.loads(raw).get('params', {})
            if params.get('sessionId') != sid:  # Resume copies its source's updates.
                continue
            update = params.get('update', {})
            call_id = update.get('toolCallId')
            tool = update.get('_meta', {}).get('x.ai/tool', {})
            if (update.get('sessionUpdate') == 'tool_call' and isinstance(call_id, str)
                    and (tool.get('namespace'), tool.get('kind'), tool.get('name'))
                    == ('distill', 'task', 'spawn_subagent') and isinstance(update.get('rawInput'), dict)):
                requests.setdefault(call_id, []).append({
                    'origin': 'tool', 'tool_call_id': call_id, 'tool': tool,
                    'input': {key: value for key, value in update['rawInput'].items()
                              if key not in ('prompt', 'description')},
                    'artifact': str(updates_path), 'request_line': line})
            output = update.get('rawOutput')
            if (isinstance(output, dict) and output.get('type') == 'SubagentCompleted'
                    and isinstance(output.get('subagent_id'), str)):
                results.append((call_id, output['subagent_id'], line))
            elif (isinstance(output, dict) and output.get('type') == 'Text'
                    and call_id in requests and update.get('status') == 'completed'
                    and isinstance(output.get('text'), str)):
                # task.rs format_subagent_{started_background,auto_backgrounded}:
                # only the generated header and immediately following ID line.
                notice = output['text'].split('\n', 2)
                if (len(notice) == 3 and notice[0] in background_headers
                        and notice[1].startswith('subagent_id: ')):
                    child = notice[1].removeprefix('subagent_id: ')
                    if child and child == child.strip():
                        results.append((call_id, child, line))
        for call_id, child, line in results:
            candidates = requests.get(call_id, [])
            task_requests.setdefault((sid, child), []).append(
                {**candidates[0], 'result_line': line} if len(candidates) == 1 else None)
    # A parent ledger folds descendants. Attribute each physical call to the
    # deepest ledger supported by durable parent/child metadata, not its model.
    # Resumed sessions also copy their source ledger; those calls keep the source owner.
    owners = {}
    for sid, identities in session_attempts.items():
        folded = child_ids if sid == session else {
            identity for child, records in metadata.items() if len(records) == 1
            and records[0][0].get('parent_session_id') == sid
            for identity in session_attempts.get(child, set())}
        records = metadata.get(sid, [])
        meta, meta_path = records[0] if len(records) == 1 and sid != session else ({}, None)
        if meta.get('resumed_from'):
            sources = [source for records in metadata.values() for source, _ in records
                       if source.get('subagent_id') == meta['resumed_from']]
            if len(sources) == 1:
                folded = folded | session_attempts.get(sources[0].get('child_session_id'), set())
        requests = task_requests.get((meta.get('parent_session_id'), meta.get('subagent_id')), [])
        owner = {'session_id': sid, 'parent_session_id': meta.get('parent_session_id'),
                 'subagent_id': meta.get('subagent_id'), 'spawn_attempt_id': meta.get('attempt_id'),
                 'subagent_type': meta.get('subagent_type'),
                 'effective_model_id': meta.get('effective_model_id'),
                 'effective_context_source': meta.get('effective_context_source'),
                 'resumed_from': meta.get('resumed_from'), 'effort_auto': meta.get('effort_auto'),
                 'model_routing_locked': meta.get('model_routing_locked'),
                 'spawn_request': requests[0] if len(requests) == 1 else None,
                 'usage_path': str(usage_paths[sid]),
                 'metadata_path': str(meta_path) if meta_path is not None else None}
        for identity in identities - folded:
            owners.setdefault(identity, []).append(owner)
    rows = []
    for attribution in attributions:
        usage = attribution.get('usage')
        call_owners = owners.get(attribution['attempt_id'], [])
        identity = {'attempt': attribution['attempt_id'], 'model': attribution['model_id'],
                    'request_id': attribution.get('request_id'), 'task_id': attribution.get('task_id'),
                    'source_kind': attribution.get('source_kind'),
                    'role': attribution['role'], 'status': attribution['status'],
                    'agent': 'worker' if attribution['attempt_id'] in child_ids else 'main',
                    'endpoint': attribution.get('endpoint'),
                    'owner': call_owners[0] if len(call_owners) == 1 else None,
                    'requested_effort': attribution.get('requested_effort'),
                    'effort': attribution.get('applied_effort')}
        if not usage:
            rows.append({**identity, 'complete': False})
            continue
        incoming = usage['prompt_tokens']
        outgoing = usage['completion_tokens']
        cached = usage.get('cached_prompt_tokens', 0)
        rows.append({**identity,
                     'input_tokens': incoming, 'cached_input_tokens': cached, 'output_tokens': outgoing,
                     'reasoning_tokens': usage.get('reasoning_tokens', 0),
                     'complete': attribution.get('usage_complete', False),
                     'credits': credits(attribution['model_id'], incoming, cached, outgoing)})
    total_fields = {'input_tokens': 'inputTokens', 'output_tokens': 'outputTokens',
                    'cached_input_tokens': 'cachedReadTokens', 'reasoning_tokens': 'reasoningTokens'}
    totals_match = all(sum(row.get(key, 0) for row in rows) == ledger[value]
                       for key, value in total_fields.items())
    subscription_only = all(row.get('endpoint') == 'https://chatgpt.com/backend-api/codex/responses'
                            for row in rows)
    complete = (bool(rows) and totals_match and subscription_only and not ledger.get('usageIsIncomplete', False)
                and len(root_ids) == len(rows) == ledger['modelCalls']
                and child_ids <= root_ids and all(r['complete'] and r.get('credits') is not None for r in rows))
    return {'accounting_complete': complete, 'calls': len(rows), 'call_usage': rows,
            'ledger_totals_match': totals_match, 'subscription_only': subscription_only,
            'unfolded_child_attempts': sorted(child_ids - root_ids),
            'input_tokens': ledger['inputTokens'], 'output_tokens': ledger['outputTokens'],
            'cached_input_tokens': ledger['cachedReadTokens'], 'reasoning_tokens': ledger['reasoningTokens'],
            'credit_estimate': sum(r['credits'] for r in rows) if complete else None}


def codex_accounting(events, stdout):
    # Native response-completed telemetry is per model request, including child
    # conversations. CLI turn totals alone cannot prove complete child billing.
    logs = [json.loads(line)['fields'] for line in events.read_text().splitlines()]
    # Native timestamps distinguish requests and allow retried OTLP exports to
    # be deduplicated without discarding legitimate equal-sized model calls.
    unique = {}
    for index, log in enumerate(logs):
        key = json.dumps(log, sort_keys=True) if log.get('event.timestamp') else str(index)
        unique[key] = log
    logs = list(unique.values())
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
                     'effort': log.get('model_reasoning_effort'), 'auth_mode': log.get('auth_mode'),
                     'input_tokens': incoming, 'cached_input_tokens': cached, 'output_tokens': outgoing,
                     'reasoning_tokens': int(log.get('reasoning_token_count', 0)),
                     'credits': credits(model, incoming, cached, outgoing)})
    cli = [json.loads(line) for line in stdout.read_text().splitlines()]
    main_conversation = next((row['thread_id'] for row in cli if row.get('type') == 'thread.started'), None)
    for row in rows:
        row['agent'] = 'main' if row.get('conversation') == main_conversation else 'subagent'
    failures = any(row.get('type') in {'error', 'turn.failed'} for row in cli)
    requests = [log for log in logs if log.get('event.name') == 'codex.websocket_request']
    conversations = {log.get('conversation.id') for log in logs
                     if log.get('event.name') == 'codex.conversation_starts'}
    response_conversations = {row.get('conversation') for row in rows}
    per_conversation_match = all(
        sum(request.get('conversation.id') == conversation for request in requests)
        == sum(row.get('conversation') == conversation for row in rows)
        for conversation in conversations | response_conversations)
    counts_match = len(requests) == len(rows) and per_conversation_match
    for conversation in conversations | response_conversations:
        missing = (sum(request.get('conversation.id') == conversation for request in requests)
                   - sum(row.get('conversation') == conversation for row in rows))
        for _ in range(max(0, missing)):
            rows.append({'conversation': conversation, 'complete': False, 'status': 'missing_usage'})
    complete = (bool(rows) and not failures and counts_match
                and all(str(request.get('success')).lower() == 'true' for request in requests)
                and conversations == response_conversations
                and all(r['complete'] and r.get('credits') is not None for r in rows))
    subscription_only = all(str(row.get('auth_mode', '')).lower() == 'chatgpt' for row in rows)
    return {'accounting_complete': complete, 'calls': len(rows), 'call_usage': rows,
            'subscription_only': subscription_only, 'main_conversation': main_conversation,
            'native_requests': len(requests), 'conversations': len(conversations),
            'request_response_counts_match': counts_match,
            'parent_cli_usage': [row['usage'] for row in cli if row.get('type') == 'turn.completed'],
            'telemetry_events': len(logs),
            **{key: sum(row.get(key, 0) for row in rows)
               for key in ('input_tokens', 'cached_input_tokens', 'output_tokens', 'reasoning_tokens')},
            'credit_estimate': sum(r['credits'] for r in rows) if complete else None,
            'credit_estimate_excluding_zero_output':
                sum(r['credits'] for r in rows if r.get('output_tokens', 0) > 0) if complete else None}


def subscription_account(auth_file):
    auth = json.loads(auth_file.read_text())
    if auth.get('auth_mode') != 'chatgpt' or not auth.get('tokens') or auth.get('OPENAI_API_KEY'):
        raise ValueError('Benchmark requires ChatGPT OAuth, not a model API key')
    tokens = auth['tokens']
    payload = tokens['id_token'].split('.')[1]
    claims = json.loads(base64.urlsafe_b64decode(payload + '=' * (-len(payload) % 4)))
    return tokens.get('account_id') or claims.get('https://api.openai.com/auth', {}).get('chatgpt_account_id')


def run_cell(args):
    distill_auth = args.distill_profile.expanduser() / 'codex-auth.json'
    codex_auth = Path.home() / '.codex/auth.json'
    account = subscription_account(distill_auth)
    if not account or account != subscription_account(codex_auth):
        raise ValueError('Both agents must use the same signed-in ChatGPT account')
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    cohort = json.loads((ROOT / 'cohort-v1.json').read_text())
    case = next(c for c in cohort['cases'] if c['id'] == args.case)
    frozen = {'fixture_sha256': _sha256_tree(ROOT / case['fixture_ref']),
              'grader_sha256': _sha256_file(ROOT / case['grader']['script_ref'])}
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
    # Both agents receive the same available runtime fact for the Python cohort.
    prompt += '\nThe available Python interpreter is python3.'
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
        auth_target = home / 'codex-auth.json'
        shutil.copyfile(distill_auth, auth_target)
        auth_target.chmod(0o600)
        config = distill_config()
        (home / 'config.toml').write_text(config)
        environment['DISTILL_HOME'] = environment['GROK_HOME'] = str(home)
        command = [str(executable), '--no-leader', '--always-approve', '--disable-web-search',
                   '--cwd', str(worktree), '--session-id', session, '--prompt-file',
                   str(output / 'prompt.txt'), '--output-format', 'streaming-json',
                   '--model', 'chatgpt/gpt-6.1-sol']
        command.extend(['--tools', LOCAL_TOOLS])
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
    process = None
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
        if process is not None and process.poll() is None:
            _terminate_owned_group(process)
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
    inputs_unchanged = (frozen['fixture_sha256'] == _sha256_tree(ROOT / case['fixture_ref'])
                        and frozen['grader_sha256'] == _sha256_file(ROOT / case['grader']['script_ref']))
    native_payloads = list((executable.parent.parent / 'node_modules/@openai').glob(
        'codex-*/vendor/*/bin/codex')) if executable.suffix == '.js' else [executable]
    if len(native_payloads) != 1:
        raise ValueError('Cannot identify the native benchmark executable')
    summary = {'variant': args.variant, 'case': args.case, 'session': session,
               'provider': 'chatgpt_subscription', 'version': version,
               'same_subscription_account': True,
               'executable_sha256': _sha256_file(executable),
               'native_payload_sha256': _sha256_file(native_payloads[0]),
               'source_sha': subprocess.check_output(['git', 'rev-parse', version.split('(')[-1].rstrip(')')],
                                                     cwd=ROOT, text=True).strip() if args.variant == 'distill' else None,
               'harness_source_sha': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
               'harness_sha256': _sha256_file(Path(__file__)),
               'prompt_sha256': hashlib.sha256(prompt.encode()).hexdigest(),
               **frozen, 'frozen_inputs_unchanged': inputs_unchanged,
               'config_sha256': hashlib.sha256(config.encode()).hexdigest(),
               'wall_time_s': round(elapsed, 6), 'exit_code': process.returncode,
               'timed_out': timed_out, 'grader_passed': grade.returncode == 0,
               'cost_basis': 'published_subscription_credit_rates', **accounting}
    (output / 'result.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps({k: v for k, v in summary.items() if k != 'call_usage'}), flush=True)
    return 0 if (process.returncode == 0 and grade.returncode == 0 and inputs_unchanged
                 and accounting['accounting_complete'] and accounting['subscription_only']) else 1


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
