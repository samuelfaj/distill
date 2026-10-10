import json
import tempfile
import tomllib
import unittest
from pathlib import Path

from .compare_codex import codex_accounting, distill_accounting, distill_config


class ProfileConfigTest(unittest.TestCase):
    def test_effort_pins_change_only_worker_and_local_utility_defaults(self):
        default = tomllib.loads(distill_config())
        explicit = tomllib.loads(distill_config(worker_effort='medium', utility_effort='medium'))
        self.assertEqual(default['models']['worker_effort'], 'auto')
        self.assertEqual(default['jev']['local']['effort'], 'auto')
        self.assertEqual(explicit['models']['worker'], 'chatgpt/gpt-6-luna')
        self.assertEqual(explicit['jev']['local']['model'], 'chatgpt/gpt-6-luna')
        self.assertEqual(explicit['models']['worker_effort'], 'medium')
        self.assertEqual(explicit['jev']['local']['effort'], 'medium')
        explicit['models']['worker_effort'] = 'auto'
        explicit['jev']['local']['effort'] = 'auto'
        self.assertEqual(explicit, default)


class SubscriptionAccountingTest(unittest.TestCase):
    def test_codex_counts_children_and_deduplicates_exports_without_hiding_missing_usage(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            events, stdout = root / 'events.jsonl', root / 'stdout.jsonl'
            stdout.write_text('{"type":"turn.completed","usage":{"input_tokens":300}}\n')
            logs = []
            for conversation in ('main', 'child'):
                logs.append({'event.name': 'codex.conversation_starts', 'conversation.id': conversation})
            for index, (conversation, incoming, outgoing) in enumerate(
                    [('main', 100, 0), ('main', 200, 10), ('child', 300, 5)]):
                logs.extend([
                    {'event.name': 'codex.websocket_request', 'conversation.id': conversation,
                     'event.timestamp': f'request-{index}', 'success': 'true'},
                    {'event.name': 'codex.sse_event', 'event.kind': 'response.completed',
                     'conversation.id': conversation, 'event.timestamp': f'response-{index}',
                     'model': 'gpt-6.1-sol', 'input_token_count': incoming,
                     'cached_token_count': 0, 'output_token_count': outgoing},
                ])
            logs.append(logs[-1].copy())
            events.write_text(''.join(json.dumps({'fields': log}) + '\n' for log in logs))
            result = codex_accounting(events, stdout)
            self.assertTrue(result['accounting_complete'])
            self.assertEqual((result['calls'], result['input_tokens'], result['output_tokens']), (3, 600, 15))
            self.assertLess(result['credit_estimate_excluding_zero_output'], result['credit_estimate'])

            # The child request still counts when its completed response is absent.
            events.write_text(''.join(json.dumps({'fields': log}) + '\n' for log in logs
                                      if log.get('event.timestamp') != 'response-2'))
            result = codex_accounting(events, stdout)
            self.assertFalse(result['accounting_complete'])
            self.assertEqual(result['calls'], 3)
            self.assertIsNone(result['credit_estimate'])

    def test_distill_folds_child_once_and_rejects_an_unfolded_child(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            def attempt(identity, model, incoming, outgoing):
                return {'attempt_id': identity, 'model_id': model, 'role': 'main',
                        'status': 'completed', 'usage_complete': True,
                        'endpoint': 'https://chatgpt.com/backend-api/codex/responses',
                        'usage': {'prompt_tokens': incoming, 'completion_tokens': outgoing,
                                  'cached_prompt_tokens': 0, 'reasoning_tokens': 0}}
            main = attempt('a', 'gpt-6.1-sol', 100, 10)
            child = attempt('b', 'gpt-6-luna', 200, 20)
            parent = root / 'sessions/main/usage.json'
            child_file = root / 'sessions/child/usage.json'
            for path in (parent, child_file):
                path.parent.mkdir(parents=True)
            totals = {'inputTokens': 300, 'outputTokens': 30, 'cachedReadTokens': 0,
                      'reasoningTokens': 0, 'modelCalls': 2, 'attributions': [main, child]}
            parent.write_text(json.dumps({'session': totals}))
            child_file.write_text(json.dumps({'session': {'attributions': [child]}}))
            result = distill_accounting(root, 'main')
            self.assertTrue(result['accounting_complete'])
            self.assertEqual((result['calls'], result['input_tokens']), (2, 300))

            totals.update(inputTokens=100, outputTokens=10, modelCalls=1, attributions=[main])
            parent.write_text(json.dumps({'session': totals}))
            result = distill_accounting(root, 'main')
            self.assertFalse(result['accounting_complete'])
            self.assertEqual(result['unfolded_child_attempts'], ['b'])
            self.assertIsNone(result['credit_estimate'])


class StrictDistillAccountingTest(unittest.TestCase):
    def fixture(self, root):
        """Persist the observed runtime schemas, including folded child usage."""
        def row(identity, role='main'):
            return {'attempt_id': identity, 'request_id': f'resp_{identity}',
                    'model_id': 'gpt-6.1-sol', 'role': role, 'status': 'completed',
                    'usage_complete': True, 'endpoint': 'https://chatgpt.com/backend-api/codex/responses',
                    'usage': {'prompt_tokens': 10, 'completion_tokens': 1,
                              'cached_prompt_tokens': 0, 'reasoning_tokens': 0}}

        rows = [row('sampler:a'), row('sampler:b'), row('sampler:c'), row('initial-title:t', 'auxiliary')]
        main, child = root / 'sessions/cwd/main', root / 'sessions/cwd/child'
        logs = []
        for path, calls in ((main, 2), (child, 1)):
            path.mkdir(parents=True)
            events = [{'type': 'turn_started', 'session_id': path.name, 'yolo_mode': True},
                      {'type': 'turn_ended', 'outcome': 'completed'}]
            (path / 'events.jsonl').write_text(''.join(json.dumps(r) + '\n' for r in events))
            (path / 'updates.jsonl').write_text('')
            # One model call may yield several messages. Never count these as requests.
            (path / 'chat_history.jsonl').write_text('{"type":"assistant","content":"text"}\n' * 4)
            for _ in range(calls):
                logs.extend([{'sid': path.name, 'msg': 'shell.turn.inference_start', 'ctx': {'loop_index': 1}},
                             {'sid': path.name, 'msg': 'shell.turn.inference_done',
                              'ctx': {'attempts': 1, **rows[0]['usage']}}])
        meta = {'parent_session_id': 'main', 'child_session_id': 'child', 'subagent_id': 'child',
                'attempt_id': 'at1.child', 'status': 'completed', 'completed_at': '2026-10-09T22:00:00Z'}
        meta_path = main / 'subagents/child/meta.json'
        meta_path.parent.mkdir(parents=True)
        meta_path.write_text(json.dumps(meta))
        (main / 'updates.jsonl').write_text(''.join(json.dumps({
            'method': '_x.ai/session/update', 'params': {'sessionId': 'main',
                'update': {'sessionUpdate': tag, **meta}}}) + '\n'
            for tag in ('subagent_spawned', 'subagent_finished')))
        (root / 'logs').mkdir()
        (root / 'logs/unified.jsonl').write_text(''.join(json.dumps(r) + '\n' for r in logs))
        self.write_ledger(main, rows)
        self.write_ledger(child, rows[2:3])
        return main, child, rows

    def write_ledger(self, path, rows):
        (path / 'usage.json').write_text(json.dumps({'session': {
            'attributions': rows, 'modelCalls': len(rows),
            'inputTokens': len(rows) * 10, 'outputTokens': len(rows),
            'cachedReadTokens': 0, 'reasoningTokens': 0}}))

    def test_available_evidence_reconciles_without_claiming_auxiliary_coverage(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            main, child, rows = self.fixture(root)
            legacy = distill_accounting(root, 'main')
            result = distill_accounting(root, 'main', strict=True)
            evidence = result['accounting_evidence']
            self.assertTrue(legacy['accounting_complete'])
            self.assertEqual(result['call_usage'], legacy['call_usage'])
            self.assertTrue(evidence['available_evidence_matches'], evidence)
            self.assertTrue(evidence['complete'] and result['accounting_complete'])
            self.assertEqual(result['credit_estimate'], legacy['credit_estimate'])
            self.assertEqual(evidence['reasons'], [])
            self.assertIn('auxiliary', evidence['coverage_gaps'][0])
            self.assertEqual(evidence['ledger_only_attempt_ids'], ['initial-title:t'])
            sessions = {r['session_id']: r for r in evidence['sessions']}
            self.assertEqual(sessions['main']['local_attempt_ids'], ['initial-title:t', 'sampler:a', 'sampler:b'])
            self.assertEqual(sessions['child']['inference_completions'], 1)
            self.assertTrue(evidence['artifacts'][str(main / 'subagents/child/meta.json')])
            # The disclosure never waives incomplete recorded auxiliary usage.
            rows[-1]['usage_complete'] = False
            self.write_ledger(main, rows)
            incomplete = distill_accounting(root, 'main', strict=True)
            self.assertFalse(incomplete['accounting_evidence']['complete'])
            self.assertFalse(incomplete['accounting_complete'])
            self.assertIsNone(incomplete['credit_estimate'])

    def test_balanced_ledger_cannot_hide_omitted_request_retry_or_child(self):
        for omission in ('request', 'retry', 'child'):
            with self.subTest(omission=omission), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                main, child, rows = self.fixture(root)
                if omission == 'request':
                    self.write_ledger(main, rows[1:])  # Totals and attribution count still agree.
                    expected = 'foreground submissions/completions'
                elif omission == 'retry':
                    with (root / 'logs/unified.jsonl').open('a') as stream:
                        stream.write(json.dumps({'sid': 'main', 'msg': 'shell.turn.inference_retry',
                            'ctx': {'sampler_request_id': 'b', 'kind': 'api', 'attempt': 1}}) + '\n')
                    expected = 'retry evidence'
                else:
                    (child / 'usage.json').unlink()
                    self.write_ledger(main, rows[:2] + rows[3:])
                    expected = 'usage.json'
                self.assertTrue(distill_accounting(root, 'main')['accounting_complete'])
                result = distill_accounting(root, 'main', strict=True)
                evidence = result['accounting_evidence']
                self.assertFalse(result['accounting_complete'] or evidence['available_evidence_matches'])
                self.assertTrue(any(expected in reason for reason in evidence['reasons']), evidence)
                self.assertIsNone(result['credit_estimate'])
                if omission == 'retry':
                    main_evidence = next(r for r in evidence['sessions'] if r['session_id'] == 'main')
                    self.assertEqual(main_evidence['retry_attempt_ids'], ['sampler:b:retry:api:1'])

    def test_missing_request_evidence_is_unknown_even_with_complete_ledger(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            log = root / 'logs/unified.jsonl'
            log.unlink()
            result = distill_accounting(root, 'main', strict=True)
            self.assertFalse(result['accounting_complete'])
            evidence = result['accounting_evidence']
            self.assertFalse(evidence['artifacts'][str(log)])
            self.assertTrue(any(str(log) in reason for reason in evidence['reasons']))

    def test_requested_effort_does_not_replace_missing_or_distinct_applied_effort(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            main, _, rows = self.fixture(root)
            rows[-1].update(role='utility', requested_effort='medium', applied_effort=None,
                            task_id='select_units', source_kind='tool_families')
            rows[0].update(requested_effort='high', applied_effort='effort:medium')
            self.write_ledger(main, rows)
            result = distill_accounting(root, 'main', strict=True)
            self.assertTrue(result['accounting_complete'])
            calls = {row['attempt']: row for row in result['call_usage']}
            self.assertEqual(calls['initial-title:t']['requested_effort'], 'medium')
            self.assertIsNone(calls['initial-title:t']['effort'])
            self.assertEqual(calls['sampler:a']['requested_effort'], 'high')
            self.assertEqual(calls['sampler:a']['effort'], 'effort:medium')
            self.assertEqual(calls['initial-title:t']['task_id'], 'select_units')
            self.assertEqual(calls['initial-title:t']['source_kind'], 'tool_families')

    def test_call_owner_uses_deepest_ledger_and_durable_child_type_without_model_relabeling(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            main, child, rows = self.fixture(root)
            meta_path = main / 'subagents/child/meta.json'
            meta = json.loads(meta_path.read_text())
            meta.update(subagent_type='code-reviewer', effective_model_id='chatgpt/gpt-6.1-sol')
            meta_path.write_text(json.dumps(meta))
            grandchild = root / 'sessions/cwd/grandchild'
            grandchild.mkdir()
            nested = {**rows[2], 'attempt_id': 'sampler:nested', 'request_id': 'resp_nested', 'model_id': 'gpt-6-luna'}
            self.write_ledger(grandchild, [nested])
            self.write_ledger(child, [rows[2], nested])
            self.write_ledger(main, rows + [nested])
            nested_meta = child / 'subagents/grandchild/meta.json'
            nested_meta.parent.mkdir(parents=True)
            nested_meta.write_text(json.dumps({'child_session_id': 'grandchild', 'parent_session_id': 'child',
                'subagent_type': 'explore', 'effective_model_id': 'chatgpt/gpt-6-luna'}))
            result = distill_accounting(root, 'main')
            self.assertTrue(result['accounting_complete'])
            calls = {row['attempt']: row for row in result['call_usage']}
            self.assertEqual(calls['sampler:a']['owner']['session_id'], 'main')
            self.assertIsNone(calls['sampler:a']['owner']['subagent_type'])
            self.assertEqual(calls['sampler:c']['model'], 'gpt-6.1-sol')
            self.assertEqual(calls['sampler:c']['owner'], {
                'session_id': 'child', 'parent_session_id': 'main', 'subagent_type': 'code-reviewer',
                'subagent_id': 'child', 'spawn_attempt_id': 'at1.child',
                'effective_model_id': 'chatgpt/gpt-6.1-sol', 'usage_path': str(child / 'usage.json'),
                'effective_context_source': None, 'resumed_from': None, 'effort_auto': None,
                'model_routing_locked': None, 'spawn_request': None,
                'metadata_path': str(meta_path)})
            self.assertEqual(calls['sampler:nested']['owner']['session_id'], 'grandchild')
            self.assertEqual(calls['sampler:nested']['owner']['subagent_type'], 'explore')
            self.assertEqual(calls['sampler:nested']['model'], 'gpt-6-luna')
            resumed = root / 'sessions/cwd/resumed'
            resumed.mkdir()
            resumed_call = {**nested, 'attempt_id': 'sampler:resumed', 'request_id': 'resp_resumed'}
            self.write_ledger(resumed, [rows[2], nested, resumed_call])
            self.write_ledger(main, rows + [nested, resumed_call])
            resume_meta = main / 'subagents/resumed/meta.json'
            resume_meta.parent.mkdir(parents=True)
            resume_meta.write_text(json.dumps({'child_session_id': 'resumed', 'parent_session_id': 'main',
                'subagent_type': 'general-purpose', 'resumed_from': 'child'}))
            calls = {row['attempt']: row for row in distill_accounting(root, 'main')['call_usage']}
            self.assertEqual(calls['sampler:c']['owner']['subagent_type'], 'code-reviewer')
            self.assertEqual(calls['sampler:nested']['owner']['session_id'], 'grandchild')
            self.assertEqual(calls['sampler:resumed']['owner']['session_id'], 'resumed')
            nested_meta.unlink()
            ambiguous = distill_accounting(root, 'main')['call_usage']
            self.assertIsNone(next(row['owner'] for row in ambiguous if row['attempt'] == 'sampler:nested'))

    def test_native_task_request_requires_exact_result_link_and_preserves_model_omission(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            main, _, _ = self.fixture(root)
            meta_path = main / 'subagents/child/meta.json'
            meta = json.loads(meta_path.read_text())
            meta.update(subagent_type='code-reviewer', effective_model_id='chatgpt/gpt-6.1-sol',
                        effective_context_source='new', effort_auto=False, model_routing_locked=False)
            meta_path.write_text(json.dumps(meta))
            tool = {'namespace': 'distill', 'kind': 'task', 'name': 'spawn_subagent',
                    'version': 1, 'label': 'Subagent', 'read_only': False}
            request = {'sessionUpdate': 'tool_call', 'toolCallId': 'call-review',
                       '_meta': {'x.ai/tool': tool}, 'rawInput': {
                           'subagent_type': 'code-reviewer', 'background': False,
                           'prompt': 'Review the change\u2028policy'}}
            result = {'sessionUpdate': 'tool_call_update', 'toolCallId': 'call-review', 'status': 'completed',
                      'rawOutput': {'type': 'SubagentCompleted', 'subagent_id': 'child'}}
            updates = main / 'updates.jsonl'
            original = updates.read_text()
            def write_updates(extra=()):
                updates.write_text(original + ''.join(json.dumps({'method': 'session/update',
                    'params': {'sessionId': 'main', 'update': update}}, ensure_ascii=False) + '\n'
                    for update in (request, result, *extra)))
            write_updates()
            self.assertIn('\u2028'.encode(), updates.read_bytes())
            strict = distill_accounting(root, 'main', strict=True)
            self.assertTrue(strict['accounting_complete'], strict['accounting_evidence'])
            call = next(row for row in distill_accounting(root, 'main')['call_usage'] if row['attempt'] == 'sampler:c')
            owner = call['owner']
            self.assertEqual(call['model'], 'gpt-6.1-sol')
            self.assertEqual(call['request_id'], 'resp_sampler:c')
            self.assertEqual(owner['effective_context_source'], 'new')
            self.assertIs(owner['effort_auto'], False)
            self.assertIs(owner['model_routing_locked'], False)
            self.assertEqual(owner['spawn_request'], {
                'origin': 'tool', 'tool_call_id': 'call-review', 'tool': tool,
                'input': {'subagent_type': 'code-reviewer', 'background': False},
                'artifact': str(updates), 'request_line': 3, 'result_line': 4})
            foreground = result['rawOutput']
            auto_header = ('Subagent took longer than the foreground budget and was moved to the '
                           'background to keep the conversation responsive. It is still running')
            for header, background in (
                ('Subagent started in background.', True),
                (auto_header + '.', False),
                (auto_header + ' — you will be notified when it completes.', False),
            ):
                request['rawInput']['background'] = background
                result['rawOutput'] = {'type': 'Text', 'text':
                    header + '\nsubagent_id: child\ntype: code-reviewer\ndescription: Review the change\n'}
                write_updates()
                call = next(r for r in distill_accounting(root, 'main')['call_usage'] if r['attempt'] == 'sampler:c')
                with self.subTest(header=header):
                    self.assertEqual(call['owner']['spawn_request']['tool_call_id'], 'call-review')
                    self.assertIs(call['owner']['spawn_request']['input']['background'], background)
            # Neither arbitrary text nor a separate wait call proves the spawn.
            valid_notice = result['rawOutput']['text']
            for call_id, notice in (('call-review', 'Reported text:\n' + valid_notice),
                                    ('wait-call', valid_notice)):
                result.update(toolCallId=call_id, rawOutput={'type': 'Text', 'text': notice})
                write_updates()
                call = next(r for r in distill_accounting(root, 'main')['call_usage'] if r['attempt'] == 'sampler:c')
                with self.subTest(call_id=call_id, notice=notice):
                    self.assertIsNone(call['owner']['spawn_request'])
            result.update(toolCallId='call-review', rawOutput=foreground)
            # A second result without a matching request makes the child link ambiguous.
            write_updates(({**result, 'toolCallId': 'unrelated-call'},))
            call = next(row for row in distill_accounting(root, 'main')['call_usage'] if row['attempt'] == 'sampler:c')
            self.assertIsNone(call['owner']['spawn_request'])


if __name__ == '__main__':
    unittest.main()
