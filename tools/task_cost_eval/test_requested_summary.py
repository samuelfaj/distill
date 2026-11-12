import io
import json
import math
import tempfile
import unittest
from contextlib import redirect_stderr
from pathlib import Path

from .requested_summary import PRICING, build, check_pricing, main

RESULTS = Path(__file__).parent / 'results'
LEGACY = [RESULTS / f'requested-20261016-{family}.json' for family in ('astra', 'sol', 'opus', 'deepseek')]
DOLLAR = 10**10  # Jev receipts are USD ticks


def run(arm, mode, *, cost=None, ticks=(), wall=10.0, passed=True, activated=True):
    """One synthetic cell; each receipt is a Jev call priced only by its reported ticks."""
    calls = [{'model': 'jev/test', 'role': 'jev', 'agent': 'main', 'cost_usd_ticks': t} for t in ticks]
    result = {'variant': 'codex' if arm == 'native' else 'distill', 'version': f'{arm} 1.0 (abc123)',
              'grader_passed': passed, 'exit_code': 0, 'timed_out': False, 'frozen_inputs_unchanged': True,
              'model_verified': True, 'worker_verified': True, 'accounting_complete': cost is not None,
              'reference_cost_usd': cost, 'wall_time_s': wall, 'call_usage': calls}
    if arm != 'native':
        result.update(execution_mode=mode, execution_mode_activation_confirmed=activated)
    return {'arm': arm, 'mode': mode, 'case': 'tax-bug-en', 'repetition': 1, 'result': result}


def family(path, runs, name='astra'):
    path.write_text(json.dumps({'family': name, 'date': '2099-01-01', 'cases': ['tax-bug-en'], 'repetitions': 3,
                                'timeout_s': 300, 'status': 'complete', 'outcome_based_reruns': False, 'runs': runs}))
    return path


def synthetic():
    runs = [run('native', None, cost=1.0, ticks=[DOLLAR]) for _ in range(3)]
    runs += [run('distill', 'normal', cost=.5, ticks=[DOLLAR // 2], wall=6) for _ in range(3)]
    runs += [run('distill', 'ultrasave', cost=.25, ticks=[DOLLAR // 4], wall=9) for _ in range(3)]
    runs += [run('distill', 'ultrafast', cost=.5, ticks=[DOLLAR // 2], wall=3, passed=i == 0) for i in range(3)]
    # One Auto cell lost a receipt: its cost is unknown, the arm is a lower bound.
    runs += [run('distill', 'auto', cost=None, ticks=[None, DOLLAR // 5]),
             run('distill', 'auto', cost=.3, ticks=[3 * DOLLAR // 10]),
             run('distill', 'auto', cost=.3, ticks=[3 * DOLLAR // 10])]
    return runs


class SummaryTest(unittest.TestCase):
    def test_committed_family_files_rebuild_the_committed_summary(self):
        # The builder and the published 2026-10-16 numbers follow one cost and quality rule.
        built = build(LEGACY)
        committed = json.loads((RESULTS / 'requested-20261016-summary.json').read_text())
        self.assertEqual((built['runs'], built['modes']), (committed['runs'], ['normal']))
        for name, pair in committed['pairs'].items():
            for key, value in pair.items():
                for field, expected in (value.items() if isinstance(value, dict) else [(None, value)]):
                    actual = built['pairs'][name][key] if field is None else built['pairs'][name][key][field]
                    with self.subTest(family=name, key=key, field=field):
                        if isinstance(expected, float):
                            self.assertTrue(math.isclose(actual, expected, rel_tol=1e-12, abs_tol=1e-12))
                        else:
                            self.assertEqual(actual, expected)

    def test_hand_computed_family_deltas_against_native_and_normal(self):
        with tempfile.TemporaryDirectory() as directory:
            summary = build([family(Path(directory) / 'f.json', synthetic())])
        pair = summary['pairs']['astra']
        arms = pair['arms']
        self.assertEqual(summary['modes'], ['normal', 'auto', 'ultrasave', 'ultrafast'])
        self.assertEqual((pair['native']['reference_cost_usd'], pair['native']['wall_time_s']), (3.0, 30.0))
        self.assertEqual(pair['distill'], {k: v for k, v in arms['normal'].items()
                                           if k not in ('measured', 'vs_native', 'vs_normal', 'quality_gate')})
        normal, save = arms['normal'], arms['ultrasave']
        self.assertAlmostEqual(normal['vs_native']['cost_delta_percent'], -50.0)   # 1.5 / 3.0
        self.assertAlmostEqual(normal['vs_native']['time_delta_percent'], -40.0)   # 18 / 30
        self.assertIsNone(normal['vs_normal'])
        self.assertAlmostEqual(save['vs_native']['cost_delta_percent'], -75.0)     # 0.75 / 3.0
        self.assertAlmostEqual(save['vs_normal']['cost_delta_percent'], -50.0)     # 0.75 / 1.5
        self.assertAlmostEqual(save['vs_normal']['time_delta_percent'], 50.0)      # 27 / 18
        self.assertEqual(save['vs_normal']['quality_delta_percent'], 0.0)
        self.assertEqual((save['measured'], arms['auto']['measured']), ('mode pinned', 'under Auto'))
        self.assertEqual(save['versions'], ['distill 1.0 (abc123)'])

    def test_missing_receipt_makes_a_lower_bound_never_a_zero(self):
        with tempfile.TemporaryDirectory() as directory:
            auto = build([family(Path(directory) / 'f.json', synthetic())])['pairs']['astra']['arms']['auto']
        self.assertIsNone(auto['reference_cost_usd'])
        self.assertAlmostEqual(auto['known_cost_usd'], .6)
        self.assertAlmostEqual(auto['reference_cost_lower_bound_usd'], .8)  # every priceable call
        self.assertEqual((auto['missing_cost_cells'], auto['unpriced_calls'], auto['priced_calls']), (1, 1, 3))
        # The true cost is at least .8, so the deltas are lower bounds, rounded so they still hold.
        self.assertEqual(auto['vs_normal']['cost_delta_kind'], 'conservative_lower_bound')
        self.assertAlmostEqual(auto['vs_normal']['cost_delta_percent_lower_bound'], 100 * (.8 / 1.5 - 1))
        self.assertEqual(auto['vs_normal']['cost_delta_display_lower_bound'], -46.7)
        self.assertEqual(auto['vs_native']['cost_delta_display_lower_bound'], -73.4)
        self.assertIsNone(auto['vs_normal']['cost_delta_percent'])

    def test_failed_arm_keeps_its_numbers_but_fails_the_gate(self):
        with tempfile.TemporaryDirectory() as directory:
            path = family(Path(directory) / 'f.json', synthetic())
            fast = build([path])['pairs']['astra']['arms']['ultrafast']
            self.assertEqual((fast['quality_passes'], fast['runs']), (1, 3))
            self.assertAlmostEqual(fast['vs_normal']['quality_delta_percentage_points'], 100 * (1 / 3 - 1))
            self.assertFalse(fast['quality_gate']['passed'])
            self.assertIn('1/3 successes against Normal 3/3', fast['quality_gate']['reason'])
            self.assertTrue(build([path], tolerance=2)['pairs']['astra']['arms']['ultrafast']['quality_gate']['passed'])
            runs = synthetic()
            runs[6]['result']['execution_mode_activation_confirmed'] = False  # an UltraSave cell
            save = build([family(path, runs)])['pairs']['astra']['arms']['ultrasave']
            self.assertFalse(save['configuration_verified'] or save['quality_gate']['passed'])
            self.assertIn('configuration', save['quality_gate']['reason'])
            no_normal = build([family(path, [r for r in synthetic() if r['mode'] != 'normal'])])['pairs']['astra']
            self.assertNotIn('distill', no_normal)
            self.assertEqual(no_normal['arms']['ultrasave']['quality_gate']['reason'], 'no Normal arm to gate against')

    def test_pricing_record_must_match_the_cost_model(self):
        record = json.loads(PRICING.read_text())
        check_pricing(record)
        record['claude_usd_per_million']['claude-opus-5-5']['output'] = 25
        with self.assertRaisesRegex(ValueError, 'claude-opus-5-5 output'):
            check_pricing(record)

    def test_cli_writes_once_and_never_overwrites(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / 'summary.json'
            self.assertEqual(main(['--families', *map(str, LEGACY), '--output', str(output)]), 0)
            written = output.read_text()
            self.assertEqual(json.loads(written)['pairs']['opus']['source'],
                             'tools/task_cost_eval/results/requested-20261016-opus.json')
            with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                main(['--families', *map(str, LEGACY), '--output', str(output)])
            self.assertEqual(output.read_text(), written)


if __name__ == '__main__':
    unittest.main()
