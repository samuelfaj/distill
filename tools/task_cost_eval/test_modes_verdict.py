import unittest

from .modes_verdict import arm_labels, normal_verdict, verdict

CASES = ('a', 'b', 'c', 'd')


def row(arm, case, rep, *, credits=10.0, wall=100.0, passed=True, **extra):
    """One complete ACP run row of an N-arm campaign (synthetic; no model call)."""
    mode = 'normal' if arm in ('normal_aa', 'baseline', 'candidate') else arm.split('-no-')[0]
    activation = {'mode': 'normal', 'choice': 'auto'} if mode == 'auto' else {'mode': mode}
    return {'variant': arm, 'case': case, 'kind': 'divisible', 'repetition': rep, 'passed': passed,
            'wall_time_s': wall, 'credits': credits, 'grader': {'wall_time_s': 0, 'exit_code': 0 if passed else 1},
            'settings': {'execution_mode': mode, 'transport': 'acp', 'timeout_s': 900, 'binary_sha256': 'same',
                         'model': 'same', 'effort': 'auto', 'max_concurrent': 8},
            'acp': {'setup_confirmed': True, 'cleanup_complete': True, 'activation_receipt': {'result': activation}},
            'execution_mode_activation_confirmed': True, 'accounting_complete': True,
            'accounting_evidence': {'complete': True}, 'call_usage': [{'complete': True}],
            'frozen_inputs_unchanged': True, 'binary_unchanged': True, **extra}


def campaign(arms, *, cases=CASES, repetitions=3, profile='single', values=None):
    """values(arm, case, rep) -> dict of row overrides."""
    rows = [row(arm, case, rep, **(values(arm, case, rep) if values else {}))
            for rep in range(1, repetitions + 1) for case in cases for arm in arms]
    return {'arms': list(arms), 'cases': list(cases), 'repetitions': repetitions, 'model_profile': profile,
            'runs': rows}


def per_arm(**by_arm):
    return lambda arm, case, rep: by_arm.get(arm, {})


class ModesVerdictTest(unittest.TestCase):
    def test_a_lever_off_arm_is_reported_beside_its_decision_gate_line(self):
        # No mode claim depends on decision metrics: NO DATA is reported and blocks nothing.
        report = verdict(campaign(('normal', 'normal_aa', 'normal-no-c4_diff_risk')))
        self.assertEqual(report['verdict'], 'PASS', report['reasons'])
        self.assertEqual(report['claims']['normal-no-c4_diff_risk']['status'], 'reported')
        self.assertEqual(report['decision_gate']['normal-no-c4_diff_risk']['status'], 'NO DATA')
        self.assertFalse(report['decision_gate']['normal-no-c4_diff_risk']['required'])

    def test_decision_gate_line_compares_each_arm_with_normal_inside_the_aa_deviation(self):
        def values(arm, case, rep):
            abandoned = {'normal': 2, 'normal_aa': 3, 'ultrafast': 6}[arm]
            return {'decision_events_found': True, 'decision_labels': {'mode.picks': 10, 'mode.abandoned': abandoned}}
        report = verdict(campaign(('normal', 'normal_aa', 'ultrafast'), values=values))
        gate = report['decision_gate']['ultrafast']
        self.assertEqual(gate['status'], 'FAIL')
        self.assertAlmostEqual(gate['metrics']['mode.abandoned_rate']['aa_deviation'], 0.1)
        self.assertNotIn('decision', ' '.join(report['reasons']), 'reported only')

    def test_arm_labels_name_the_second_normal_as_the_aa_arm(self):
        self.assertEqual(arm_labels(['normal', 'ultrasave', 'normal', 'auto']),
                         ['normal', 'ultrasave', 'normal_aa', 'auto'])

    def test_two_identical_arms_give_a_valid_neutral_verdict(self):
        # Uneven split across cases with equal totals: still neutral.
        report = verdict(campaign(('normal', 'normal_aa'), values=lambda arm, case, rep: {
            'credits': 5.0 if (case in 'ab') == (arm == 'normal') else 15.0}))
        self.assertEqual(report['verdict'], 'PASS', report['reasons'])
        self.assertTrue(report['aa_check']['valid'])
        self.assertAlmostEqual(report['aa_check']['credit_ratio'], 1.0)
        self.assertAlmostEqual(report['aa_check']['time_ratio'], 1.0)
        self.assertEqual(report['claims'], {})

    def test_aa_outside_the_band_marks_the_campaign_invalid(self):
        for field, value in (('credits', 11.5), ('wall', 115.0)):
            with self.subTest(field=field):
                report = verdict(campaign(('normal', 'normal_aa', 'ultrafast'), values=per_arm(normal_aa={field: value})))
                self.assertEqual(report['verdict'], 'INVALID')
                self.assertFalse(report['aa_check']['valid'])
                self.assertEqual(report['claims'], {}, 'no mode claim from an invalid campaign')

    def test_a_campaign_without_an_aa_arm_cannot_give_a_verdict(self):
        report = verdict(campaign(('normal', 'ultrafast')))
        self.assertEqual(report['verdict'], 'INCOMPLETE')
        self.assertTrue(any('A/A arm' in reason for reason in report['reasons']))

    def test_ultrafast_claim_needs_time_saving_beyond_twice_the_aa_deviation(self):
        def values(time):
            # A/A reads 1.08 on time: savings must clear a 0.16 margin.
            return per_arm(normal_aa={'wall': 108.0}, ultrafast={'wall': time})
        passing = verdict(campaign(('normal', 'normal_aa', 'ultrafast'), values=values(80.0)))
        self.assertEqual(passing['claims']['ultrafast']['status'], 'PASS', passing['claims'])
        self.assertAlmostEqual(passing['aa_margins']['time'], 0.16)
        inside_noise = verdict(campaign(('normal', 'normal_aa', 'ultrafast'), values=values(85.0)))
        self.assertEqual(inside_noise['claims']['ultrafast']['status'], 'no claim yet')
        self.assertEqual(inside_noise['verdict'], 'FAIL')

    def test_ultrafast_claim_caps_credits_at_one_point_one_five(self):
        report = verdict(campaign(('normal', 'normal_aa', 'ultrafast'),
                                  values=per_arm(ultrafast={'wall': 50.0, 'credits': 12.0})))
        self.assertEqual(report['claims']['ultrafast']['status'], 'no claim yet')
        self.assertIn({'gate': 'credits <= 1.15x Normal', 'passed': False}, report['claims']['ultrafast']['checks'])

    def test_ultrasave_money_claim_only_on_the_tiered_profile(self):
        values = per_arm(ultrasave={'credits': 4.0}, ultrafast={'wall': 50.0, 'credits': 10.0})
        arms = ('normal', 'normal_aa', 'ultrasave', 'ultrafast')
        single = verdict(campaign(arms, values=values))
        self.assertEqual(single['claims']['ultrasave']['status'], 'no claim')
        tiered = verdict(campaign(arms, values=values, profile='tiered'))
        self.assertEqual(tiered['claims']['ultrasave']['status'], 'PASS', tiered['claims'])
        self.assertTrue(tiered['separation']['distinct'])
        too_dear = verdict(campaign(arms, profile='tiered', values=per_arm(
            ultrasave={'credits': 5.0}, ultrafast={'wall': 50.0, 'credits': 10.0})))
        self.assertEqual(too_dear['claims']['ultrasave']['status'], 'no claim yet')

    def test_silent_failures_are_unknown_without_the_optional_fields(self):
        arms = ('normal', 'normal_aa', 'ultrafast')
        failing = {'passed': False}

        def values(extra):
            return lambda arm, case, rep: ({**failing, **extra} if arm == 'ultrafast' and (case, rep) == ('a', 1)
                                           else {'wall': 50.0} if arm == 'ultrafast' else {})
        unknown = verdict(campaign(arms, values=values({})))
        self.assertEqual(unknown['comparisons']['ultrafast']['silent_failures'], {'silent': 0, 'unknown': 1})
        self.assertEqual(unknown['claims']['ultrafast']['status'], 'no claim yet')
        escalated = verdict(campaign(arms, values=values({'escalations': 1, 'failed_check_facts': 0})))
        self.assertEqual(escalated['comparisons']['ultrafast']['silent_failures'], {'silent': 0, 'unknown': 0})
        silent = verdict(campaign(arms, values=values({'escalations': 0, 'failed_check_facts': 0})))
        self.assertEqual(silent['comparisons']['ultrafast']['silent_failures'], {'silent': 1, 'unknown': 0})
        self.assertIn({'gate': 'silent failures <= runs/60', 'passed': False},
                      silent['claims']['ultrafast']['checks'])

    def test_six_arm_campaign_reports_auto_and_asks_for_the_ultracode_case_split(self):
        arms = ('normal', 'normal_aa', 'ultrasave', 'ultrafast', 'ultracode', 'auto')
        report = verdict(campaign(arms, values=per_arm(ultrafast={'wall': 50.0})))
        self.assertEqual(report['auto']['verdict'], 'PASS', report['auto'])
        self.assertEqual(report['ultracode']['performance_verdict'], 'NOT_EVALUATED')
        self.assertEqual(set(report['comparisons']), {'ultrasave', 'ultrafast', 'ultracode', 'auto'})
        self.assertEqual(report['claims']['auto']['status'], 'reported')

    def test_declared_case_split_evaluates_ultracode_inside_the_campaign(self):
        data = campaign(('normal', 'normal_aa', 'ultracode'), values=per_arm(ultracode={'review_overhead_s': []}))
        data['ultracode_cases'] = {'substantial': ['a', 'b'], 'control': ['c', 'd']}
        report = verdict(data)
        # Both arms solve everything: no gain, so UltraCode earns no GO and the campaign reports it.
        self.assertEqual(report['ultracode']['performance_verdict'], 'NO_GO')
        self.assertEqual(report['ultracode']['performance']['accepted_point_gain'], 0)
        self.assertEqual(report['verdict'], 'FAIL')
        self.assertIn('ultracode: NO_GO', report['reasons'])

    def test_ultrasave_pays_for_money_with_at_most_one_point_five_times_normal_time(self):
        arms = ('normal', 'normal_aa', 'ultrasave')
        slower = verdict(campaign(arms, profile='tiered', values=per_arm(ultrasave={'credits': 4.0, 'wall': 140.0})))
        self.assertEqual(slower['claims']['ultrasave']['status'], 'PASS', slower['claims'])
        too_slow = verdict(campaign(arms, profile='tiered', values=per_arm(ultrasave={'credits': 4.0, 'wall': 160.0})))
        self.assertEqual(too_slow['claims']['ultrasave']['status'], 'no claim yet')
        self.assertIn({'gate': 'charged time <= 1.5x Normal', 'passed': False}, too_slow['claims']['ultrasave']['checks'])

    def test_ultrafast_may_spend_two_point_five_times_normal_credits_only_on_a_fast_provider_path(self):
        data = campaign(('normal', 'normal_aa', 'ultrafast'), values=per_arm(ultrafast={'wall': 50.0, 'credits': 20.0}))
        same_path = verdict(data)
        self.assertEqual(same_path['claims']['ultrafast']['status'], 'no claim yet')
        self.assertIn({'gate': 'credits <= 1.15x Normal', 'passed': False}, same_path['claims']['ultrafast']['checks'])
        data['ultrafast_provider_tier'] = 'fast'
        fast_path = verdict(data)
        self.assertEqual(fast_path['claims']['ultrafast']['status'], 'PASS', fast_path['claims'])
        self.assertIn({'gate': 'credits <= 2.5x Normal on the fast provider path', 'passed': True},
                      fast_path['claims']['ultrafast']['checks'])

    def ultracode_campaign(self, ultracode, normal_fails='a', aa_fails=(('c', 1), ('d', 1)), wall=900.0):
        """Normal fails `normal_fails` and the A/A arm also fails `aa_fails`, so accepted runs deviate by 2 in 12
        by default. A Normal run lasting the whole timeout keeps the A/A time ratio at 1 either way."""
        def values(arm, case, rep):
            if arm == 'ultracode':
                return ultracode(case, rep)
            return {'wall': wall,
                    'passed': not (case in normal_fails or (arm == 'normal_aa' and (case, rep) in aa_fails))}
        data = campaign(('normal', 'normal_aa', 'ultracode'), values=values)
        data['ultracode_cases'] = {'substantial': ['a', 'b'], 'control': ['c', 'd']}
        return data

    def test_ultracode_gain_must_clear_twice_the_aa_deviation_of_accepted_runs(self):
        clear = verdict(self.ultracode_campaign(lambda case, rep: {'credits': 20.0, 'wall': 200.0}))
        self.assertAlmostEqual(clear['aa_margins']['accepted'], 4 / 12)
        self.assertEqual(clear['claims']['ultracode']['status'], 'PASS', clear['claims'])
        # One substantial run more than Normal: 1/6 of the runs, inside the 4/12 margin.
        noise = verdict(self.ultracode_campaign(lambda case, rep: {
            'credits': 20.0, 'wall': 200.0, 'passed': not (case == 'a' and rep < 3)}))
        self.assertEqual(noise['claims']['ultracode']['status'], 'no claim yet')
        self.assertIn({'gate': 'substantial accepted gain beyond the A/A margin of accepted runs', 'passed': False},
                      noise['claims']['ultracode']['checks'])

    def test_ultracode_losses_stay_under_three_times_credits_and_two_point_five_times_time(self):
        dear = verdict(self.ultracode_campaign(lambda case, rep: {'credits': 35.0, 'wall': 200.0}))
        self.assertIn({'gate': 'substantial credits <= 3x Normal', 'passed': False}, dear['claims']['ultracode']['checks'])
        slow = verdict(self.ultracode_campaign(lambda case, rep: {'credits': 20.0, 'wall': 300.0},
                                               normal_fails='', aa_fails=(), wall=100.0))
        self.assertIn({'gate': 'substantial charged time <= 2.5x Normal', 'passed': False},
                      slow['claims']['ultracode']['checks'])
        undeclared = self.ultracode_campaign(lambda case, rep: {'credits': 20.0, 'wall': 200.0})
        del undeclared['ultracode_cases']
        report = verdict(undeclared)
        self.assertEqual(report['claims']['ultracode']['status'], 'no claim yet')
        self.assertIn({'gate': 'substantial credits <= 3x Normal', 'passed': None}, report['claims']['ultracode']['checks'])

    # A/A reads 1.05 on credits and time: the merge rule's deviation is 0.05 on both.
    NOISY_AA = {'credits': 10.5, 'wall': 105.0}

    def test_a_lean_mode_beaten_on_credits_and_time_is_reported_as_dominated(self):
        arms = ('normal', 'normal_aa', 'ultrasave', 'ultrafast')
        report = verdict(campaign(arms, profile='tiered', values=per_arm(
            normal_aa=self.NOISY_AA, ultrasave={'credits': 4.0, 'wall': 60.0}, ultrafast={'credits': 10.0, 'wall': 80.0})))
        self.assertEqual(report['merge'], ['MERGE: ultrafast is dominated by ultrasave'])
        self.assertEqual(report['claims']['ultrafast']['status'], 'no claim yet')
        self.assertEqual(report['claims']['ultrafast']['merge'], ['MERGE: ultrafast is dominated by ultrasave'])
        self.assertIn('MERGE: ultrafast is dominated by ultrasave', report['reasons'])
        # Slower by less than the A/A deviation: not dominated.
        close = verdict(campaign(arms, profile='tiered', values=per_arm(
            normal_aa=self.NOISY_AA, ultrasave={'credits': 4.0, 'wall': 60.0}, ultrafast={'credits': 10.0, 'wall': 62.0})))
        self.assertEqual(close['merge'], [])

    def test_modes_inside_the_aa_deviation_on_all_three_metrics_are_reported_as_duplicates(self):
        arms = ('normal', 'normal_aa', 'ultrasave', 'ultrafast', 'ultracode')
        report = verdict(campaign(arms, values=per_arm(
            normal_aa=self.NOISY_AA, ultracode={'credits': 10.3, 'wall': 103.0},
            ultrasave={'credits': 7.0, 'wall': 70.0}, ultrafast={'credits': 7.2, 'wall': 68.0})))
        self.assertEqual(sorted(report['merge']), ['MERGE: ultracode duplicates normal',
                                                   'MERGE: ultrafast duplicates ultrasave'])
        for mode in ('ultracode', 'ultrafast'):
            self.assertEqual(report['claims'][mode]['status'], 'no claim yet')
        self.assertNotIn('merge', report['claims']['ultrasave'])

    def test_distinct_modes_report_no_merge_and_their_signature(self):
        arms = ('normal', 'normal_aa', 'ultrasave', 'ultrafast')
        report = verdict(campaign(arms, profile='tiered', values=per_arm(
            normal_aa=self.NOISY_AA, ultrasave={'credits': 4.0, 'wall': 120.0}, ultrafast={'credits': 11.0, 'wall': 60.0})))
        self.assertEqual(report['merge'], [])
        self.assertEqual(report['verdict'], 'PASS', report['reasons'])
        self.assertEqual(set(report['mode_signature']), {'ultrasave', 'ultrafast'})
        # Synthetic rows carry no policy rows or per-call effort: nothing to check, never a pass.
        self.assertEqual(report['mode_signature']['ultrafast']['status'], 'NO DATA')

    def test_fewer_than_three_repetitions_is_a_smoke(self):
        report = verdict(campaign(('normal', 'normal_aa'), repetitions=1))
        self.assertEqual((report['verdict'], report['smoke_verdict']), ('SMOKE', 'PASS'))

    def test_missing_or_mislabelled_runs_are_incomplete(self):
        data = campaign(('normal', 'normal_aa', 'ultrafast'))
        data['runs'].pop()
        self.assertEqual(verdict(data)['verdict'], 'INCOMPLETE')
        data = campaign(('normal', 'normal_aa', 'ultrafast'))
        data['runs'][2]['settings']['execution_mode'] = 'normal'
        self.assertEqual(verdict(data)['verdict'], 'INCOMPLETE')


class NormalVersionsVerdictTest(unittest.TestCase):
    def versions(self, candidate):
        data = campaign(('baseline', 'candidate'), values=per_arm(candidate=candidate))
        data['compare_versions'] = True
        for item in data['runs']:
            item['settings']['binary_sha256'] = item['variant']
        return data

    def test_new_normal_inside_the_aa_band_passes_and_a_costlier_one_fails(self):
        self.assertEqual(normal_verdict(self.versions({'credits': 10.5}))['verdict'], 'PASS')
        report = normal_verdict(self.versions({'credits': 12.0}))
        self.assertEqual(report['verdict'], 'FAIL')
        self.assertTrue(any('credit ratio of totals' in reason for reason in report['reasons']))

    def test_normal_verdict_requires_compare_versions(self):
        data = self.versions({})
        data['compare_versions'] = False
        self.assertEqual(normal_verdict(data)['verdict'], 'INCOMPLETE')


if __name__ == '__main__':
    unittest.main()
