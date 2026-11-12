import unittest

from .auto_verdict import acceptance_shortfall, verdict
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


if __name__ == '__main__':
    unittest.main()
