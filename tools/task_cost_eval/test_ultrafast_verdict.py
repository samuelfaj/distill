import copy
import unittest

from .ultrafast_verdict import verdict


def result(cases=('parallel', 'sequential')):
    rows = []
    for case in cases:
        for variant, mode, wall in (('baseline', 'normal', 100), ('candidate', 'ultrafast', 55)):
            rows.append({'case': case, 'repetition': 1, 'variant': variant, 'passed': True,
                'wall_time_s': wall, 'credits': 1, 'grader': {'wall_time_s': 1},
                'settings': {'execution_mode': mode, 'transport': 'acp', 'timeout_s': 900,
                    'binary_sha256': 'same', 'model': 'same', 'effort': 'auto', 'max_concurrent': 8},
                'acp': {'setup_confirmed': True, 'cleanup_complete': True,
                    'activation_receipt': {'result': {'mode': mode}}},
                'execution_mode_activation_confirmed': True, 'accounting_complete': True,
                'accounting_evidence': {'complete': True},
                'call_usage': [{'complete': True, 'model': 'same', 'effort': 'low'}],
                'frozen_inputs_unchanged': True, 'binary_unchanged': True})
    return {'cases': list(cases), 'repetitions': 1, 'runs': rows}


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
        self.assertEqual(report['candidate']['accepted'], 2)
        self.assertEqual(report['candidate']['calls_by_model_effort'], {'same/low': 2})

    def test_forty_percent_target_is_the_boundary(self):
        data = result()
        for row in data['runs']:
            if row['variant'] == 'candidate':
                row['wall_time_s'] = 60  # 61 / 101 > 0.60
        self.assertEqual(verdict(data)['verdict'], 'FAIL')

    def test_one_quality_regression_is_tolerated_when_speed_holds(self):
        data = result(FOUR)
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

    def test_candidate_must_accept_all_but_one_even_without_regressions(self):
        data = result(FOUR)
        for index in (0, 1, 2, 3):  # baseline and candidate both fail two cases
            data['runs'][index].update(passed=False)
        report = verdict(data)
        self.assertEqual(report['quality_regressions'], 0)
        self.assertEqual(report['verdict'], 'FAIL')
        self.assertTrue(any('accepted 2 of 4' in reason for reason in report['reasons']))

    def test_compare_versions_allows_same_mode_and_different_binaries_with_looser_target(self):
        report = verdict(versions(result(), 80))
        self.assertEqual(report['verdict'], 'PASS', report['reasons'])
        self.assertEqual(report['target']['maximum_median_paired_completion_ratio'], .85)
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
        self.assertEqual(report['candidate']['charged_completion_s'], 956)
        self.assertEqual(report['candidate']['credits'], 2, 'failed usage remains in totals')

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
