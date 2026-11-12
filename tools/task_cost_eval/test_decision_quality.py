"""Synthetic event streams and run rows only; no real session data and no model calls."""
import json
import re
import tempfile
import unittest
from pathlib import Path

from . import decision_quality as dq
from .test_session_usage import call, run_main, write_session

TS = '2026-01-10T10:00:00+00:00'
REPO = Path(__file__).resolve().parents[2]


def ev(event_type, **fields):
    return {'type': event_type, 'ts': TS, **fields}


def start(number=1, ts=TS):
    return {'type': 'turn_started', 'ts': ts, 'turn_number': number}


def loop(*tools):
    return [ev('loop_started'), *(ev('tool_started', tool_name=tool) for tool in tools)]


def decision(lever, label, seq=1, **extra):
    return ev('jev_decision', **{'seq': seq, 'lever': lever, 'label': label, 'latency_ms': 40, **extra})


def pick(label, **extra):
    return decision('b8_execution_mode', label, **extra)


def switch(old, new, trigger='review'):
    return ev('mode_switched', **{'from': old, 'to': new, 'trigger': trigger})


def outcome(failed=0, escalations=0, **extra):
    return ev('turn_outcome', mode_choice='auto', checks={'passed': 1, 'failed': failed, 'unknown': 0},
              escalations=escalations, **extra)


def flat(*parts):
    events = []
    for part in parts:
        events.extend(part if isinstance(part, list) else [part])
    return events


def turns_of(events, directory):
    path = Path(directory) / 'events.jsonl'
    path.write_text(''.join(json.dumps(event) + '\n' for event in events))
    return dq.read_turns(path)


def facts(events, initial_mode=None):
    with tempfile.TemporaryDirectory() as directory:
        return dq.outcome_facts(turns_of(events, directory), initial_mode)


class ModePickLabelTest(unittest.TestCase):
    def test_a_switch_away_within_three_rounds_abandons_the_pick(self):
        # The pick was wrong if the harness left it before it could pay off.
        got = facts(flat(start(), pick('ultrafast'), switch('normal', 'ultrafast', 'request_start'),
                         loop('read_file'), loop('read_file'), switch('ultrafast', 'normal'), loop(), ev('turn_ended')))
        self.assertEqual((got['mode.picks'], got['mode.abandoned'], got['mode.held']), (1, 1, 0))

    def test_a_switch_after_three_rounds_leaves_the_pick_held(self):
        got = facts(flat(start(), pick('ultrafast'), switch('normal', 'ultrafast', 'request_start'),
                         loop(), loop(), loop(), loop(), switch('ultrafast', 'normal'), loop(), ev('turn_ended')))
        self.assertEqual((got['mode.abandoned'], got['mode.held'], got['mode.labelled']), (0, 1, 1))

    def test_a_truncated_stream_leaves_a_recent_pick_unlabelled(self):
        # Fewer than three rounds seen and no turn end: neither held nor abandoned is known.
        got = facts(flat(start(), pick('normal'), loop()))
        self.assertEqual((got['mode.picks'], got['mode.labelled']), (1, 0))

    def test_keep_picks_the_current_mode(self):
        got = facts(flat(start(), pick('keep'), loop(), switch('ultrasave', 'normal'), ev('turn_ended')),
                    initial_mode='ultrasave')
        self.assertEqual(got['mode.abandoned'], 1)

    def test_lean_mode_ending_with_a_failed_check_or_escalation(self):
        for failed, escalations in ((1, 0), (0, 1)):
            with self.subTest(failed=failed, escalations=escalations):
                got = facts(flat(start(), pick('ultrasave'), switch('normal', 'ultrasave', 'request_start'),
                                 loop('bash'), outcome(failed, escalations), ev('turn_ended')))
                self.assertEqual((got['mode.lean_ended_failing'], got['mode.lean_ended_clean']), (1, 0))

    def test_lean_mode_ending_clean_or_a_turn_that_left_the_lean_mode(self):
        clean = facts(flat(start(), pick('ultrasave'), switch('normal', 'ultrasave', 'request_start'),
                           loop('bash'), outcome(), ev('turn_ended')))
        self.assertEqual((clean['mode.lean_ended_failing'], clean['mode.lean_ended_clean']), (0, 1))
        # Escalated to Normal before the end: the failure is not the lean pick's last word.
        left = facts(flat(start(), pick('ultrasave'), switch('normal', 'ultrasave', 'request_start'), loop(),
                          loop(), loop(), loop(), switch('ultrasave', 'normal'), outcome(failed=1), ev('turn_ended')))
        self.assertEqual((left['mode.lean_ended_failing'], left['mode.lean_ended_clean']), (0, 0))

    def test_a_return_to_a_mode_left_earlier_in_the_turn_is_a_flip_flop(self):
        got = facts(flat(start(), loop(), switch('normal', 'ultracode'), loop(), pick('normal'),
                         switch('ultracode', 'normal'), loop(), ev('turn_ended')))
        self.assertEqual((got['mode.flip_flop'], got['switch.flip_flop'], got['switch.mid_turn']), (1, 1, 2))

    def test_moving_on_to_a_new_mode_is_not_a_flip_flop(self):
        got = facts(flat(start(), switch('normal', 'ultrafast', 'request_start'), loop(), pick('ultracode'),
                         switch('ultrafast', 'ultracode'), loop(), ev('turn_ended')))
        self.assertEqual((got['mode.flip_flop'], got['switch.flip_flop'], got['switch.mid_turn']), (0, 0, 1))


def reminder_turn(followup, check_after=False):
    return flat(start(), loop('edit'), ev('final_check_reminder', seq=7, label='incomplete', worktree_hash='0' * 12),
                loop('bash'), *( [ev('check_recorded', outcome='passed', command_hash='1' * 12)] if check_after else []),
                *( [ev('final_check_followup', reminder_seq=7, **followup)] if followup is not None else []),
                ev('turn_ended'))


class FinalCheckLabelTest(unittest.TestCase):
    QUIET = {'tool_rounds': 1, 'edited': False, 'delivered': False, 'fixed_token': False, 'worktree_changed': False}

    def label(self, followup, check_after=False):
        got = facts(reminder_turn(followup, check_after))
        labels = [name for name in ('catch', 'false_alarm', 'unclear') if got[f'final_check.{name}']]
        return labels, got

    def test_catch_when_the_reminder_led_to_an_edit_a_change_or_a_delivery(self):
        for field in ('edited', 'worktree_changed', 'delivered'):
            with self.subTest(field=field):
                self.assertEqual(self.label({**self.QUIET, field: True})[0], ['catch'])

    def test_catch_when_tool_rounds_were_followed_by_a_recorded_check(self):
        self.assertEqual(self.label(self.QUIET, check_after=True)[0], ['catch'])
        self.assertEqual(self.label(self.QUIET)[0], ['unclear'], 'tool rounds alone prove nothing')

    def test_false_alarm_on_the_fixed_token_or_zero_tool_rounds(self):
        self.assertEqual(self.label({**self.QUIET, 'fixed_token': True})[0], ['false_alarm'])
        self.assertEqual(self.label({**self.QUIET, 'tool_rounds': 0})[0], ['false_alarm'])

    def test_a_reminder_without_a_follow_up_stays_unlabelled(self):
        labels, got = self.label(None)
        self.assertEqual((labels, got['final_check.reminders'], got['final_check.labelled']), ([], 1, 0))
        self.assertEqual(dq.coverage(got)['final_check'], 0.0)


class SkillC4EffortActedLabelTest(unittest.TestCase):
    def test_a_hint_is_followed_only_by_a_later_read_of_the_same_skill(self):
        followed = facts(flat(start(), ev('skill_suggested', name_hash='h:aaaaaaaaaaaa'), loop(),
                              ev('skill_read', name_hash='h:aaaaaaaaaaaa'), ev('turn_ended')))
        self.assertEqual((followed['skill.suggested'], followed['skill.followed']), (1, 1))
        self.assertEqual(followed['skill.unsuggested_read_turns'], 0)
        other = facts(flat(start(), ev('skill_read', name_hash='h:aaaaaaaaaaaa'),
                           ev('skill_suggested', name_hash='h:aaaaaaaaaaaa'), ev('skill_read', name_hash='h:bbbbbbbbbbbb'),
                           ev('turn_ended'), start(2), loop(), ev('turn_ended')))
        self.assertEqual((other['skill.suggested'], other['skill.followed']), (1, 0), 'read before the hint')
        # The base rate counts turns that read a skill nobody suggested: one of two turns here.
        self.assertEqual(dq.rates(other)['skill.base_rate'], 0.5)

    def test_c4_note_followed_by_an_edit_in_the_next_two_rounds(self):
        followed = facts(flat(start(), loop('edit'), decision('c4_diff_risk', 'review:mismatch'), loop('read_file'),
                              loop('search_replace'), loop(), ev('turn_ended')))
        self.assertEqual((followed['c4.note_then_edit'], followed['c4.note_no_edit']), (1, 0))
        late = facts(flat(start(), loop('edit'), decision('c4_diff_risk', 'review:breaks'), loop(), loop(),
                          loop('apply_patch'), ev('turn_ended')))
        self.assertEqual((late['c4.note_then_edit'], late['c4.note_no_edit']), (0, 1))
        base = facts(flat(start(), loop('edit'), decision('c4_diff_risk', 'review:ok'), loop('edit'), ev('turn_ended')))
        self.assertEqual((base['c4.ok_then_edit'], base['c4.verdicts'], base['c4.labelled']), (1, 1, 1))
        unseen = facts(flat(start(), loop('edit'), decision('c4_diff_risk', 'review:ok'), loop()))
        self.assertEqual((unseen['c4.verdicts'], unseen['c4.labelled']), (1, 0), 'stream cut before two rounds')
        skipped = facts(flat(start(), loop('edit'), decision('c4_diff_risk', 'review:skip-prose'), ev('turn_ended')))
        self.assertEqual(skipped['c4.verdicts'], 0, 'no verdict, nothing to label')

    def test_d4_and_p2_acted_counts_show_a_dormant_lever_as_zero(self):
        got = facts(flat(start(), decision('d4_compaction_timing', 'compact'), decision('d4_compaction_timing', 'defer'),
                         ev('turn_ended')))
        self.assertEqual((got['d4_compaction_timing.acted'], got['d4_compaction_timing.decisions']), (1, 2))
        self.assertEqual(got['p2_read_shortlist.acted'], 0)
        self.assertIn('p2_read_shortlist.acted', got)
        self.assertIsNone(dq.rates(got)['p2_read_shortlist.acted_rate'])

    def test_effort_is_labelled_only_when_a_raise_follows_a_low_or_capped_choice(self):
        raised = facts(flat(start(), decision('b2_micro_effort', 'effort:low'), loop(),
                            ev('effort_floor_raised', level='high', trigger='c4'), ev('turn_ended')))
        self.assertEqual((raised['effort.low_or_capped'], raised['effort.escalated_after_low']), (1, 1))
        before = facts(flat(start(), ev('effort_floor_raised', level='high', trigger='c4'),
                            decision('b2_micro_effort', 'effort:low'), ev('turn_ended')))
        self.assertEqual((before['effort.low_or_capped'], before['effort.escalated_after_low']), (1, 0))
        high = facts(flat(start(), decision('b2_micro_effort', 'effort:high'), ev('effort_floor_raised', level='xhigh'),
                          ev('turn_ended')))
        self.assertEqual(high['effort.low_or_capped'], 0)
        capped = facts(flat(start(), ev('mode_policy', seq=2, mode='ultrasave', rule='effort_ceiling'), loop(),
                            outcome(escalations=1), ev('turn_ended')))
        self.assertEqual((capped['effort.low_or_capped'], capped['effort.escalated_after_low']), (1, 1))


class LeverStatsTest(unittest.TestCase):
    def test_asked_answered_no_answer_memo_latency_and_safe_labels(self):
        events = flat(start(), decision('b8_execution_mode', 'normal', memo=True, latency_ms=10),
                      decision('b8_execution_mode', 'ultrafast', memo=False, latency_ms=30,
                               answers={'mode': {'type': 'choice', 'choice': 'ultrafast'}}),
                      decision('b8_execution_mode', 'src/secret/path.rs', latency_ms=50),
                      ev('jev_no_answer', seq=4, lever='b8_execution_mode', kind='timeout', latency_ms=800),
                      ev('jev_decision', lever='a6_test_to_run'),  # no label, no latency: skipped fields only
                      ev('some_future_event', lever='x'), ev('turn_ended'))
        with tempfile.TemporaryDirectory() as directory:
            report = dq.lever_report(dq.lever_stats(turns_of(events, directory)))
        mode = report['b8_execution_mode']
        self.assertEqual((mode['asked'], mode['answered'], mode['with_answers']), (4, 3, 1))
        self.assertEqual(mode['no_answer'], {'timeout': 1})
        self.assertEqual(mode['memo_share'], 0.5, 'over decisions that carry the memo flag')
        self.assertEqual(mode['labels'], {'normal': 1, 'ultrafast': 1, 'other': 1}, 'a path never prints')
        self.assertEqual(mode['latency_ms'], {'n': 3, 'median': 30, 'p90': 50})
        self.assertIsNone(mode['applied'], 'no applied field recorded: unknown, not zero')
        self.assertEqual(report['a6_test_to_run']['labels'], {'other': 1})

    def test_labels_beyond_the_top_twelve_are_summed(self):
        events = flat(start(), *(decision('p6_skill_suggestion', f'label{i:02d}') for i in range(15)), ev('turn_ended'))
        with tempfile.TemporaryDirectory() as directory:
            labels = dq.lever_report(dq.lever_stats(turns_of(events, directory)))['p6_skill_suggestion']['labels']
        self.assertEqual((len(labels), labels['(rest)']), (13, 3))

    def test_safe_label_accepts_identifiers_and_hashes_only(self):
        for value, expected in (('review:ok', 'review:ok'), ('h:0123456789ab', 'h:0123456789ab'),
                                ('tests/a.rs::case', 'other'), ('vendor/model-x', 'other'), ('a b', 'other'),
                                (None, 'other'), ('x' * 41, 'other')):
            self.assertEqual(dq.safe_label(value), expected, value)


def decision_session(root, sid, events, outcomes=None):
    write_session(root, sid, [call(f'{sid}-a')], events=events, outcomes=outcomes)


class DecisionsReportTest(unittest.TestCase):
    def test_sessions_without_decision_events_are_excluded_not_zero(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            decision_session(root, 'session-with-0001', flat(start(), pick('ultrafast'), loop(), loop(), loop(), loop(),
                                                             ev('turn_ended')),
                             outcomes={'jev:b8_execution_mode': {'decisions': {'ultrafast': 1, 'no_answer:timeout': 2}},
                                       'policy:ultrasave': {'decisions': {'effort_ceiling': 3}}})
            decision_session(root, 'session-old-0001', flat(start(), loop('edit'), ev('turn_ended')))
            write_session(root, 'session-none-0001', [call('n-a')])
            code, output, text = run_main(['--sessions-root', str(root), '--report', 'decisions'])
        self.assertEqual(code, 0)
        report = output['decisions']
        self.assertEqual(report['sessions']['with_decision_events'], 1)
        self.assertEqual(report['sessions']['no_decision_events'], 2)
        self.assertEqual(report['outcomes']['mode.picks'], 1)
        self.assertEqual(report['rates']['mode.held_rate'], 1.0, 'the excluded sessions add no zero')
        self.assertEqual(report['usage_counters']['levers']['b8_execution_mode'],
                         {'ultrafast': 1, 'no_answer:timeout': 2})
        self.assertEqual(report['usage_counters']['policy'], {'ultrasave': {'effort_ceiling': 3}})
        self.assertEqual(report['coverage']['mode'], 1.0)
        self.assertNotIn('session-with-0001', text)

    def test_excluded_days_drop_turns_by_their_start(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            decision_session(root, 'session-days-0001', flat(start(1, '2026-01-05T10:00:00+00:00'), pick('normal'),
                                                             ev('turn_ended'), start(2), pick('ultrafast'),
                                                             ev('turn_ended')))
            _, output, _ = run_main(['--sessions-root', str(root), '--report', 'decisions',
                                     '--exclude-days', '2026-01-04:2026-01-06'])
        self.assertEqual(output['decisions']['levers']['b8_execution_mode']['labels'], {'ultrafast': 1})

    def test_expect_prints_reference_rates_and_flags_gaps_over_three_points(self):
        events = flat(start(), loop(), switch('normal', 'ultracode'), switch('ultracode', 'normal'),
                      switch('normal', 'ultrafast'), switch('ultrafast', 'normal'), pick('normal'), ev('turn_ended'))
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            decision_session(root, 'session-expect-0001', events)
            _, output, _ = run_main(['--sessions-root', str(root), '--report', 'decisions', '--expect',
                                     'mode.flip_flop_per_switch=0.27,mode.held_rate=100%', '--expect',
                                     'final_check.catch_rate=0.074'])
        expect = output['decisions']['expect']
        # Two of four switches return to a mode left earlier: 0.5 against 0.27 is a 23-point gap.
        self.assertEqual(output['decisions']['rates']['mode.flip_flop_per_switch'], 0.5)
        self.assertTrue(expect['mode.flip_flop_per_switch']['flagged'])
        self.assertAlmostEqual(expect['mode.flip_flop_per_switch']['difference'], 0.23)
        self.assertEqual(expect['mode.held_rate'], {'expected': 1.0, 'measured': 1.0, 'difference': 0.0,
                                                    'flagged': False})
        self.assertTrue(expect['final_check.catch_rate']['flagged'], 'nothing measured is a gap too')
        self.assertIsNone(expect['final_check.catch_rate']['measured'])

    def test_expect_within_three_points_is_not_flagged(self):
        self.assertFalse(dq.compare_expected({'x': 0.10}, {'x': 0.12})['x']['flagged'])
        self.assertTrue(dq.compare_expected({'x': 0.10}, {'x': 0.14})['x']['flagged'])


def run_home(root, events, session='runtime-session'):
    """An isolated run home as compare_parallel leaves it: sessions/<project>/<session>/events.jsonl."""
    folder = Path(root) / 'distill-home' / 'sessions' / 'project' / session
    folder.mkdir(parents=True)
    (folder / 'events.jsonl').write_text(''.join(json.dumps(event) + '\n' for event in events))
    (folder.parent / 'child-session').mkdir()
    (folder.parent / 'child-session' / 'events.jsonl').write_text(
        json.dumps(pick('ultracode')) + '\n')
    return Path(root) / 'distill-home'


AUTO_RUN = flat(start(), pick('ultrasave', applied=True), switch('normal', 'ultrasave', 'request_start'),
                loop('read_file'), ev('mode_policy', seq=2, mode='ultrasave', rule='effort_ceiling'),
                ev('jev_no_answer', seq=3, lever='c4_diff_risk', kind='timeout', latency_ms=900),
                ev('check_recorded', outcome='failed', command_hash='2' * 12),
                outcome(failed=1, escalations=0, policy={'ultrasave:effort_ceiling': 1}), ev('turn_ended'),
                start(2), pick('normal', applied=False), loop(),
                outcome(failed=0, escalations=2, policy={'ultrasave:effort_ceiling': 2}), ev('turn_ended'))


class RunRecordTest(unittest.TestCase):
    def test_run_row_carries_turn_outcomes_policy_counts_decisions_and_verdict_fields(self):
        with tempfile.TemporaryDirectory() as directory:
            home = run_home(directory, AUTO_RUN)
            record = dq.run_record(dq.session_events_path(home, 'runtime-session'), initial_mode='normal')
        self.assertTrue(record['decision_events_found'])
        self.assertEqual([item['escalations'] for item in record['turn_outcomes']], [0, 2])
        self.assertNotIn('type', record['turn_outcomes'][0])
        self.assertEqual(record['policy_counts'], {'ultrasave:effort_ceiling': 3}, 'summed from the turn receipts')
        self.assertEqual(record['decision_events']['b8_execution_mode'],
                         {'asked': 2, 'answered': 2, 'no_answer': {}, 'applied': {'ultrasave': 1},
                          'labels': {'ultrasave': 1, 'normal': 1}})
        self.assertEqual(record['decision_events']['c4_diff_risk']['no_answer'], {'timeout': 1})
        self.assertEqual((record['escalations'], record['failed_check_facts']), (2, 1))
        self.assertEqual(record['decision_labels']['mode.lean_ended_failing'], 1)
        self.assertEqual(record['first_mode_pick'], 'ultrasave', 'the root session, not the child')

    def test_without_outcome_records_the_optional_verdict_fields_stay_absent(self):
        # Absent means unknown to the verdicts; a zero would claim "no escalation".
        with tempfile.TemporaryDirectory() as directory:
            home = run_home(directory, flat(start(), pick('normal'), loop(), ev('turn_ended')))
            record = dq.run_record(dq.session_events_path(home, 'runtime-session'))
            self.assertNotIn('escalations', record)
            self.assertNotIn('failed_check_facts', record)
            home = run_home(Path(directory) / 'two', flat(start(), ev('check_recorded', outcome='passed'),
                                                          ev('turn_ended')))
            record = dq.run_record(dq.session_events_path(home, 'runtime-session'))
            self.assertEqual(record['failed_check_facts'], 0, 'recorded checks, none failed')
            self.assertNotIn('escalations', record)
            self.assertFalse(record['decision_events_found'])

    def test_missing_session_folder_gives_an_empty_record(self):
        record = dq.run_record(None)
        self.assertEqual((record['decision_events_found'], record['turn_outcomes'], record['decision_events'],
                          record['first_mode_pick']), (False, [], {}, None))
        policy_only = flat(start(), ev('mode_policy', seq=1, mode='ultrafast', rule='no_wait'), ev('turn_ended'))
        with tempfile.TemporaryDirectory() as directory:
            record = dq.run_record(turns_path(directory, policy_only))
        self.assertEqual(record['policy_counts'], {'ultrafast:no_wait': 1}, 'policy events without receipts')

    def test_first_pick_prefers_the_request_start_switch_then_the_first_decision(self):
        cases = ((flat(start(), pick('normal'), switch('normal', 'ultrafast', 'request_start'), ev('turn_ended')),
                  None, 'ultrafast'),
                 (flat(start(), pick('keep'), ev('turn_ended'), start(2), switch('normal', 'ultracode', 'request_start'),
                       ev('turn_ended')), 'normal', 'normal'),
                 (flat(start(), loop(), ev('turn_ended'), start(2), pick('ultrasave'), ev('turn_ended')), None,
                  'ultrasave'),
                 (flat(start(), decision('b8_execution_mode', 'defer'), ev('turn_ended')), None, None))
        for events, initial, expected in cases:
            with self.subTest(expected=expected), tempfile.TemporaryDirectory() as directory:
                self.assertEqual(dq.first_mode_pick(turns_of(events, directory), initial), expected)


def turns_path(directory, events):
    path = Path(directory) / 'events.jsonl'
    path.write_text(''.join(json.dumps(event) + '\n' for event in events))
    return path


def bench_row(passed, labels, levers=None, found=True):
    return {'variant': 'auto', 'grader': {'exit_code': 0 if passed else 1}, 'passed': passed,
            'decision_events_found': found, 'decision_labels': labels, 'decision_events': levers or {}}


class QualityBlockTest(unittest.TestCase):
    def test_block_sums_levers_and_outcome_labels_per_group(self):
        lever = {'asked': 3, 'answered': 2, 'no_answer': {'error': 1}, 'applied': {'normal': 1},
                 'labels': {'normal': 1, 'keep': 1}}
        unknown = {'asked': 1, 'answered': 1, 'no_answer': {}, 'applied': None, 'labels': {'ultrafast': 1}}
        rows = [bench_row(True, {'mode.picks': 2, 'mode.held': 2, 'mode.labelled': 2}, {'b8_execution_mode': lever}),
                bench_row(False, {'mode.picks': 1, 'mode.abandoned': 1, 'mode.labelled': 1},
                          {'b8_execution_mode': unknown}),
                {'variant': 'auto', 'passed': True}]
        block = dq.quality_block(rows)
        self.assertEqual((block['runs'], block['runs_with_decision_events']), (2, 2), 'rows without records skipped')
        self.assertEqual(block['levers']['b8_execution_mode'],
                         {'asked': 4, 'answered': 3, 'applied': {'normal': 1},
                          'labels': {'normal': 1, 'keep': 1, 'ultrafast': 1}})
        self.assertAlmostEqual(block['rates']['mode.abandoned_rate'], 1 / 3)
        self.assertEqual(block['coverage']['mode'], 1.0)

    def test_label_agreement_with_the_hidden_grader_discards_a_label_under_point_eight(self):
        # lean_ended_failing claims a bad end: 4 of 5 labelled runs failed the grader (0.8, kept);
        # false_alarm claims a good end: 3 of 4 passed (0.75, discarded but still reported).
        rows = ([bench_row(False, {'mode.lean_ended_failing': 1})] * 4 + [bench_row(True, {'mode.lean_ended_failing': 1})]
                + [bench_row(True, {'final_check.false_alarm': 1})] * 3
                + [bench_row(False, {'final_check.false_alarm': 1})])
        agreement = dq.quality_block(rows)['label_agreement']
        self.assertEqual(agreement['mode.lean_ended_failing'],
                         {'claim': 'bad', 'labelled': 5, 'agreed': 4, 'agreement': 0.8, 'discarded': False})
        self.assertEqual(agreement['final_check.false_alarm'],
                         {'claim': 'good', 'labelled': 4, 'agreed': 3, 'agreement': 0.75, 'discarded': True})
        self.assertEqual(agreement['mode.lean_ended_clean'],
                         {'claim': 'good', 'labelled': 0, 'agreed': 0, 'agreement': None, 'discarded': False})


class DecisionGateTest(unittest.TestCase):
    @staticmethod
    def arm(abandoned, picks=10, found=True):
        return [bench_row(True, {'mode.picks': picks, 'mode.abandoned': abandoned}, found=found)]

    def test_pass_inside_the_aa_deviation(self):
        # Control 0.2, A/A 0.3: candidate within 0.1 of the control passes.
        gate = dq.decision_gate(self.arm(3), self.arm(2), self.arm(3))
        self.assertEqual(gate['status'], 'PASS')
        self.assertEqual(set(gate['metrics']), {'mode.abandoned_rate', 'mode.held_rate', 'mode.flip_flop_rate',
                                                'mode.lean_ended_failing_rate'})

    def test_fail_outside_the_aa_deviation(self):
        gate = dq.decision_gate(self.arm(4), self.arm(2), self.arm(3))
        self.assertEqual(gate['status'], 'FAIL')
        self.assertFalse(gate['metrics']['mode.abandoned_rate']['within'])

    def test_no_data_without_events_in_any_arm_or_without_an_aa_arm(self):
        for candidate, aa in ((self.arm(3, found=False), self.arm(3)), (self.arm(3), []),
                              (self.arm(0, picks=0), self.arm(0, picks=0))):
            with self.subTest(aa=aa):
                gate = dq.decision_gate(candidate, self.arm(2), aa, required=True)
                self.assertEqual((gate['status'], gate['metrics'], gate['required']), ('NO DATA', {}, True))


class LadderKeysTest(unittest.TestCase):
    def test_ladder_keys_match_the_rust_overlay(self):
        source = (REPO / 'crates/codegen/distill-workspace/src/jev/flags.rs').read_text()
        body = re.search(r'pub struct JevLadderOverlay \{(.*?)\n\}', source, re.S).group(1)
        self.assertEqual(sorted(dq.LADDER_KEYS), sorted(re.findall(r'pub (\w+): Option<bool>', body)))

    def test_jev_off_names_resolve_to_ladder_keys(self):
        self.assertEqual(dq.jev_off_levers('c4_diff_risk'), ['c4_diff_risk'])
        self.assertEqual(dq.jev_off_levers('final_check'), ['c1_premature_stop', 'c3_completion_check'])
        self.assertIsNone(dq.jev_off_levers('c4'))


if __name__ == '__main__':
    unittest.main()
