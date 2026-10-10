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


if __name__ == '__main__':
    unittest.main()
