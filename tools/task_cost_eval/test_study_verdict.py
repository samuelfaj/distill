import unittest

from .study_verdict import analyze


def rows(base=100, candidate=70, base_time=100, candidate_time=70, candidate_pass=True):
    out = []
    for case in ('s', 'c'):
        for rep in (1, 2, 3):
            for variant in ('baseline', 'candidate'):
                cand = variant == 'candidate'
                digest = 'b' * 64 if cand else 'a' * 64
                settings = {'transport': 'acp', 'ultracode': True, 'max_depth': 2 if cand else 1,
                    'depth_env': '2' if cand else '1', 'timeout_s': 900, 'order_seed': 20261009,
                    'model': 'chatgpt/gpt-6.1-sol', 'effort': 'medium',
                    'configured_models': {'default': 'chatgpt/gpt-6.1-sol', 'worker': 'chatgpt/gpt-6-luna',
                        'worker_effort': 'medium', 'session_summary': 'chatgpt/gpt-6-luna'},
                    'configured_utility': {'model': 'chatgpt/gpt-6-luna', 'effort': 'medium'},
                    'configured_main_effort_auto': True, 'binary_sha256': digest}
                acp = {'setup_confirmed': True, 'completed': True, 'cleanup_complete': True,
                    'timed_out': False, 'error': None, 'exit_code': 0,
                    'model_receipt': {'id': 3, 'result': {'_meta': {'canonicalModelId': 'chatgpt/gpt-6.1-sol',
                        'reasoningEffort': 'medium', 'reasoningEffortAuto': False}}},
                    'activation_receipt': {'id': 4, 'result': {'enabled': True}},
                    'prompt_receipt': {'id': 5, 'result': {'stopReason': 'end_turn'}}}
                out.append({'variant': variant, 'case': case, 'repetition': rep,
                    'passed': not (cand and not candidate_pass), 'credits': candidate if cand else base,
                    'wall_time_s': candidate_time if cand else base_time, 'accounting_complete': True,
                    'ultracode_activation_confirmed': True, 'frozen_inputs_unchanged': True,
                    'binary_unchanged': True, 'settings': settings, 'acp': acp,
                    'accounting_evidence': {'complete': True, 'reasons': []},
                    'call_usage': [
                        {'attempt': 'initial-title:id', 'model': 'gpt-6-luna', 'role': 'auxiliary',
                        'status': 'completed', 'agent': 'main', 'endpoint': 'https://chatgpt.com/backend-api/codex/responses', 'effort': 'effort:low',
                        'complete': True, 'credits': 1, 'input_tokens': 1, 'cached_input_tokens': 0,
                        'output_tokens': 1, 'reasoning_tokens': 0},
                        {'attempt': 'sampler:a', 'model': 'gpt-6.1-sol', 'role': 'main',
                        'status': 'completed', 'agent': 'main', 'endpoint': 'https://chatgpt.com/backend-api/codex/responses', 'effort': 'effort:medium',
                        'complete': True, 'credits': 1, 'input_tokens': 1, 'cached_input_tokens': 0,
                        'output_tokens': 1, 'reasoning_tokens': 0},
                        {'attempt': 'sampler:b', 'model': 'gpt-6-luna', 'role': 'main',
                        'status': 'completed', 'agent': 'worker', 'endpoint': 'https://chatgpt.com/backend-api/codex/responses', 'effort': 'effort:medium',
                        'complete': True, 'credits': 1, 'input_tokens': 1, 'cached_input_tokens': 0,
                        'output_tokens': 1, 'reasoning_tokens': 0}]})
    return out


def gate(data):
    provenance = {
        'baseline': {'source_sha': 'ac7d11b7a4a97c9a1ffadbe6c288ed9783259414', 'binary_sha256': 'a' * 64,
                     'build_profile': 'cargo-default-dev', 'compiler_version': 'rustc test'},
        'candidate': {'source_sha': '1' * 40, 'binary_sha256': 'b' * 64,
                      'build_profile': 'cargo-default-dev', 'compiler_version': 'rustc test'}}
    return analyze(data, ['s'], ['c'], 3, binary_provenance=provenance)


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

    def test_accounted_timeout_after_verified_setup_is_retained_as_failed_run(self):
        data = rows()
        row = next(r for r in data if r['variant'] == 'candidate')
        row['passed'] = False
        row['wall_time_s'] = 12
        row['acp'].update(completed=False, exit_code=-15, timed_out=True, error='deadline exceeded')
        out = gate(data)
        self.assertEqual(out['performance_verdict'], 'NO_GO')
        candidate = out['performance']['all']['s']['candidate']
        self.assertEqual(candidate['accepted'], 2)
        self.assertEqual(candidate['charged_elapsed_s'], 1040)
        self.assertEqual(candidate['raw_elapsed_s'], 152)

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
        data[-1]['settings'] = dict(data[-1]['settings'], binary_sha256='c' * 64)
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

    def test_tampered_settings_applied_calls_or_missing_audit_and_provenance_are_inconclusive(self):
        for field, mutate in (
            ('settings', lambda r: r['settings'].__setitem__('timeout_s', 30)),
            ('applied call', lambda r: r['call_usage'][0].__setitem__('effort', 'high')),
            ('audit', lambda r: r.pop('accounting_evidence')),
        ):
            data = rows(); mutate(data[0])
            with self.subTest(field=field):
                self.assertEqual(gate(data)['performance_verdict'], 'INCONCLUSIVE')
        data = rows()
        self.assertEqual(analyze(data, ['s'], ['c'], 3)['performance_verdict'], 'INCONCLUSIVE')


if __name__ == '__main__':
    unittest.main()
