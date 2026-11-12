import io
import json
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest.mock import patch

from .compare_additional import comparison_cost, config, native_usage, provider_usage


class AdditionalBenchmarkTests(unittest.TestCase):
    def test_deepseek_config_parses_quoted_model_id_and_ablations(self):
        import tomllib

        text = config('deepseek-v4.1-flash', ablation=True)
        parsed = tomllib.loads(text)
        self.assertEqual(parsed['models']['default'], 'deepseek-v4.1-flash')
        self.assertEqual(parsed['models']['worker'], 'deepseek-v4.1-flash')
        self.assertFalse(parsed['jev']['enabled'])
        self.assertFalse(parsed['jev']['effort_auto'])
        self.assertEqual(parsed['models']['worker_effort'], 'low')
        self.assertEqual(parsed['model']['deepseek-v4.1-flash']['model'], '~deepseek/deepseek-flash-latest')

    def test_normal_config_enables_jev_and_auto_effort(self):
        import tomllib

        parsed = tomllib.loads(config('claude/claude-sonnet-5-5'))
        self.assertTrue(parsed['jev']['enabled'])
        self.assertTrue(parsed['jev']['effort_auto'])
        self.assertEqual(parsed['models']['worker_effort'], 'auto')

    def test_native_usage_reads_camel_case_cache_counts_and_cost(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'native.json'
            path.write_text(json.dumps({'modelUsage': {'claude-sonnet-5-5': {
                'inputTokens': 10, 'outputTokens': 2, 'cacheReadInputTokens': 1,
                'cacheCreationInputTokens': 3}}, 'total_cost_usd': 0.25, 'is_error': False}))
            result = native_usage(path)
        self.assertTrue(result['accounting_complete'])
        self.assertEqual(result['call_usage'][0]['cacheReadInputTokens'], 1)
        self.assertEqual(result['call_usage'][0]['cacheCreationInputTokens'], 3)
        self.assertEqual(result['cost_usd'], 0.25)
        self.assertFalse(result['native_is_error'])

    def test_native_usage_missing_unknown_cost_null_and_invalid_input(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'native.json'
            path.write_text(json.dumps({'modelUsage': {'m': {
                'inputTokens': 1, 'outputTokens': 0, 'cacheReadInputTokens': 0,
                'cacheCreationInputTokens': 0}}}))
            result = native_usage(path)
            self.assertTrue(result['accounting_complete'])
            self.assertIsNone(result['cost_usd'])
            path.write_text('{}')
            self.assertFalse(native_usage(path)['accounting_complete'])
            path.write_text('{')
            self.assertFalse(native_usage(path)['accounting_complete'])
            self.assertFalse(native_usage(Path(directory) / 'missing.json')['accounting_complete'])

    def test_native_usage_rejects_incomplete_cache_counts_and_negative_cost(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'native.json'
            path.write_text(json.dumps({'modelUsage': {'m': {
                'inputTokens': 1, 'outputTokens': 0, 'cacheReadInputTokens': 0}}}))
            self.assertFalse(native_usage(path)['accounting_complete'])
            path.write_text(json.dumps({'modelUsage': {'m': {
                'inputTokens': 1, 'outputTokens': 0, 'cacheReadInputTokens': 0,
                'cacheCreationInputTokens': 0}}, 'total_cost_usd': -1}))
            self.assertIsNone(native_usage(path)['cost_usd'])

    def test_provider_usage_requires_complete_unique_matching_successful_owned_rows(self):
        result = {'call_usage': [{'attempt': 'a', 'complete': True, 'status': 'completed', 'owner': 's', 'model': 'deepseek/deepseek-v4.1-flash'}],
                  'ledger_totals_match': True, 'unfolded_child_attempts': []}
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            usage = home / 'sessions' / 's' / 'usage.json'
            usage.parent.mkdir(parents=True)
            usage.write_text(json.dumps({'session': {'usageIsIncomplete': False, 'modelCalls': 1}}))
            with patch('tools.task_cost_eval.compare_additional.distill_accounting', return_value=result.copy()):
                self.assertTrue(provider_usage(home, 's')['accounting_complete'])
            bad_rows = [
                [],
                [dict(result['call_usage'][0], attempt='missing', complete=False)],
                [result['call_usage'][0], dict(result['call_usage'][0])],
                [dict(result['call_usage'][0], status='failed')],
                [dict(result['call_usage'][0], owner=None)],
                [dict(result['call_usage'][0], model='unknown')],
            ]
            for rows in bad_rows:
                with self.subTest(rows=rows):
                    broken = dict(result, call_usage=rows)
                    with patch('tools.task_cost_eval.compare_additional.distill_accounting', return_value=broken):
                        self.assertFalse(provider_usage(home, 's')['accounting_complete'])
            incomplete = dict(result)
            incomplete['ledger_totals_match'] = False
            with patch('tools.task_cost_eval.compare_additional.distill_accounting', return_value=incomplete):
                self.assertFalse(provider_usage(home, 's')['accounting_complete'])
            usage.write_text(json.dumps({'session': {'usageIsIncomplete': True, 'modelCalls': 1}}))
            with patch('tools.task_cost_eval.compare_additional.distill_accounting', return_value=result.copy()):
                self.assertFalse(provider_usage(home, 's')['accounting_complete'])


    def test_rejected_answer_keeps_its_reported_bill_and_missing_cost_stays_unknown(self):
        row = {'attempt': 'a', 'complete': True, 'status': 'rejected',
               'owner': 's', 'model': 'deepseek/deepseek-v4.1-flash'}
        accounted = {'call_usage': [row], 'ledger_totals_match': True, 'unfolded_child_attempts': []}
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            usage = home / 'sessions/s/usage.json'
            usage.parent.mkdir(parents=True)
            for cost, expected in [(12000000, .0012), (None, None)]:
                usage.write_text(json.dumps({'session': {'modelCalls': 1, 'attributions': [
                    {'attempt_id': 'a', 'cost_usd_ticks': cost, 'cost_basis': 'reported'}]}}))
                with patch('tools.task_cost_eval.compare_additional.distill_accounting', return_value=accounted):
                    result = provider_usage(home, 's')
                self.assertTrue(result['accounting_complete'])
                self.assertEqual(result['cost_usd'], expected)

    def test_claude_prices_uncached_input_once_and_splits_cache_write_durations(self):
        row = {'attempt': 'a', 'complete': True, 'status': 'completed',
               'owner': 's', 'model': 'claude-sonnet-5-5'}
        accounted = {'call_usage': [row], 'ledger_totals_match': True, 'unfolded_child_attempts': []}
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            usage = home / 'sessions/s/usage.json'
            usage.parent.mkdir(parents=True)
            usage.write_text(json.dumps({'session': {'modelCalls': 1, 'attributions': [
                {'attempt_id': 'a', 'usage': {'prompt_tokens': 100, 'cached_prompt_tokens': 20,
                 'cache_creation_prompt_tokens': 30, 'cache_creation_1h_prompt_tokens': 10,
                 'completion_tokens': 10}}]}}))
            with patch('tools.task_cost_eval.compare_additional.distill_accounting', return_value=accounted):
                result = provider_usage(home, 's')
            self.assertTrue(result['accounting_complete'])
            self.assertTrue(result['accounting_complete'])
            self.assertIsNone(result['cost_usd'])
            self.assertAlmostEqual(result['reference_cost_usd'], (50 * 2 + 20 * .1 + 20 * 2.5 + 10 * 4 + 10 * 10) / 1e6)

    def test_reference_cost_mixed_gpt_jev_and_free_unknown(self):
        rows = [
            {'model': 'gpt-6-luna', 'usage': {'prompt_tokens': 100, 'cached_prompt_tokens': 20, 'completion_tokens': 10}},
            {'model': 'jev/openrouter/utility', 'cost_usd_ticks': 5000000},
            {'model': 'worker-model:free', 'cost_usd_ticks': 0},
        ]
        from .compare_codex import credits
        expected = credits('gpt-6-luna', 100, 20, 10) * .04 + .0005
        self.assertAlmostEqual(comparison_cost(rows), expected)
        self.assertIsNone(comparison_cost([{'model': 'worker-model:free', 'usage': {'prompt_tokens': 1, 'cached_prompt_tokens': 0, 'completion_tokens': 1}}]))
        self.assertIsNone(comparison_cost([{'model': 'jev/unknown', 'usage': {'prompt_tokens': 1, 'cached_prompt_tokens': 0, 'completion_tokens': 1}}]))

    def test_reference_cost_opus_and_haiku_threshold_and_cache_writes(self):
        opus = {'model': 'claude-opus-5-5', 'usage': {'prompt_tokens': 100, 'cached_prompt_tokens': 20,
                'cache_creation_prompt_tokens': 30, 'cache_creation_1h_prompt_tokens': 10, 'completion_tokens': 10}}
        self.assertAlmostEqual(comparison_cost([opus]), (50 * 4 + 20 * .2 + 20 * 5 + 10 * 8 + 10 * 20) / 1e6)
        low = {'model': 'claude-haiku-5-5', 'usage': {'prompt_tokens': 100000, 'cached_prompt_tokens': 0, 'completion_tokens': 0}}
        high = {'model': 'claude-haiku-5-5', 'usage': {'prompt_tokens': 100001, 'cached_prompt_tokens': 0, 'completion_tokens': 0}}
        self.assertAlmostEqual(comparison_cost([low]), .01)
        self.assertAlmostEqual(comparison_cost([high]), .0500005)
        self.assertIsNone(comparison_cost([{'model': 'claude-opus-5-5', 'usage': {'prompt_tokens': 5}}]))


    def test_requested_auto_configs_preserve_workers_and_full_deepseek_effort_choices(self):
        import tomllib
        from .compare_additional import OPUS, HAIKU, DEEPSEEK, FREE_WORKER
        opus = tomllib.loads(config(OPUS, worker=HAIKU, enable_jev=True))
        self.assertEqual(opus['models']['worker'], HAIKU)
        self.assertEqual(opus['models']['worker_effort'], 'auto')
        self.assertEqual(opus['models']['session_summary'], HAIKU)
        self.assertEqual(opus['jev']['local']['model'], HAIKU)
        self.assertEqual(opus['jev']['api_key_env'], 'OPENROUTER_API_KEY')
        deepseek = tomllib.loads(config(DEEPSEEK, worker=FREE_WORKER, enable_jev=True))
        self.assertEqual(deepseek['model'][FREE_WORKER]['model'], 'openrouter/free')
        self.assertEqual(deepseek['model'][FREE_WORKER]['context_window'], 200000)
        self.assertEqual({r['value'] for r in deepseek['model'][DEEPSEEK]['reasoning_efforts']}, {'none', 'low', 'high', 'max'})


    def test_native_list_cost_preserves_unexposed_cache_write_lifetime(self):
        from .compare_additional import comparison_cost
        row = {'model': 'claude-opus-5-5', 'costBasis': 'list', 'costUSD': .1570766,
               'usage': {'prompt_tokens': 62690, 'cached_prompt_tokens': 50743,
                         'cache_creation_prompt_tokens': 11935, 'completion_tokens': 2570}}
        self.assertAlmostEqual(comparison_cost([row]), .1570766)
        del row['costUSD']
        self.assertAlmostEqual(comparison_cost([row]), .1212716)


# Stands in for the Distill binary: no model call; it only acknowledges a mode
# flag the way headless does after the server confirms the mode.
FAKE_DISTILL = """#!{python}
import sys
args = sys.argv[1:]
if args == ['--version']:
    print('distill 0.0.0 (HEAD)')
    raise SystemExit(0)
session = args[args.index('--session-id') + 1]
labels = {{'normal': 'Normal', 'auto': 'Auto', 'ultrafast': 'UltraFast',
          'ultracode': 'UltraCode', 'ultrasave': 'UltraSave'}}
mode = args[args.index('--mode') + 1] if '--mode' in args else next(
    (a[2:] for a in args if a[2:] in labels), None)
if mode and {silent} is False:
    print(f'{{labels[mode]}} enabled for session {{session}}', file=sys.stderr)
"""


def command_shape(command):
    """A command without its per-cell paths and session ID."""
    session = command[command.index('--session-id') + 1]
    return [arg for arg in command if not arg.startswith('/') and arg != session]


def fake_distill(root, silent=False):
    import sys
    path = root / 'fake-distill'
    path.write_text(FAKE_DISTILL.format(python=sys.executable, silent=silent))
    path.chmod(0o755)
    return path


class ModeArmTests(unittest.TestCase):
    def run_cell(self, root, variant, mode, silent=False):
        from types import SimpleNamespace
        from .compare_additional import run
        profile = root / 'profile'
        profile.mkdir(exist_ok=True)
        output = root / f'{variant}-{mode}'
        args = SimpleNamespace(variant=variant, case='tax-bug-en', output=output, enable_jev=False,
                               distill=fake_distill(root, silent), claude=root / 'missing-claude',
                               distill_profile=profile, claude_auth=profile / 'claude-auth.json',
                               timeout=30, mode=mode)
        with redirect_stdout(io.StringIO()):
            run(args)
        return json.loads((output / 'command.json').read_text()), json.loads((output / 'result.json').read_text())

    def test_mode_reaches_the_distill_command_and_the_run_evidence(self):
        # A mode arm differs from the unpinned run only by the flag; the
        # evidence must name the mode so a summary never mixes arms.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            plain, plain_result = self.run_cell(root, 'deepseek-free-distill', None)
            for mode, flags in (('ultrasave', ['--ultrasave']), ('normal', ['--mode', 'normal']),
                                ('auto', ['--mode', 'auto'])):
                with self.subTest(mode=mode):
                    command, result = self.run_cell(root, 'deepseek-free-distill', mode)
                    self.assertEqual(command_shape(command), command_shape(plain) + flags)
                    self.assertEqual(result['execution_mode'], mode)
                    self.assertIs(result['execution_mode_activation_confirmed'], True)
            self.assertFalse({'--mode', '--ultrasave', '--ultrafast', '--ultracode'} & set(plain))
            self.assertIsNone(plain_result['execution_mode'])
            self.assertIsNone(plain_result['execution_mode_activation_confirmed'])

    def test_unconfirmed_mode_is_recorded_as_unconfirmed(self):
        # A run that did not confirm its mode may have measured another mode.
        with tempfile.TemporaryDirectory() as directory:
            _, result = self.run_cell(Path(directory), 'opus-distill', 'ultrafast', silent=True)
            self.assertIs(result['execution_mode_activation_confirmed'], False)

    def test_native_variants_reject_mode(self):
        from types import SimpleNamespace
        from .compare_additional import main, run
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / 'cell'
            for variant in ('opus-native', 'claude-native'):
                with self.subTest(variant=variant):
                    with self.assertRaisesRegex(ValueError, 'only to Distill'):
                        run(SimpleNamespace(variant=variant, mode='normal', output=output))
                    with patch('tools.task_cost_eval.compare_additional.shutil.which', return_value='/bin/false'), \
                         patch('sys.stderr'), self.assertRaises(SystemExit):
                        main(['--variant', variant, '--case', 'tax-bug-en', '--output', str(output), '--mode', 'auto'])
                    self.assertFalse(output.exists())


if __name__ == '__main__':
    unittest.main()
