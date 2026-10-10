import unittest

from .study_verdict import analyze


def rows(base=100, candidate=70, base_time=100, candidate_time=70, candidate_pass=True):
    settings = {'model': 'same', 'effort': 'medium', 'configured_models': {},
                'configured_utility': {}, 'configured_main_effort_auto': False,
                'binary_sha256': 'a' * 64}
    out = []
    for case in ('s', 'c'):
        for rep in (1, 2, 3):
            for variant in ('baseline', 'candidate'):
                cand = variant == 'candidate'
                out.append({'variant': variant, 'case': case, 'repetition': rep,
                    'passed': not (cand and not candidate_pass), 'credits': candidate if cand else base,
                    'wall_time_s': candidate_time if cand else base_time, 'accounting_complete': True,
                    'ultracode_activation_confirmed': True, 'frozen_inputs_unchanged': True,
                    'binary_unchanged': True, 'settings': settings})
    return out


def gate(data):
    return analyze(data, ['s'], ['c'], 3)


class StudyVerdictTest(unittest.TestCase):
    def test_success_and_unreviewed_is_never_reviewed(self):
        data = rows()
        for row in data:
            if row['variant'] == 'candidate':
                row['settings'] = dict(row['settings'], binary_sha256='b' * 64)
        out = gate(data)
        self.assertEqual(out['performance_verdict'], 'GO')
        self.assertNotIn('verdict', out)
        self.assertIn('required external confirmation receipt', out['blind_review'])

    def test_candidate_failure_counts_against_acceptance_and_charges_900(self):
        out = gate(rows(candidate_pass=False))
        self.assertEqual(out['performance_verdict'], 'NO_GO')
        c = out['performance']['all']['s']['candidate']
        self.assertEqual(c['accepted'], 0)
        self.assertEqual(c['charged_elapsed_s'], 2700)
        self.assertEqual(c['raw_elapsed_s'], 210)

    def test_missing_accounting_or_pair_is_inconclusive(self):
        data = rows()
        data[0]['credits'] = None
        self.assertEqual(gate(data)['performance_verdict'], 'INCONCLUSIVE')
        data = rows()
        data.pop()
        self.assertEqual(gate(data)['performance_verdict'], 'INCONCLUSIVE')

    def test_missing_or_changing_binary_hash_is_inconclusive(self):
        data = rows()
        del data[0]['settings']['binary_sha256']
        self.assertEqual(gate(data)['performance_verdict'], 'INCONCLUSIVE')
        data = rows()
        data[-1]['settings'] = dict(data[-1]['settings'], binary_sha256='b' * 64)
        self.assertEqual(gate(data)['performance_verdict'], 'INCONCLUSIVE')

    def test_substantial_regression_cannot_hide_behind_control_savings(self):
        data = rows(base=100, candidate=100)
        for r in data:
            if r['case'] == 's' and r['variant'] == 'candidate':
                r['credits'] = 130
            if r['case'] == 'c' and r['variant'] == 'candidate':
                r['credits'] = 1
        out = gate(data)
        self.assertEqual(out['performance_verdict'], 'NO_GO')
        self.assertIn('substantial credits per accepted task not met', out['reasons'])

    def test_control_regression_fails_its_own_cap(self):
        data = rows()
        for r in data:
            if r['case'] == 'c' and r['variant'] == 'candidate':
                r['credits'] = 120
        out = gate(data)
        self.assertEqual(out['performance_verdict'], 'NO_GO')
        self.assertIn('controls credits <= 1.10 baseline not met', out['reasons'])


if __name__ == '__main__':
    unittest.main()
