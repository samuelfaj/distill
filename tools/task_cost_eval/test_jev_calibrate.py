"""Synthetic events.jsonl files only; no real session data and no model calls."""
import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path

from . import jev_calibrate as jc

TS = '2026-01-10T10:00:00+00:00'


def decision(seq, lever, p, label='incomplete', ts=TS, memo=False, latency=400, model='jev-test',
             qhash='0123456789ab', **extra):
    return {'ts': ts, 'type': 'jev_decision', 'seq': seq, 'lever': lever, 'question_hash': qhash,
            'model_id': model, 'answers': {'q': {'type': 'noul', 'noul': p}}, 'label': label,
            'memo': memo, 'latency_ms': latency, **extra}


def no_answer(seq, lever, latency=4000):
    return {'ts': TS, 'type': 'jev_no_answer', 'seq': seq, 'lever': lever, 'kind': 'timeout',
            'latency_ms': latency}


def write_events(root, session, events, turns=1):
    directory = root / 'project' / session
    directory.mkdir(parents=True)
    lines = [{'ts': TS, 'type': 'turn_started', 'turn_number': n} for n in range(turns)] + events
    (directory / 'events.jsonl').write_text(''.join(json.dumps(line) + '\n' for line in lines))


def run(argv):
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        jc.main(argv)
    return out.getvalue()


class ReliabilityTests(unittest.TestCase):
    def test_a_lever_with_fewer_than_30_of_the_rarer_label_is_refused(self):
        # 40 true and 29 false: the rarer class is short of 30, so no threshold is proposed.
        points = [(0.9, True)] * 40 + [(0.2, False)] * 29
        proposal = jc.propose_threshold(points)
        self.assertTrue(proposal['refused'])
        self.assertEqual((proposal['positives'], proposal['negatives']), (40, 29))

    def test_a_separable_lever_gets_a_threshold_between_the_classes(self):
        # True outcomes sit at 0.8, false ones at 0.3: any cut in (0.3, 0.8] separates them,
        # and the first best one is proposed.
        points = [(0.8, True)] * 30 + [(0.3, False)] * 30
        proposal = jc.propose_threshold(points)
        self.assertFalse(proposal['refused'])
        self.assertTrue(0.3 < proposal['threshold'] <= 0.8)
        self.assertEqual(proposal['balanced_accuracy'], 1.0)

    def test_bins_report_observed_rate_against_mean_probability(self):
        rows = jc.reliability([(0.05, False), (0.08, True), (0.95, True), (1.0, True)], bins=10)
        self.assertEqual(rows[0][2], 2)
        self.assertAlmostEqual(rows[0][4], 0.5)
        self.assertEqual(rows[-1][2], 2, 'p = 1.0 falls in the top bin')

    def test_labels_join_by_seq_first_then_by_session(self):
        labels = {'decisions': {'s1': {'3': False}}, 'sessions': {'s1': True}}
        self.assertIs(jc.label_of(labels, 's1', {'seq': 3}), False)
        self.assertIs(jc.label_of(labels, 's1', {'seq': 4}), True)
        self.assertIsNone(jc.label_of(labels, 's2', {'seq': 3}))

    def test_probability_reads_the_named_question_or_the_confidence(self):
        event = {'confidence': 0.7, 'answers': {'a': {'type': 'noul', 'noul': 0.2},
                                                'm': {'type': 'choice', 'choice': 'normal',
                                                      'probabilities': {'normal': 0.6}}}}
        self.assertEqual(jc.probability(event), 0.7)
        self.assertEqual(jc.probability(event, 'a'), 0.2)
        self.assertEqual(jc.probability(event, 'm'), 0.6)
        self.assertIsNone(jc.probability({'answers': event['answers']}))

    def test_end_to_end_prints_a_proposal_with_its_provenance_and_shadow_apart(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            events = [decision(n, 'c1_premature_stop', 0.8 if n < 30 else 0.3) for n in range(60)]
            events.append(decision(60, 'c4_diff_risk', 0.9, label='shadow:c4:edge_case'))
            write_events(root, 'session-a', events)
            labels = root / 'labels.json'
            labels.write_text(json.dumps({'decisions': {'session-a': {str(n): n < 30 for n in range(61)}}}))
            text = run(['--sessions-root', str(root), '--labels', str(labels)])
        self.assertIn('PROPOSED', text)
        self.assertIn('fit window 2026-01-10..2026-01-10, n=60', text)
        self.assertIn('0123456789ab', text)
        self.assertIn('c4_diff_risk (shadow)', text)
        self.assertIn('REFUSED', text, 'one shadow decision is far from 30 per class')
        self.assertNotIn('session-a', text)


class ScorecardTests(unittest.TestCase):
    def test_counts_asked_answered_applied_and_awaited_time(self):
        events = [('s', decision(0, 'c4_diff_risk', 0.9, label='review:breaks', latency=500)),
                  ('s', decision(1, 'c4_diff_risk', 0.1, label='review:ok', latency=300)),
                  ('s', decision(2, 'c4_diff_risk', 0.1, label='review:ok', latency=0, memo=True)),
                  ('s', no_answer(3, 'c4_diff_risk', latency=1200)),
                  # Recorded without answers after the no-answer event: not a second ask.
                  ('s', {**decision(4, 'c4_diff_risk', 0.0, label='defer'), 'answers': {}}),
                  ('s', decision(5, 'c4_diff_risk', 0.6, label='shadow:c4:breaks'))]
        labels = {'decisions': {'s': {'0': True}}, 'sessions': {}}
        rows = jc.scorecard(events, labels, turns=2)
        live = rows['c4_diff_risk']
        self.assertEqual((live['asked'], live['answered'], live['applied']), (4, 3, 1))
        self.assertEqual(live['ms_per_turn'], (500 + 300 + 1200) / 2)
        self.assertEqual(live['precision'], 1.0)
        low, high = live['interval']
        self.assertLess(low, 1.0)
        self.assertEqual(high, 1.0)
        self.assertEqual(rows['c4_diff_risk (shadow)']['applied'], 0, 'shadow is never applied')

    def test_an_applied_field_wins_over_the_label(self):
        self.assertFalse(jc.applied({'label': 'compact', 'applied': 'shadow'}))
        self.assertTrue(jc.applied({'label': 'defer', 'applied': True}))
        self.assertTrue(jc.applied({'label': 'compact'}))
        self.assertFalse(jc.applied({'label': 'defer'}))

    def test_a_lever_that_never_acts_over_500_answers_is_dormant(self):
        # D4 shape: hundreds of answers, every one deferred.
        events = [('s', decision(n, 'd4_compaction_timing', 0.4, label='defer')) for n in range(500)]
        events.append(('s', decision(500, 'c1_premature_stop', 0.9, label='incomplete')))
        rows = jc.scorecard(events, {'decisions': {}, 'sessions': {}}, turns=10)
        self.assertTrue(rows['d4_compaction_timing']['dormant'])
        self.assertEqual(rows['d4_compaction_timing']['applied'], 0)
        self.assertFalse(rows['c1_premature_stop']['dormant'], 'too few answers to call it dormant')
        text = jc.format_scorecard(rows, 10)
        self.assertIn('DORMANT', text)
        self.assertIn('unlabelled', text)

    def test_excluded_days_are_left_out(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_events(root, 's', [decision(0, 'c4_diff_risk', 0.9, label='review:breaks'),
                                     decision(1, 'c4_diff_risk', 0.9, label='review:breaks',
                                              ts='2026-01-11T09:00:00+00:00')])
            events, turns = jc.load_events(root, jc.parse_day_ranges(['2026-01-11']))
            self.assertEqual(len(events), 1)
            self.assertEqual(turns, 1)
            text = run(['--sessions-root', str(root), '--report', 'scorecard'])
        self.assertIn('c4_diff_risk', text)
        self.assertIn('turns read: 1', text)


if __name__ == '__main__':
    unittest.main()
