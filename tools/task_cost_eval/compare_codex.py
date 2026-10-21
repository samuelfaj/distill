#!/usr/bin/env python3
"""Run one isolated Codex or Distill task-cost benchmark cell."""
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

from .acp_driver import OwnedTree
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
                                    'response_id', 'response.id', 'request_id', 'http.status_code', 'status', 'cost', 'cost_usd', 'cost_metadata', 'usage.cost',
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


def distill_config(*, main_model='chatgpt/gpt-6.1-sol', worker_model='chatgpt/gpt-6-luna',
                   worker_effort='auto', utility_effort='auto', enable_jev=False,
                   utility_model='chatgpt/gpt-6-luna'):
    return f'''[cli]
use_leader = false
[models]
default = {json.dumps(main_model)}
worker = {json.dumps(worker_model)}
worker_effort = {json.dumps(worker_effort)}
session_summary = {json.dumps(utility_model)}
[jev]
enabled = true
effort_auto = true
api_key_env = {json.dumps("OPENROUTER_API_KEY" if enable_jev else "DISTILL_BENCH_NO_EXTERNAL_MODEL_KEY")}
[jev.local]
model = {json.dumps(utility_model)}
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


def distill_accounting(home, session, *, strict=False, zero_usage_attempts=frozenset()):
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

        directories = {p.parent.name: p.parent for name in ('usage.json', 'events.jsonl', 'updates.jsonl')
                       for p in sorted((home / 'sessions').rglob(name))}
        logs = read(home / 'logs/unified.jsonl', lines=True)
        # A retry the runtime observed before any channel emitted output cannot
        # have produced billable output; the provider failed before generation.
        # Only those attempts, each corroborated by its own retry log line, may
        # be reconciled as known zero-usage calls. An attempt that had already
        # started streaming keeps its unknown usage and the ledger stays
        # incomplete.
        corroborated = set()
        for row in logs:
            if row.get('msg') != 'shell.turn.inference_retry':
                continue
            ctx = row.get('ctx', {})
            if ctx.get('output_started') is not False:
                continue
            request = ctx.get('sampler_request_id')
            if request:
                corroborated.add(
                    f'sampler:{request}:retry:{ctx.get("kind")}:{ctx.get("attempt")}')
        zero_usage_attempts = frozenset(zero_usage_attempts) | corroborated
        try:
            result = distill_accounting(
                home, session, zero_usage_attempts=zero_usage_attempts)
        except (OSError, ValueError, KeyError, TypeError) as error:
            result = {'accounting_complete': False, 'reason': f'invalid usage ledger: {error}'}
        if not result['accounting_complete']:
            reasons.append(result.get('reason', 'canonical ledger reconciliation incomplete'))
        for row in result.get('call_usage', []):
            if row['owner'] is None:
                reasons.append(f'{row["attempt"]}: ambiguous owning session in usage ledgers')
        ledgers = {sid: read(path / 'usage.json').get('session', {}) for sid, path in directories.items()}
        attempts = {sid: {row['attempt_id']: row for row in ledger.get('attributions', [])}
                    for sid, ledger in ledgers.items()}
        canonical = attempts.get(session, {})
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
                    'zero_usage_attempts': sorted(zero_usage_attempts),
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
            if attribution['attempt_id'] in zero_usage_attempts:
                rows.append({**identity, 'complete': True, 'imputed_zero_usage': True,
                             'input_tokens': 0, 'cached_input_tokens': 0,
                             'output_tokens': 0, 'reasoning_tokens': 0, 'credits': 0.0})
            else:
                rows.append({**identity, 'complete': False})
            continue
        incoming = usage['prompt_tokens']
        outgoing = usage['completion_tokens']
        cached = usage.get('cached_prompt_tokens', 0)
        if attribution['role'] == 'jev':
            # Jev is billed in native USD ticks, not subscription credits; see compare_parallel jev_usd.
            rows.append({**identity,
                         'input_tokens': incoming, 'cached_input_tokens': cached, 'output_tokens': outgoing,
                         'reasoning_tokens': usage.get('reasoning_tokens', 0),
                         'complete': attribution.get('usage_complete', False), 'credits': 0.0,
                         'cost_usd_ticks': attribution.get('cost_usd_ticks')})
            continue
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
                            for row in rows if row['role'] != 'jev')
    zeroed = {row['attempt'] for row in rows if row.get('imputed_zero_usage')}
    corroborated = zero_usage_attempts & {row['attempt'] for row in rows}
    explained_incomplete = (bool(corroborated) and zeroed == corroborated
                            and not (ledger.get('pendingAttemptIds') or ledger.get('pending_attempt_ids'))
                            and all(r['complete'] and r.get('credits') is not None for r in rows))
    complete = (bool(rows) and totals_match and subscription_only
                and (not ledger.get('usageIsIncomplete', False) or explained_incomplete)
                and len(root_ids) == len(rows) == ledger['modelCalls']
                and child_ids <= root_ids and all(r['complete'] and r.get('credits') is not None for r in rows))
    return {'accounting_complete': complete, 'calls': len(rows), 'call_usage': rows,
            'ledger_totals_match': totals_match, 'subscription_only': subscription_only,
            'unfolded_child_attempts': sorted(child_ids - root_ids),
            'input_tokens': ledger['inputTokens'], 'output_tokens': ledger['outputTokens'],
            'cached_input_tokens': ledger['cachedReadTokens'], 'reasoning_tokens': ledger['reasoningTokens'],
            'credit_estimate': sum(r['credits'] for r in rows) if complete else None}


def codex_accounting(events, stdout, provider='chatgpt'):
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
    if not requests:
        requests = [log for log in logs if log.get('event.name') == 'codex.api_request'
                    ]
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
    subscription_only = provider == 'chatgpt' and all(str(row.get('auth_mode', '')).lower() == 'chatgpt' for row in rows)
    for row in rows:
        row['provider'] = provider
    request_success = all(str(r.get('success')).lower() == 'true' if r.get('event.name') == 'codex.websocket_request'
                          else str(r.get('http.status_code', r.get('http.response.status_code', ''))).startswith('2') for r in requests)
    complete = (bool(rows) and not failures and counts_match and request_success
                and conversations == response_conversations
                and all(r['complete'] and (provider != 'chatgpt' or r.get('credits') is not None) for r in rows))
    return {'accounting_complete': complete, 'calls': len(rows), 'call_usage': rows,
            'subscription_only': subscription_only, 'main_conversation': main_conversation,
            'native_requests': len(requests), 'conversations': len(conversations),
            'request_response_counts_match': counts_match,
            'parent_cli_usage': [row['usage'] for row in cli if row.get('type') == 'turn.completed'],
            'telemetry_events': len(logs),
            **{key: sum(row.get(key, 0) for row in rows)
               for key in ('input_tokens', 'cached_input_tokens', 'output_tokens', 'reasoning_tokens')},
            'credit_estimate': sum(r['credits'] for r in rows) if complete and provider == 'chatgpt' else None,
            'credit_estimate_excluding_zero_output':
                sum(r['credits'] for r in rows if r.get('output_tokens', 0) > 0) if complete and provider == 'chatgpt' else None}


def subscription_account(auth_file):
    auth = json.loads(auth_file.read_text())
    if auth.get('auth_mode') != 'chatgpt' or not auth.get('tokens') or auth.get('OPENAI_API_KEY'):
        raise ValueError('Benchmark requires ChatGPT OAuth, not a model API key')
    tokens = auth['tokens']
    payload = tokens['id_token'].split('.')[1]
    claims = json.loads(base64.urlsafe_b64decode(payload + '=' * (-len(payload) % 4)))
    return tokens.get('account_id') or claims.get('https://api.openai.com/auth', {}).get('chatgpt_account_id')


def run_cell(args):
    distill_auth = args.codex_auth.expanduser()
    codex_auth = Path.home() / '.codex/auth.json'
    provider = getattr(args, 'provider', 'chatgpt')
    main_model = getattr(args, 'main_model', 'gpt-6.1-sol')
    worker_model = getattr(args, 'worker_model', 'gpt-6-luna')
    codex_effort = getattr(args, 'codex_effort', 'high')
    enable_jev = getattr(args, 'enable_jev', False)
    if provider == 'chatgpt':
        account = subscription_account(distill_auth)
        if not account or account != subscription_account(codex_auth):
            raise ValueError('Both agents must use the same signed-in ChatGPT account')
    elif args.variant != 'codex':
        raise ValueError('Use compare_additional deepseek-free-distill for the OpenRouter Distill arm')
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
    environment = {k: v for k, v in os.environ.items() if not k.endswith('_API_KEY')}
    environment['PATH'] = str(args.codex.expanduser().parent) + os.pathsep + environment['PATH']
    environment['GROK_MANAGED_MCPS_ENABLED'] = 'false'
    environment['GROK_MANAGED_MCP_GATEWAY_TOOLS_ENABLED'] = 'false'
    auth_target = home / 'codex-auth.json'
    staged_auth = []
    started = None
    timed_out = False
    process = None
    tree = None
    cleanup_complete = False
    try:
        if args.variant == 'distill':
            main_model = getattr(args, 'main_model', 'gpt-6.1-sol')
            worker_model = getattr(args, 'worker_model', 'gpt-6-luna')
            codex_effort = getattr(args, 'codex_effort', 'high')
            enable_jev = getattr(args, 'enable_jev', False)
            if enable_jev:
                jev_source = args.distill_profile.expanduser() / 'openrouter-auth.json'
                jev_auth = json.loads(jev_source.read_text())
                key = jev_auth.get('key')
                if not isinstance(key, str) or not key:
                    raise ValueError('OpenRouter auth file has no key')
                jev_target = home / 'openrouter-auth.json'
                staged_auth.append(jev_target)
                jev_target.touch(mode=0o600)
                jev_target.write_text(jev_source.read_text())
                environment['OPENROUTER_API_KEY'] = key
            shutil.copyfile(distill_auth, auth_target)
            auth_target.chmod(0o600)
            for name in ('auth.json', 'campaigns_state.json'):
                source = args.distill_profile.expanduser() / name
                if source.is_file():
                    target = home / name
                    staged_auth.append(target)
                    shutil.copyfile(source, target)
                    target.chmod(0o600)
            config = distill_config(main_model=f'chatgpt/{main_model}',
                                    worker_model=f'chatgpt/{worker_model}', enable_jev=enable_jev)
            (home / 'config.toml').write_text(config)
            environment['DISTILL_HOME'] = environment['GROK_HOME'] = str(home)
            command = [str(executable), '--no-leader', '--always-approve', '--disable-web-search',
                       '--cwd', str(worktree), '--session-id', session, '--prompt-file',
                       str(output / 'prompt.txt'), '--output-format', 'streaming-json',
                       '--model', f'chatgpt/{main_model}']
            command.extend(['--tools', LOCAL_TOOLS])
        else:
            main_model = getattr(args, 'main_model', 'gpt-6.1-sol')
            codex_effort = getattr(args, 'codex_effort', 'high')
            endpoint = f'http://127.0.0.1:{collector.server_port}/v1/logs'
            config = f'model="{main_model}"\nmodel_reasoning_effort="{codex_effort}"\nforced_login_method="chatgpt"\n'
            command = [str(executable), 'exec', '--yolo', '--ignore-user-config', '--ephemeral',
                       '--json', '-C', str(worktree), '-m', main_model,
                       '-c', f'model_reasoning_effort="{codex_effort}"', '-c', 'forced_login_method="chatgpt"',
                       '-c', 'web_search="disabled"',
                       '-c', f'otel.exporter={{otlp-http={{endpoint="{endpoint}",protocol="json"}}}}',
                       '-c', 'otel.log_user_prompt=false', '-']
            if provider == 'openrouter':
                environment['OPENROUTER_API_KEY'] = json.loads((args.distill_profile / 'openrouter-auth.json').read_text())['key']
                index = command.index('forced_login_method="chatgpt"')
                del command[index - 1:index + 1]
                definition = '{name="OpenRouter",base_url="https://openrouter.ai/api/v1",wire_api="responses",env_key="OPENROUTER_API_KEY"}'
                command[-1:-1] = ['-c', 'model_provider="openrouter"', '-c', 'model_providers.openrouter=' + definition]
                config = f'model="{main_model}"\nmodel_reasoning_effort="{codex_effort}"\nmodel_provider="openrouter"\nmodel_providers.openrouter={definition}\n'
        (output / 'config.toml').write_text(config)
        (output / 'command.json').write_text(json.dumps(command, indent=2) + '\n')
        started = time.monotonic()
        with (output / 'stdout.jsonl').open('w') as stdout, (output / 'stderr.txt').open('w') as stderr:
            process = subprocess.Popen(command, env=environment, cwd=worktree, stdin=subprocess.PIPE,
                                       stdout=stdout, stderr=stderr, start_new_session=True, text=True)
            tree = OwnedTree(process)
            if args.variant == 'codex':
                process.stdin.write(prompt)
            process.stdin.close()
            while process.poll() is None:
                tree.refresh()
                if time.monotonic() - started >= args.timeout:
                    timed_out = True
                    break
                time.sleep(.02)
            cleanup_complete = tree.stop()
    finally:
        if tree is not None and not cleanup_complete:
            cleanup_complete = tree.stop()
        elif process is not None and process.poll() is None:
            _terminate_owned_group(process)
        elapsed = time.monotonic() - started if started is not None else 0
        collector.shutdown()
        collector.server_close()
        thread.join()
        if args.variant == 'distill':
            auth_target.unlink(missing_ok=True)
            for staged in staged_auth:
                staged.unlink(missing_ok=True)
    if not cleanup_complete:
        raise RuntimeError('Inference process tree did not terminate; grading skipped')
    grade = subprocess.run(['python3', str(ROOT / case['grader']['script_ref']), str(worktree)],
                           text=True, capture_output=True)
    (output / 'grader.txt').write_text(grade.stdout + grade.stderr)
    diff = subprocess.check_output(['git', 'diff', '--no-ext-diff'], cwd=worktree, text=True)
    (output / 'diff.patch').write_text(diff)
    from .compare_additional import comparison_cost, provider_usage
    accounting = (provider_usage(home, session) if args.variant == 'distill' and enable_jev
                  else distill_accounting(home, session) if args.variant == 'distill'
                  else codex_accounting(output / 'telemetry.jsonl', output / 'stdout.jsonl', provider=provider))
    accounting['reference_cost_usd'] = comparison_cost(accounting.get('call_usage', [])) if accounting.get('accounting_complete') else None
    expected = main_model
    if args.variant == 'distill':
        main_calls = [r for r in accounting.get('call_usage', []) if r.get('agent') == 'main' and r.get('role') in ('main', 'main_retry')]
        children = [r for r in accounting.get('call_usage', []) if r.get('agent') == 'worker' and r.get('role') != 'jev']
        model_verified = bool(main_calls) and all(r['model'] == expected for r in main_calls)
        worker_verified = all(r['model'] == worker_model for r in children)
    else:
        main_calls = [r for r in accounting.get('call_usage', []) if r.get('agent') == 'main' and r.get('output_tokens', 0) > 0]
        model_verified = bool(main_calls) and all(r['model'] == expected and r['effort'] == codex_effort for r in main_calls)
        worker_verified = True
    inputs_unchanged = (frozen['fixture_sha256'] == _sha256_tree(ROOT / case['fixture_ref'])
                        and frozen['grader_sha256'] == _sha256_file(ROOT / case['grader']['script_ref']))
    native_payloads = list((executable.parent.parent / 'node_modules/@openai').glob(
        'codex-*/vendor/*/bin/codex')) if executable.suffix == '.js' else [executable]
    if len(native_payloads) != 1:
        raise ValueError('Cannot identify the native benchmark executable')
    summary = {'variant': args.variant, 'case': args.case, 'session': session,
               'provider': provider, 'version': version, 'main_model': main_model, 'worker_model': worker_model if args.variant == 'distill' else None,
               'requested_effort': 'auto' if args.variant == 'distill' else codex_effort, 'jev_enabled': bool(enable_jev and args.variant == 'distill'),
               'same_subscription_account': provider == 'chatgpt', 'model_verified': model_verified, 'worker_verified': worker_verified,
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
               'timed_out': timed_out, 'cleanup_complete': cleanup_complete, 'grader_passed': grade.returncode == 0,
               'cost_basis': 'published_subscription_credit_rates', **accounting}
    summary['passed'] = process.returncode == 0 and grade.returncode == 0 and inputs_unchanged and not timed_out and model_verified and worker_verified
    (output / 'result.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps({k: v for k, v in summary.items() if k != 'call_usage'}), flush=True)
    return 0 if summary['passed'] and accounting['accounting_complete'] and accounting.get('reference_cost_usd') is not None else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--variant', choices=('distill', 'codex'), required=True)
    parser.add_argument('--case', required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--distill', type=Path, default=Path('target/release/distill'))
    parser.add_argument('--codex', type=Path, default=Path(shutil.which('codex')))
    parser.add_argument('--distill-profile', type=Path, default=Path.home() / '.distill')
    parser.add_argument('--codex-auth', type=Path, default=Path.home() / '.codex/auth.json')
    parser.add_argument('--timeout', type=float, default=300)
    parser.add_argument('--main-model', choices=('gpt-6.1-sol', 'gpt-6-astra', 'deepseek/deepseek-v4.1-flash'), default='gpt-6.1-sol')
    parser.add_argument('--codex-effort', choices=('low', 'medium', 'high', 'max'), default='high')
    parser.add_argument('--worker-model', default='gpt-6-luna')
    parser.add_argument('--enable-jev', action='store_true')
    parser.add_argument('--provider', choices=('chatgpt', 'openrouter'), default='chatgpt')
    return run_cell(parser.parse_args())


if __name__ == '__main__':
    raise SystemExit(main())
