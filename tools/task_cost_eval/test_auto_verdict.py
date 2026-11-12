import unittest

from .auto_verdict import acceptance_shortfall, matrix_verdict, verdict
from .test_modes_verdict import campaign
from .test_ultrafast_verdict import FOUR
from .test_ultrasave_verdict import result as paired_result


def result(cases=('parallel', 'sequential'), baseline_mode='normal'):
    """Candidate runs Auto: activation reports the mode Auto chose (normal) plus choice 'auto'."""
    data = paired_result(cases)
    for row in data['runs']:
        if row['variant'] == 'candidate':
            row['settings']['execution_mode'] = 'auto'
            row['acp']['activation_receipt']['result'] = {'mode': 'normal', 'choice': 'auto'}
        else:
            row['settings']['execution_mode'] = baseline_mode
            row['acp']['activation_receipt']['result'] = {'mode': baseline_mode}
    return data


class AutoVerdictTest(unittest.TestCase):
    def test_cheaper_and_not_slower_passes_with_auto_choice_receipt(self):
        report = verdict(result())
        self.assertEqual(report['verdict'], 'PASS', report['reasons'])
        self.assertAlmostEqual(report['median_paired_credit_ratio'], .4)
        self.assertAlmostEqual(report['median_paired_completion_ratio'], 56 / 101)

    def test_slower_and_pricier_fails_both_ratios(self):
        data = result()
        for row in data['runs']:
            if row['variant'] == 'candidate':
                row['wall_time_s'] = 150
                row['credits'] = 12
        report = verdict(data)
        self.assertEqual(report['verdict'], 'FAIL')
        self.assertTrue(any('credit ratio' in reason for reason in report['reasons']), report['reasons'])
        self.assertTrue(any('completion ratio' in reason for reason in report['reasons']), report['reasons'])

    def test_auto_may_trail_the_pinned_arm_by_at_most_one_in_sixty_runs(self):
        # 12 runs allow 0.2 missed acceptances: one lost task already fails.
        data = result(FOUR)
        data['runs'][1]['passed'] = False
        report = verdict(data)
        self.assertEqual((report['quality_regressions'], report['verdict']), (1, 'FAIL'))
        self.assertTrue(any('Auto accepted 11 of 12 against 12' in reason for reason in report['reasons']),
                        report['reasons'])
        # The rule is relative: a task both arms fail is not held against Auto.
        data['runs'][0]['passed'] = False
        report = verdict(data)
        self.assertEqual(report['quality_regressions'], 0)
        self.assertFalse(any('Auto accepted' in reason for reason in report['reasons']), report['reasons'])

    def test_one_in_sixty_allowance_scales_with_the_campaign(self):
        self.assertEqual(acceptance_shortfall(59, 60, 60), 0)
        self.assertEqual(acceptance_shortfall(58, 60, 60), 2)
        self.assertEqual(acceptance_shortfall(11, 12, 12), 1)
        self.assertEqual(acceptance_shortfall(13, 12, 12), 0)

    def test_mode_only_activation_for_auto_candidate_is_incomplete(self):
        data = result()
        for row in data['runs']:
            if row['variant'] == 'candidate':
                row['acp']['activation_receipt']['result'] = {'mode': 'normal'}
        report = verdict(data)
        self.assertEqual(report['verdict'], 'INCOMPLETE')
        self.assertIsNone(report['candidate'])

    def test_specialist_tradeoff_is_reported_without_claiming_a_performance_win(self):
        data = result(baseline_mode='ultrafast')
        for row in data['runs']:
            if row['variant'] == 'candidate':
                row['wall_time_s'] = 110
                row['credits'] = 12
        report = verdict(data)
        self.assertEqual(report['verdict'], 'PASS', report['reasons'])
        self.assertEqual(report['comparison'], 'specialist_tradeoff')
        self.assertFalse(report['performance_gate_required'])
        self.assertIsNone(report['performance_target_met'])
        self.assertGreater(report['median_paired_credit_ratio'], 1)
        self.assertGreater(report['median_paired_completion_ratio'], 1)
        self.assertIsNone(report['target']['maximum_completion_ratio_of_totals'])

    def test_specialist_comparison_still_applies_the_acceptance_rule(self):
        data = result(baseline_mode='ultrasave')
        data['runs'][1]['passed'] = False
        report = verdict(data)
        self.assertEqual(report['verdict'], 'FAIL', report['reasons'])
        self.assertEqual(report['quality_regressions'], 1)


ARMS = ('normal', 'ultrasave', 'ultrafast', 'auto')
COSTS = {'normal': (10.0, 100.0), 'ultrasave': (4.0, 120.0), 'ultrafast': (12.0, 50.0), 'auto': (5.0, 70.0)}


def matrix(overrides=None):
    def values(arm, case, rep):
        credits, wall = COSTS[arm]
        return {'credits': credits, 'wall': wall, **((overrides or {}).get((arm, case, rep), {}))}
    data = campaign(ARMS, values=values)
    by_arm = {arm: [row for row in data['runs'] if row['variant'] == arm] for arm in ARMS}
    return matrix_verdict(by_arm, data['repetitions'])


class AutoMatrixVerdictTest(unittest.TestCase):
    def test_regret_is_against_the_best_single_pinned_mode_on_each_axis(self):
        report = matrix()
        self.assertEqual(report['verdict'], 'PASS', report['reasons'])
        self.assertEqual(report['regret']['credits']['best_pinned_arm'], 'ultrasave')
        self.assertAlmostEqual(report['regret']['credits']['regret'], 5 / 4)
        self.assertEqual(report['regret']['charged_completion_s']['best_pinned_arm'], 'ultrafast')
        self.assertAlmostEqual(report['regret']['charged_completion_s']['regret'], 70 / 50)

    def test_a_pinned_arm_below_the_acceptance_bar_is_not_the_regret_reference(self):
        lost = {('ultrasave', case, 1): {'passed': False} for case in 'abc'}
        report = matrix(lost)
        self.assertEqual(report['regret']['credits']['best_pinned_arm'], 'normal')
        self.assertEqual(report['verdict'], 'PASS', 'Auto is compared with the best pinned acceptance, 12 of 12')

    def test_auto_fails_when_it_trails_the_best_pinned_arm_beyond_one_in_sixty(self):
        report = matrix({('auto', 'a', 2): {'passed': False}})
        self.assertEqual(report['verdict'], 'FAIL')
        self.assertTrue(any('Auto accepted 11 of 12 against 12' in reason for reason in report['reasons']))

    def test_oracle_is_chosen_on_odd_repetitions_and_scored_on_even_ones(self):
        # UltraSave is cheapest on repetitions 1 and 3 but expensive on 2: an
        # oracle chosen and scored on the same runs would hide that noise.
        report = matrix({('ultrasave', case, 2): {'credits': 20.0} for case in 'abcd'})
        oracle = report['per_case_oracle']['credits']
        self.assertEqual(set(oracle['chosen_on_odd_repetitions'].values()), {'ultrasave'})
        self.assertEqual(oracle['scored_repetitions'], [2])
        self.assertEqual((oracle['oracle_total'], oracle['auto_total']), (80.0, 20.0))
        self.assertAlmostEqual(oracle['regret'], 0.25)


if __name__ == '__main__':
    unittest.main()
