import copy
import unittest

from .ultrafast_verdict import verdict


def result(cases=('parallel', 'sequential'), repetitions=3):
    """Gate-sized campaign: rows[0] and rows[1] are the first case's first pair."""
    rows = []
    arms = (('baseline', 'normal', 100), ('candidate', 'ultrafast', 55))
    for repetition, case, (variant, mode, wall) in (
            (rep, case, arm) for rep in range(1, repetitions + 1) for case in cases for arm in arms):
        rows.append({'case': case, 'repetition': repetition, 'variant': variant, 'passed': True,
            'wall_time_s': wall, 'credits': 1, 'grader': {'wall_time_s': 1},
            'settings': {'execution_mode': mode, 'transport': 'acp', 'timeout_s': 900,
                'binary_sha256': 'same', 'model': 'same', 'effort': 'auto', 'max_concurrent': 8},
            'acp': {'setup_confirmed': True, 'cleanup_complete': True,
                'activation_receipt': {'result': {'mode': mode}}},
            'execution_mode_activation_confirmed': True, 'accounting_complete': True,
            'accounting_evidence': {'complete': True},
            'call_usage': [{'complete': True, 'model': 'same', 'effort': 'low'}],
            'frozen_inputs_unchanged': True, 'binary_unchanged': True})
    return {'cases': list(cases), 'repetitions': repetitions, 'runs': rows}


def versions(data, candidate_wall):
    """Head-to-head: both variants UltraFast, different binaries."""
    data['compare_versions'] = True
    for row in data['runs']:
        row['settings']['execution_mode'] = 'ultrafast'
        row['acp']['activation_receipt']['result'] = {'mode': 'ultrafast'}
        row['wall_time_s'] = candidate_wall if row['variant'] == 'candidate' else 100
        row['settings']['binary_sha256'] = row['variant']
    return data


FOUR = ('a', 'b', 'c', 'd')


class UltraFastVerdictTest(unittest.TestCase):
    def test_same_quality_and_over_thirty_percent_paired_savings_pass(self):
        report = verdict(result())
        self.assertEqual(report['verdict'], 'PASS')
        self.assertAlmostEqual(report['median_paired_completion_ratio'], 56 / 101)
        self.assertAlmostEqual(report['completion_ratio_of_totals'], 56 / 101)
        self.assertEqual(report['candidate']['accepted'], 6)
        self.assertEqual(report['candidate']['calls_by_model_effort'], {'same/low': 6})

    def test_forty_percent_target_is_the_boundary(self):
        data = result()
        for row in data['runs']:
            if row['variant'] == 'candidate':
                row['wall_time_s'] = 60  # 61 / 101 > 0.60
        self.assertEqual(verdict(data)['verdict'], 'FAIL')

    def test_one_quality_regression_is_tolerated_when_speed_holds(self):
        # The failure is charged the 900 s timeout in the totals, so speed only
        # holds when the other pairs save more than that charge.
        data = result(FOUR)
        for row in data['runs']:
            if row['variant'] == 'baseline':
                row['wall_time_s'] = 300
        data['runs'][1].update(passed=False, wall_time_s=.1)
        report = verdict(data)
        self.assertEqual(report['quality_regressions'], 1)
        self.assertEqual(report['verdict'], 'PASS', report['reasons'])

    def test_two_quality_regressions_fail_even_when_median_speed_holds(self):
        data = result(FOUR)
        for index in (1, 3):
            data['runs'][index].update(passed=False, wall_time_s=.1)
        report = verdict(data)
        self.assertEqual(report['quality_regressions'], 2)
        self.assertEqual(report['verdict'], 'FAIL')
        self.assertTrue(any('baseline-passing pair failed' in reason for reason in report['reasons']))

    def test_candidate_must_accept_all_but_one_even_without_regressions(self):
        data = result(FOUR)
        for index in (0, 1, 2, 3):  # baseline and candidate both fail two cases
            data['runs'][index].update(passed=False)
        report = verdict(data)
        self.assertEqual(report['quality_regressions'], 0)
        self.assertEqual(report['verdict'], 'FAIL')
        self.assertTrue(any('accepted 10 of 12' in reason for reason in report['reasons']))

    def test_compare_versions_allows_same_mode_and_different_binaries_with_looser_target(self):
        report = verdict(versions(result(), 80))
        self.assertEqual(report['verdict'], 'PASS', report['reasons'])
        self.assertEqual(report['target']['maximum_completion_ratio_of_totals'], .85)
        self.assertEqual(verdict(versions(result(), 90))['verdict'], 'FAIL')

    def test_strict_mode_still_rejects_same_mode_and_different_binaries(self):
        data = versions(result(), 80)
        data['compare_versions'] = False
        self.assertEqual(verdict(data)['verdict'], 'INCOMPLETE')

    def test_compare_versions_still_requires_matching_jev_and_utility_settings(self):
        for field, value in (('jev_enabled', True), ('utility_model', 'other')):
            with self.subTest(field=field):
                data = versions(result(), 80)
                data['runs'][1]['settings'][field] = value
                self.assertEqual(verdict(data)['verdict'], 'INCOMPLETE')

    def test_early_failure_is_charged_and_cannot_manufacture_speed(self):
        data = result()
        data['runs'][1].update(passed=False, wall_time_s=.1)
        report = verdict(data)
        self.assertEqual(report['verdict'], 'FAIL')
        self.assertEqual(report['quality_regressions'], 1)
        self.assertEqual(report['candidate']['charged_completion_s'], 900 + 5 * 56)
        self.assertEqual(report['candidate']['credits'], 6, 'failed usage remains in totals')

    def test_smaller_improvement_does_not_meet_predeclared_speed_target(self):
        data = result()
        for row in data['runs']:
            if row['variant'] == 'candidate':
                row['wall_time_s'] = 80
        self.assertEqual(verdict(data)['verdict'], 'FAIL')

    def test_incomplete_pairs_or_usage_or_unmatched_binary_are_incomplete(self):
        original = result()
        for failure in ('missing-pair', 'usage', 'binary'):
            with self.subTest(failure=failure):
                data = copy.deepcopy(original)
                if failure == 'missing-pair':
                    data['runs'].pop()
                elif failure == 'usage':
                    data['runs'][1]['accounting_complete'] = False
                else:
                    data['runs'][1]['settings']['binary_sha256'] = 'other'
                self.assertEqual(verdict(data)['verdict'], 'INCOMPLETE')


class GateStatisticTest(unittest.TestCase):
    def test_ratio_of_totals_is_neutral_where_the_median_of_paired_ratios_is_biased(self):
        # Same total work in both arms, split unevenly across cases: an
        # unbiased gate statistic must read 1.0, the median of ratios does not.
        data = result()
        for row in data['runs']:
            fast = (row['case'] == 'parallel') == (row['variant'] == 'candidate')
            row['wall_time_s'] = 50 if fast else 100
        report = verdict(data)
        self.assertAlmostEqual(report['completion_ratio_of_totals'], 1.0)
        self.assertGreater(report['median_paired_completion_ratio'], 1.1)
        interval = report['completion_ratio_bootstrap']
        self.assertLessEqual(interval['lower'], 1.0)
        self.assertGreaterEqual(interval['upper'], 1.0)
        self.assertEqual(interval['resampled_unit'], 'case')

    def test_fewer_than_three_repetitions_is_a_smoke_not_a_verdict(self):
        report = verdict(result(repetitions=1))
        self.assertEqual(report['verdict'], 'SMOKE')
        self.assertEqual(report['smoke_verdict'], 'PASS')
        self.assertTrue(any('smoke only' in reason for reason in report['reasons']))

    def test_failed_jev_calls_name_the_outage_instead_of_generic_accounting(self):
        data = result()
        data['runs'][1].update(accounting_complete=False, jev_calls=4, jev_failed_calls=4)
        report = verdict(data)
        self.assertEqual(report['verdict'], 'INCOMPLETE')
        self.assertIn('candidate parallel#1: Jev unavailable (4 of 4 calls failed)', report['reasons'])
        self.assertFalse(any('incomplete all-call accounting' in reason for reason in report['reasons']))
