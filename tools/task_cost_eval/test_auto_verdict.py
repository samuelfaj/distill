import copy
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


def picked(arm_rows, picks):
    for row in arm_rows['auto']:
        row['first_mode_pick'] = picks.get(row['case'])
    return arm_rows


def arm_rows(overrides=None):
    def values(arm, case, rep):
        credits, wall = COSTS[arm]
        return {'credits': credits, 'wall': wall, **((overrides or {}).get((arm, case, rep), {}))}
    data = campaign(ARMS, values=values)
    return {arm: [row for row in data['runs'] if row['variant'] == arm] for arm in ARMS}


class AutoOracleConfusionTest(unittest.TestCase):
    """S-030: ties inside the A/A deviation, Auto's first pick against the oracle and the expected mode."""
    # Case b on the choosing repetitions (1 and 3): Normal 4.0 + 4.0 = 8.0 credits, UltraSave 4.2 + 4.2 = 8.4.
    CLOSE = {(arm, 'b', rep): {'credits': credits} for rep in (1, 3)
             for arm, credits in (('normal', 4.0), ('ultrasave', 4.2))}
    PICKS = {'a': 'ultrasave', 'b': 'ultrasave', 'c': 'ultrafast', 'd': None}

    def oracle(self, band, expected=None):
        rows = picked(arm_rows(self.CLOSE), self.PICKS)
        return matrix_verdict(rows, 3, tie_band={'credits': band, 'charged_completion_s': band},
                              expected_modes=expected)

    def test_inside_the_aa_band_a_close_second_counts_as_the_oracle(self):
        # 8.4 <= 8.0 x 1.06, so on case b Auto's UltraSave pick agrees with the oracle.
        credits = self.oracle(0.06)['per_case_oracle']['credits']
        self.assertEqual(credits['chosen_on_odd_repetitions']['b'], 'normal')
        self.assertEqual(credits['tied_on_odd_repetitions']['b'], ['normal', 'ultrasave'])
        self.assertEqual(credits['first_pick'], {'confusion': {'ultrasave': {'ultrafast': 1, 'ultrasave': 2,
                                                                             'unknown': 1}},
                                                 'scored': 3, 'unknown': 1, 'agreement': 2 / 3})

    def test_outside_the_band_the_same_pick_is_a_miss(self):
        credits = self.oracle(0.0)['per_case_oracle']['credits']
        self.assertEqual(credits['tied_on_odd_repetitions']['b'], ['normal'])
        self.assertEqual(credits['first_pick']['confusion'],
                         {'normal': {'ultrasave': 1}, 'ultrasave': {'ultrafast': 1, 'ultrasave': 1, 'unknown': 1}})
        self.assertAlmostEqual(credits['first_pick']['agreement'], 1 / 3)

    def test_regret_in_credits_is_scored_on_the_even_repetition(self):
        # Repetition 2: the oracle pays UltraSave 4 on a, c, d and Normal 10 on b (chosen on 1 and 3): 22.
        credits = self.oracle(0.06)['per_case_oracle']['credits']
        self.assertEqual((credits['oracle_total'], credits['auto_total']), (22.0, 20.0))
        self.assertAlmostEqual(credits['regret_delta'], -2.0)
        self.assertAlmostEqual(credits['regret'], 20 / 22)
        seconds = self.oracle(0.06)['per_case_oracle']['charged_completion_s']
        self.assertEqual(set(seconds['chosen_on_odd_repetitions'].values()), {'ultrafast'})
        self.assertAlmostEqual(seconds['regret_delta'], 4 * (70 - 50))

    def test_first_pick_against_the_expected_mode_of_each_case(self):
        report = self.oracle(0.06, {'a': 'ultrasave', 'b': 'normal', 'c': 'ultrafast', 'd': 'ultracode'})
        self.assertEqual(report['first_pick_vs_expected'],
                         {'confusion': {'normal': {'ultrasave': 3}, 'ultracode': {'unknown': 3},
                                        'ultrafast': {'ultrafast': 3}, 'ultrasave': {'ultrasave': 3}},
                          'scored': 9, 'unknown': 3, 'agreement': 6 / 9})
        self.assertNotIn('first_pick_vs_expected', self.oracle(0.06))

    def test_a_lever_off_arm_is_never_a_pinned_reference(self):
        rows = arm_rows()
        rows['normal-no-c4_diff_risk'] = copy.deepcopy(rows['normal'])
        for row in rows['normal-no-c4_diff_risk']:
            row.update(variant='normal-no-c4_diff_risk', credits=1.0)
        report = matrix_verdict(rows, 3)
        self.assertNotIn('normal-no-c4_diff_risk', report['pinned_arms'])
        self.assertEqual(report['regret']['credits']['best_pinned_arm'], 'ultrasave')


def with_records(data, abandoned):
    """Decision records on every paired row: `abandoned` of 10 mode picks per arm."""
    for row in data['runs']:
        count = abandoned[row['variant']]
        row.update(decision_events_found=True, decision_labels={'mode.picks': 10, 'mode.abandoned': count})
    aa = [dict(copy.deepcopy(row), variant='baseline_aa') for row in data['runs'] if row['variant'] == 'baseline']
    for row in aa:
        row['decision_labels'] = {'mode.picks': 10, 'mode.abandoned': abandoned['baseline_aa']}
    data['aa_runs'] = aa
    return data


class AutoDecisionGateTest(unittest.TestCase):
    def auto_against_auto(self):
        data = result(baseline_mode='auto')
        data['compare_versions'] = True
        return data

    def test_against_a_previous_auto_the_gate_is_required(self):
        report = verdict(with_records(self.auto_against_auto(), {'baseline': 2, 'baseline_aa': 3, 'candidate': 3}))
        self.assertEqual((report['verdict'], report['decision_gate']['status']), ('PASS', 'PASS'), report['reasons'])
        self.assertTrue(report['decision_gate']['required'])
        report = verdict(with_records(self.auto_against_auto(), {'baseline': 2, 'baseline_aa': 3, 'candidate': 5}))
        self.assertEqual((report['verdict'], report['decision_gate']['status']), ('FAIL', 'FAIL'))
        self.assertIn('decision metrics against the A/A deviation: FAIL', report['reasons'])

    def test_no_data_never_passes_a_claim_that_depends_on_it(self):
        report = verdict(self.auto_against_auto())
        self.assertEqual(report['decision_gate']['status'], 'NO DATA')
        self.assertEqual(report['verdict'], 'FAIL')
        self.assertIn('decision metrics against the A/A deviation: NO DATA', report['reasons'])

    def test_no_data_or_fail_never_blocks_a_claim_that_does_not_depend_on_it(self):
        report = verdict(result())
        self.assertEqual((report['verdict'], report['decision_gate']['status']), ('PASS', 'NO DATA'))
        self.assertFalse(report['decision_gate']['required'])
        report = verdict(with_records(result(), {'baseline': 2, 'baseline_aa': 2, 'candidate': 9}))
        self.assertEqual((report['verdict'], report['decision_gate']['status']), ('PASS', 'FAIL'))


if __name__ == '__main__':
    unittest.main()
