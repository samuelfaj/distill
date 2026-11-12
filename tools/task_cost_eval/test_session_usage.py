"""Synthetic session directories only; no real session data and no model calls."""
import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path

from . import session_usage as su

OPUS = 'vendor/claude-opus-x'
WORKER = 'vendor/small-worker'
TS = '2026-01-10T10:00:00+00:00'


def call(attempt, role='main', model=OPUS, prompt=1000, cached=0, write=0, write_1h=0, output=100, turn='t1',
         duration=1000, effort='high', label=None, status='completed'):
    usage = {'prompt_tokens': prompt, 'cached_prompt_tokens': cached, 'cache_creation_prompt_tokens': write,
             'completion_tokens': output, 'reasoning_tokens': 0, 'total_tokens': prompt + output}
    if write_1h:
        usage['cache_creation_1h_prompt_tokens'] = write_1h
    return {'attempt_id': attempt, 'task_id': None, 'turn_id': turn, 'request_id': None, 'role': role,
            'model_id': model, 'endpoint': 'https://example.invalid', 'requested_effort': effort,
            'applied_effort': effort, 'status': status, 'usage': usage, 'usage_complete': True,
            'api_duration_ms': duration, 'cost_usd_ticks': None, 'cost_basis': 'unknown', 'source_kind': label}


def write_session(root, sid, rows, *, project='project', turns=None, events=None, children=(), primary=OPUS,
                  primary_input=None, counters=None, outcomes=None, updated=TS):
    directory = root / project / sid
    directory.mkdir(parents=True)
    by_model = {}
    for row in rows:
        by_model.setdefault(row['model_id'], 0)
        by_model[row['model_id']] += row['usage']['prompt_tokens']
    if primary_input is not None:
        by_model[primary] = primary_input
    session = {'inputTokens': sum(row['usage']['prompt_tokens'] for row in rows), 'primaryModelId': primary,
               'modelUsage': {model: {'inputTokens': tokens} for model, tokens in by_model.items()},
               'attributions': rows, 'utilityOutcomes': outcomes or {}}
    if counters is not None:
        session['cacheBreaks'] = counters
    if turns is None:
        turns = [{'turnNumber': index + 1, 'endedAt': TS,
                  'attributions': [row for row in rows if row['turn_id'] == turn]}
                 for index, turn in enumerate(dict.fromkeys(row['turn_id'] for row in rows))]
    (directory / 'usage.json').write_text(json.dumps({'sessionId': sid, 'updatedAt': updated,
                                                      'session': session, 'turns': turns}))
    if events is not None:
        (directory / 'events.jsonl').write_text(''.join(json.dumps(event) + '\n' for event in events))
    for index, child in enumerate(children):
        meta = directory / 'subagents' / f'agent-{index}' / 'meta.json'
        meta.parent.mkdir(parents=True)
        meta.write_text(json.dumps({'child_session_id': child, 'parent_session_id': sid}))
    return directory


def analyse(root, *names, days=None):
    prices = su.prices_from(su.argparse.Namespace(prices=None, price=None))
    sessions, parents = su.load(root, prices, days=days)
    threads, counts = su.dedupe(sessions, parents, su.parse_day_ranges(names))
    return prices, sessions, threads, counts


def run_main(argv):
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        code = su.main(argv)
    return code, json.loads(out.getvalue()), out.getvalue()


def root_with_child(root, *, link=True):
    child_rows = [call('c1', model=WORKER, prompt=500, turn='ct'), call('c2', model=WORKER, prompt=700, turn='ct')]
    own = [call('r1', prompt=1000), call('r2', prompt=1200, cached=900)]
    write_session(root, 'root-session-0001', own + child_rows, children=['child-session-0001'] if link else ())
    write_session(root, 'child-session-0001', child_rows, primary=WORKER)


class DedupeTest(unittest.TestCase):
    def test_copied_child_rows_count_once_and_belong_to_the_subagent_thread(self):
        for link in (True, False):
            with self.subTest(meta_link=link), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                root_with_child(root, link=link)
                prices, _, threads, counts = analyse(root)
                report = su.report_totals(threads, counts, prices)
                self.assertEqual((report['rows_read'], report['calls'], report['duplicate_rows_removed']), (6, 4, 2))
                self.assertEqual((report['root_sessions'], report['subagent_sessions']), (1, 1))
                self.assertEqual(report['by_thread']['root']['prompt_tokens'], 2200)
                self.assertEqual(report['by_thread']['subagent']['prompt_tokens'], 1200)
                self.assertEqual(report['total']['prompt_tokens'], 3400, 'not 4600 with the copies')

    def test_excluded_days_drop_rows_by_their_turn_date(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rows = [call('a', turn='t1'), call('b', turn='t2')]
            turns = [{'turnNumber': 1, 'endedAt': '2026-01-05T12:00:00+00:00', 'attributions': [rows[0]]},
                     {'turnNumber': 2, 'endedAt': '2026-01-07T12:00:00+00:00', 'attributions': [rows[1]]}]
            write_session(root, 'session-days-0001', rows, turns=turns)
            prices, _, threads, counts = analyse(root, '2026-01-04:2026-01-05')
            self.assertEqual(counts['excluded_day_rows'], 1)
            self.assertEqual([c.attempt for t in threads.values() for c in t['calls']], ['b'])


    def test_days_keeps_only_sessions_updated_inside_the_window(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_session(root, 'session-new-0001', [call('a')])
            write_session(root, 'session-old-0001', [call('b')], updated='2025-12-01T10:00:00+00:00')
            prices = su.prices_from(su.argparse.Namespace(prices=None, price=None))
            sessions, _ = su.load(root, prices, days=7, now=su.datetime(2026, 1, 12, tzinfo=su.timezone.utc))
            self.assertEqual([call.attempt for session in sessions for call in session['calls']], ['a'])


class CheckTotalsTest(unittest.TestCase):
    def test_deduped_primary_prompt_tokens_match_model_usage_within_ten_percent(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            root_with_child(root)
            code, output, _ = run_main(['--sessions-root', str(root), '--check-totals'])
            self.assertEqual(code, 0)
            check = output['check-totals']
            self.assertEqual((check['sessions_checked'], check['within'], check['outside']), (2, 2, 0))
            self.assertAlmostEqual(check['deduped_prompt_tokens_over_root_session_totals'], 3400 / 3400)

    def test_a_ledger_that_disagrees_by_more_than_ten_percent_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_session(root, 'session-off-0001', [call('a', prompt=1000)], primary_input=1200)
            code, output, _ = run_main(['--sessions-root', str(root), '--check-totals'])
            self.assertEqual(code, 1)
            self.assertEqual(output['check-totals']['outside'], 1)


class PriceTableTest(unittest.TestCase):
    ROW = dict(prompt=1000, cached=600, write=300, write_1h=100, output=50)

    def price(self, model, *argv):
        prices = su.prices_from(su.argparse.Namespace(prices=None, price=list(argv) or None))
        loaded = su.Call(attempt='a', role='main', family=su.family_of(model, prices), model=model, turn='t',
                         duration_ms=None, effort=None, status=None, label=None, mode=None, day=None, **self.ROW)
        return su.cost(loaded, prices)

    def test_cache_reads_writes_and_output_follow_the_family_table(self):
        # 100 uncached + 600 read + 200 written for 5 min + 100 for 1 h + 50 output.
        self.assertAlmostEqual(self.price('claude-opus-x'), 100 + 600 * .05 + 200 * 1.25 + 100 * 2 + 50 * 5)
        self.assertAlmostEqual(self.price('claude-fable-x'), 100 + 600 * .025 + 200 * 1.25 + 100 * 2 + 50 * 5)
        self.assertAlmostEqual(self.price('other-model'), 100 + 600 * .1 + 200 * 1.25 + 100 * 2 + 50 * 5)

    def test_prices_are_configurable_by_argument_and_file(self):
        self.assertAlmostEqual(self.price('other-model', 'default.output=4'), 100 + 60 + 250 + 200 + 50 * 4)
        with tempfile.NamedTemporaryFile('w', suffix='.json') as handle:
            json.dump({'house': {'match': ['house'], 'cache_read': 0.2}}, handle)
            handle.flush()
            prices = su.prices_from(su.argparse.Namespace(prices=handle.name, price=None))
        self.assertEqual(su.family_of('house-model', prices), 'house')
        self.assertEqual(prices['house']['cache_read'], 0.2)
        self.assertEqual(prices['house']['output'], 5.0, 'unspecified fields keep the default')

    def test_cache_missing_calls_are_priced_separately(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_session(root, 'session-miss-0001', [call('a', prompt=1000), call('b', prompt=1200, cached=1000),
                                                      call('c', prompt=1300, cached=100)])
            prices, _, threads, counts = analyse(root)
            report = su.report_totals(threads, counts, prices)
            missed = report['cache_miss_calls']
            self.assertEqual((missed['calls'], missed['prompt_tokens']), (1, 1300))
            self.assertAlmostEqual(missed['cost_eq'], 1200 + 100 * .05 + 100 * 5)


class ReportTest(unittest.TestCase):
    def test_cache_breaks_by_heuristic_cause_and_by_counters(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rows = [call('a', turn='t1'), call('b', turn='t1', cached=900),          # hit
                    call('c', turn='t1', cached=0, effort='low'),                     # effort change
                    call('d', turn='t1', cached=0, effort='low', write_1h=500),       # lifetime switch
                    call('e', turn='t1', cached=0, effort='low', write_1h=500),       # mid-turn unknown
                    call('f', turn='t2', cached=0, effort='low', write_1h=500),       # gap over 5 min
                    call('g', turn='t3', cached=0, effort='low', write_1h=500)]       # turn start, short gap
            turns = [{'turnNumber': n, 'endedAt': f'2026-01-10T10:0{n}:00+00:00',
                      'attributions': [r for r in rows if r['turn_id'] == f't{n}']} for n in (1, 2, 3)]
            events = [{'type': 'turn_started', 'turn_number': 2, 'ts': '2026-01-10T10:20:00.000Z'},
                      {'type': 'turn_started', 'turn_number': 3, 'ts': '2026-01-10T10:02:30.000Z'}]
            write_session(root, 'session-break-0001', rows, turns=turns, events=events,
                          counters={'tools_changed': 2, 'lifetime_changed': {'count': 1, 'uncached_tokens': 900}})
            prices, _, threads, _ = analyse(root)
            report = su.report_cache_breaks(threads, prices)
            heuristic = report['heuristic']
            self.assertEqual((heuristic['eligible_calls'], heuristic['breaks']), (6, 5))
            self.assertEqual({cause: entry['breaks'] for cause, entry in heuristic['by_cause'].items()},
                             {'model_or_effort_change': 1, 'lifetime_5m_to_1h': 1, 'mid_turn_unknown': 1,
                              'gap_over_5m': 1, 'turn_start': 1})
            self.assertEqual(report['counters']['by_cause']['lifetime_changed'], {'count': 1, 'uncached_tokens': 900})
            self.assertEqual(report['counters']['by_cause']['tools_changed']['count'], 2)

    def test_rounds_per_turn_and_poll_only_rounds_from_events(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rows = [call('a', turn='t1'), call('b', turn='t1'), call('c', turn='t1'), call('d', turn='t2')]
            # Phase lines dominate real files; they are skipped without parsing.
            events = [{'type': 'turn_started', 'turn_number': 1, 'ts': TS},
                      {'type': 'loop_started', 'loop_index': 0, 'ts': TS},
                      {'type': 'phase_changed', 'phase': 'waiting_for_model', 'ts': TS},
                      {'type': 'tool_started', 'tool_name': 'read_file', 'ts': TS},
                      {'type': 'loop_started', 'loop_index': 1, 'ts': TS},
                      {'type': 'tool_started', 'tool_name': 'get_command_or_subagent_output', 'ts': TS},
                      {'type': 'loop_started', 'loop_index': 2, 'ts': TS},
                      {'type': 'turn_ended', 'outcome': 'completed', 'ts': TS}]
            write_session(root, 'session-rounds-0001', rows, events=events)
            _, _, threads, _ = analyse(root)
            report = su.report_rounds(threads)
            self.assertEqual(report['main_calls_per_turn']['root'], {'n': 2, 'mean': 2.0, 'median': 2.0, 'p90': 3})
            self.assertEqual(report['event_rounds']['root']['rounds'], 3)
            self.assertEqual(report['event_rounds']['root']['tool_rounds'], 2)
            self.assertEqual(report['event_rounds']['root']['poll_only_rounds'], 1)

    def test_turn_start_sums_serial_jev_time_before_the_first_main_call(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rows = [call('j1', role='jev', turn='t1', duration=200), call('j2', role='jev', turn='t1', duration=300),
                    call('m1', turn='t1'), call('j3', role='jev', turn='t1', duration=50),
                    call('m2', turn='t2')]
            write_session(root, 'session-start-0001', rows)
            _, _, threads, _ = analyse(root)
            report = su.report_turn_start(threads)
            self.assertEqual((report['turns'], report['turns_with_jev_before_first_main_call']), (2, 1))
            self.assertEqual(report['serial_jev_ms_before_first_main_call']['median'], 250)

    def test_compaction_calls_report_cached_share_and_missing_pass_labels(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rows = [call('m', turn='t1'), call('k1', role='compaction', prompt=1000, cached=750, turn='t1'),
                    call('k2', role='compaction', prompt=1000, cached=250, turn='t1', status='failed')]
            write_session(root, 'session-compact-0001', rows,
                          outcomes={'compaction_timing': {'decisions': {'defer': 3}, 'chunks': 0}})
            prices, sessions, threads, _ = analyse(root)
            report = su.report_compaction(threads, sessions, prices)
            entry = report['calls_by_label']['unlabelled']
            self.assertEqual((entry['calls'], entry['cached_share']), (2, 0.5))
            self.assertEqual(entry['statuses'], {'completed': 1, 'failed': 1})
            self.assertEqual(report['utility_decisions'], {'compaction_timing': {'defer': 3}})
            self.assertIn('not recorded', report['pass1_vs_applied'])


class SimulatorTest(unittest.TestCase):
    def growing_thread(self, root):
        rows = [call(f'm{n}', prompt=50_000 * (n + 1), cached=50_000 * n, write=50_000, turn=f't{n // 2}')
                for n in range(8)]
        write_session(root, 'session-sim-0001', rows + [call('j', role='jev', turn='t0', prompt=10)])

    def test_baseline_equals_the_recorded_main_call_totals(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.growing_thread(root)
            prices, _, threads, counts = analyse(root)
            totals = su.report_totals(threads, counts, prices)['by_role']['main']
            result = su.simulate_compaction(threads, prices, soft=10**9, hard=10**9, post=1000, regrowth=1.0)
            self.assertEqual(result['baseline']['prompt_tokens'], totals['prompt_tokens'])
            self.assertAlmostEqual(result['baseline']['cost_eq'], totals['cost_eq'])
            self.assertEqual(result['simulated']['compactions'], 0)
            self.assertAlmostEqual(result['simulated']['cost_eq'], result['baseline']['cost_eq'],
                                   msg='a policy that never fires reproduces the baseline')

    def test_soft_threshold_compacts_at_turn_boundaries_and_regrows(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.growing_thread(root)
            prices, _, threads, _ = analyse(root)
            result = su.simulate_compaction(threads, prices, soft=120_000, hard=10**9, post=20_000, regrowth=1.0)
            # Turn starts at 150k (t1) and, after regrowth, again later: compactions only at boundaries.
            self.assertGreaterEqual(result['simulated']['compactions'], 1)
            self.assertLess(result['simulated']['prompt_tokens'], result['baseline']['prompt_tokens'])
            self.assertGreater(result['simulated']['compaction_calls_cost_eq'], 0)

    def test_command_line_prints_aggregates_only(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            root_with_child(root)
            code, output, text = run_main(['--sessions-root', str(root), '--report', 'totals', '--report', 'rounds',
                                           '--report', 'cache-breaks', '--report', 'turn-start',
                                           '--report', 'compaction', '--simulate', 'compaction'])
            self.assertEqual(code, 0)
            self.assertEqual(set(output) - {'sessions_root'},
                             {'totals', 'rounds', 'cache-breaks', 'turn-start', 'compaction', 'simulate-compaction'})
            for private in ('root-session-0001', 'child-session-0001', directory, OPUS, WORKER):
                self.assertNotIn(private, text)


if __name__ == '__main__':
    unittest.main()
