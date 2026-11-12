import unittest

from .test_modes_verdict import campaign
from .ultracode_verdict import analyze

SUBSTANTIAL = ('s1', 's2', 's3')
CONTROLS = ('c1', 'c2')


def run(normal_passes=None, ultracode_passes=None, extra=None):
    """normal/ultracode_passes: {case: passing repetitions}; every other run passes."""
    def values(arm, case, rep):
        passes = (normal_passes if arm == 'normal' else ultracode_passes) or {}
        value = {'credits': 10.0 if arm == 'normal' else 25.0, 'wall': 100.0 if arm == 'normal' else 200.0,
                 'passed': rep in passes.get(case, (1, 2, 3))}
        if arm == 'ultracode':
            value['review_overhead_s'] = [60.0, 120.0] if case in CONTROLS else []
            value['refutation_before_fix'] = True
        value.update((extra or {}).get((arm, case), {}))
        return value
    data = campaign(('normal', 'ultracode'), cases=SUBSTANTIAL + CONTROLS, values=values)
    return analyze(data['runs'], SUBSTANTIAL, CONTROLS, data['repetitions'])


# Normal solves each substantial case once in three; UltraCode solves all.
HARD = {case: (1,) for case in SUBSTANTIAL}


class UltraCodeVerdictTest(unittest.TestCase):
    def test_large_paired_gain_within_cost_caps_is_go(self):
        report = run(normal_passes=HARD)
        self.assertEqual(report['performance_verdict'], 'GO', report['reasons'])
        performance = report['performance']
        self.assertAlmostEqual(performance['accepted_point_gain'], 100 * 6 / 9)
        self.assertGreater(performance['gain_lower_bound_90'], 0)
        self.assertAlmostEqual(performance['substantial_credit_ratio_of_totals'], 2.5)
        self.assertEqual(performance['control_review_overhead_p90_s'], 120.0)
        self.assertEqual(performance['refuted_share_of_extra_passes'], 1.0)

    def test_gain_carried_by_one_case_has_no_positive_lower_bound(self):
        # +3 on s1, -1 on s2: 22 points overall, but one case carries all of it.
        report = run(normal_passes={'s1': (), 's3': (1, 2, 3)}, ultracode_passes={'s2': (1, 2)})
        self.assertGreaterEqual(report['performance']['accepted_point_gain'], 15)
        self.assertLessEqual(report['performance']['gain_lower_bound_90'], 0)
        self.assertEqual(report['performance_verdict'], 'NO_GO')
        self.assertIn('one-sided 90% bootstrap lower bound of the per-case gain > 0 not met', report['reasons'])

    def test_an_extra_control_failure_is_no_go(self):
        report = run(normal_passes=HARD, ultracode_passes={'c1': (1, 2)})
        self.assertEqual(report['performance_verdict'], 'NO_GO')
        self.assertEqual(report['performance']['control_extra_failures'], 1)

    def test_cost_caps_apply_to_substantial_cases(self):
        report = run(normal_passes=HARD, extra={('ultracode', case): {'credits': 31.0} for case in SUBSTANTIAL})
        self.assertEqual(report['performance_verdict'], 'NO_GO')
        self.assertIn('substantial credits <= 3x Normal not met', report['reasons'])

    def test_missing_optional_evidence_is_inconclusive_not_zero(self):
        for field in ('review_overhead_s', 'refutation_before_fix'):
            with self.subTest(field=field):
                extra = {('ultracode', case): {field: None} for case in SUBSTANTIAL + CONTROLS}
                report = run(normal_passes=HARD, extra=extra)
                self.assertEqual(report['performance_verdict'], 'INCONCLUSIVE')
                self.assertTrue(any(field in reason for reason in report['reasons']), report['reasons'])

    def test_extra_passes_without_a_preceding_refutation_are_no_go(self):
        extra = {('ultracode', case): {'refutation_before_fix': case == 's1'} for case in SUBSTANTIAL}
        report = run(normal_passes=HARD, extra=extra)
        self.assertAlmostEqual(report['performance']['refuted_share_of_extra_passes'], 1 / 3)
        self.assertEqual(report['performance_verdict'], 'NO_GO')

    def test_missing_arm_runs_are_inconclusive(self):
        data = campaign(('normal', 'ultracode'), cases=SUBSTANTIAL + CONTROLS)
        data['runs'].pop()
        self.assertEqual(analyze(data['runs'], SUBSTANTIAL, CONTROLS, 3)['performance_verdict'], 'INCONCLUSIVE')


if __name__ == '__main__':
    unittest.main()
