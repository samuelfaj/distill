import unittest

from .mode_signature import FAIL, NO_DATA, PASS, arm_signature, run_signature


def main_call(effort, **extra):
    return {'agent': 'main', 'role': 'main', 'effort': effort, **extra}


def outcome(verification='unverified', reason='no_check', **policy):
    return {'verification': verification, 'verification_reason': reason, 'policy': policy}


def status(mode, check, row):
    return run_signature(mode, row)[check]['status']


class UltraSaveSignatureTest(unittest.TestCase):
    # The money mode keeps effort at or below its ceiling unless a turn escalated, and never
    # buys the one-hour cache write.
    def test_a_round_above_the_ceiling_fails_only_without_an_escalation(self):
        within = {'call_usage': [main_call('medium'), main_call('high')], 'escalations': 0}
        self.assertEqual(status('ultrasave', 'effort_ceiling', within), PASS)
        above = {'call_usage': [main_call('low'), main_call('xhigh')], 'escalations': 0}
        result = run_signature('ultrasave', above)['effort_ceiling']
        self.assertEqual((result['status'], result['above_ceiling']), (FAIL, 1))
        escalated = {**above, 'escalations': 1}
        self.assertEqual(status('ultrasave', 'effort_ceiling', escalated), PASS)

    def test_effort_is_no_data_without_known_efforts_or_an_escalation_count(self):
        self.assertEqual(status('ultrasave', 'effort_ceiling', {'call_usage': [main_call(None)]}), NO_DATA)
        # A child's or Jev's call is not a main round.
        children = {'call_usage': [{'agent': 'worker', 'role': 'main', 'effort': 'max'},
                                   {'agent': 'main', 'role': 'jev', 'effort': 'max'}], 'escalations': 0}
        self.assertEqual(status('ultrasave', 'effort_ceiling', children), NO_DATA)
        unknown = {'call_usage': [main_call('effort:max')]}
        self.assertEqual(status('ultrasave', 'effort_ceiling', unknown), NO_DATA)

    def test_one_hour_cache_writes_in_three_states(self):
        clean = {'call_usage': [main_call('low', cache_creation_1h_prompt_tokens=0),
                                main_call('low', usage={'cache_creation_1h_prompt_tokens': 0})]}
        self.assertEqual(status('ultrasave', 'no_1h_cache_writes', clean), PASS)
        dear = {'call_usage': [main_call('low', usage={'cache_creation_1h_prompt_tokens': 2048})]}
        result = run_signature('ultrasave', dear)['no_1h_cache_writes']
        self.assertEqual((result['status'], result['one_hour_write_tokens']), (FAIL, 2048))
        self.assertEqual(status('ultrasave', 'no_1h_cache_writes', {'call_usage': [main_call('low')]}), NO_DATA)


class UltraFastSignatureTest(unittest.TestCase):
    # The time mode never waits on Jev before a model request and never runs a utility
    # selection between a tool result and the next request.
    asked = {'decision_events_found': True, 'decision_events': {'b2_micro_effort': {'asked': 2}}}

    def test_every_turn_of_a_run_that_asked_jev_shows_the_skip(self):
        skipped = {**self.asked, 'turn_outcomes': [outcome(**{'ultrafast:jev_wait_skipped': 1})] * 2}
        self.assertEqual(status('ultrafast', 'jev_not_awaited', skipped), PASS)
        one_waited = {**self.asked, 'turn_outcomes': [outcome(**{'ultrafast:jev_wait_skipped': 1}), outcome()]}
        result = run_signature('ultrafast', one_waited)['jev_not_awaited']
        self.assertEqual((result['status'], result['turns'], result['turns_with_skip_row']), (FAIL, 2, 1))
        self.assertIn('not_checked', result)
        no_jev = {'decision_events_found': False, 'turn_outcomes': [outcome()]}
        self.assertEqual(status('ultrafast', 'jev_not_awaited', no_jev), NO_DATA)

    def test_a_selection_call_fails_and_a_skip_row_without_one_passes(self):
        calls = [main_call('low')]
        passing = {'call_usage': calls, 'policy_counts': {'ultrafast:utility_selection_skipped': 3}}
        self.assertEqual(status('ultrafast', 'no_utility_selection', passing), PASS)
        selected = {**passing, 'call_usage': [*calls, {'role': 'utility', 'task_id': 'select_units'}]}
        self.assertEqual(status('ultrafast', 'no_utility_selection', selected), FAIL)
        # A utility title is not a selection, but without a skip row nothing shows the rule acted.
        silent = {'call_usage': [*calls, {'role': 'utility', 'task_id': 'display_text'}], 'policy_counts': {}}
        self.assertEqual(status('ultrafast', 'no_utility_selection', silent), NO_DATA)


class UltraCodeSignatureTest(unittest.TestCase):
    # The care mode reviews every code-edit turn and never asks the pass that only lowers effort.
    def test_every_reviewed_turn_carries_a_verdict_row(self):
        reviewed = outcome('reviewed', None, **{'ultracode:review_verdict:reviewed': 1})
        refuted = outcome('unverified', 'review_unverified', **{'ultracode:review_verdict:unverified': 1})
        prose_only = outcome('unverified', 'no_check')
        row = {'turn_outcomes': [reviewed, refuted, prose_only]}
        result = run_signature('ultracode', row)['review_on_edit_turns']
        self.assertEqual((result['status'], result['edit_turns']), (PASS, 2))
        missing = {'turn_outcomes': [reviewed, outcome('reviewed', None)]}
        self.assertEqual(status('ultracode', 'review_on_edit_turns', missing), FAIL)
        self.assertEqual(status('ultracode', 'review_on_edit_turns', {'turn_outcomes': [prose_only]}), NO_DATA)

    def test_a_lowering_pass_question_fails(self):
        events = {'decision_events_found': True, 'decision_events': {'b2_micro_effort': {'asked': 3}}}
        self.assertEqual(status('ultracode', 'no_lowering_pass', events), PASS)
        lowered = {**events, 'decision_events': {'b2_model_tier': {'asked': 1}}}
        self.assertEqual(status('ultracode', 'no_lowering_pass', lowered), FAIL)
        self.assertEqual(status('ultracode', 'no_lowering_pass', {'decision_events_found': False}), NO_DATA)


class ArmSignatureTest(unittest.TestCase):
    def test_an_arm_fails_on_one_failing_run_and_passes_over_runs_without_data(self):
        good = {'decision_events_found': True, 'decision_events': {},
                'turn_outcomes': [outcome('reviewed', None, **{'ultracode:review_verdict:reviewed': 1})]}
        empty = {'decision_events_found': True, 'decision_events': {}, 'turn_outcomes': []}
        arm = arm_signature('ultracode', [good, empty])
        self.assertEqual(arm['status'], PASS)
        self.assertEqual(arm['checks']['review_on_edit_turns']['runs'], {PASS: 1, FAIL: 0, NO_DATA: 1})
        self.assertEqual(arm['checks']['review_on_edit_turns']['counts']['edit_turns'], 1)
        bad = {**good, 'decision_events': {'b2_model_tier': {'asked': 1}}}
        self.assertEqual(arm_signature('ultracode', [good, bad])['status'], FAIL)
        self.assertEqual(arm_signature('ultracode', [empty])['status'], NO_DATA)
        self.assertIsNone(arm_signature('normal', [good]))


if __name__ == '__main__':
    unittest.main()
