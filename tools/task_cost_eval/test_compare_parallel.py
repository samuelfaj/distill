import contextlib
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from . import compare_parallel
from .parallel_verdict import verdict
from .test_acp_driver import fake_peer

ROOT = Path(__file__).resolve().parent
COHORT = json.loads((ROOT / 'cohort-parallel-v1.json').read_text())


def stage(case, directory, solved):
    target = Path(directory) / 'tools/task_cost_eval' / case['fixture_ref']
    shutil.copytree(ROOT / case['fixture_ref'], target)
    if solved:
        shutil.copytree(ROOT / case['reference_ref'], target, dirs_exist_ok=True)
    return directory


def grade(case, worktree):
    return subprocess.run([sys.executable, str(ROOT / case['grader']['script_ref']), str(worktree)],
                          capture_output=True, text=True).returncode


class CohortTest(unittest.TestCase):
    def test_shape_is_four_divisible_and_two_sequential(self):
        kinds = [c['kind'] for c in COHORT['cases']]
        self.assertEqual((kinds.count('divisible'), kinds.count('sequential')), (4, 2))

    def test_every_grader_fails_untouched_fixture_and_passes_reference(self):
        for case in COHORT['cases']:
            with self.subTest(case=case['id']):
                with tempfile.TemporaryDirectory() as untouched, tempfile.TemporaryDirectory() as solved:
                    self.assertEqual(grade(case, stage(case, untouched, False)), 1)
                    self.assertEqual(grade(case, stage(case, solved, True)), 0)

    def test_grader_rejects_a_partially_done_divisible_case(self):
        # One of three parts missing must still fail: the grader demands every part.
        case = next(c for c in COHORT['cases'] if c['id'] == 'par-functions-en')
        with tempfile.TemporaryDirectory() as directory:
            stage(case, directory, True)
            (Path(directory) / 'tools/task_cost_eval' / case['fixture_ref'] / 'roman.py').write_text(
                (ROOT / case['fixture_ref'] / 'roman.py').read_text())
            self.assertEqual(grade(case, directory), 1)

    def test_rename_grader_rejects_leftover_old_name_and_wrong_caller_currency(self):
        case = next(c for c in COHORT['cases'] if c['id'] == 'seq-rename-en')
        with tempfile.TemporaryDirectory() as directory:
            stage(case, directory, True)
            invoice = Path(directory) / 'tools/task_cost_eval' / case['fixture_ref'] / 'invoice.py'
            invoice.write_text(invoice.read_text().replace(", 'EUR'", ''))
            self.assertEqual(grade(case, directory), 1)


def run(variant, case, kind, credits, wall=10.0, passed=True, complete=True, rep=1):
    return {'variant': variant, 'case': case, 'kind': kind, 'repetition': rep, 'passed': passed,
            'wall_time_s': wall, 'credits': credits, 'accounting_complete': complete}


def results(base, cand):
    """base/cand: (divisible credits, sequential credits, wall, accepted of 2)."""
    rows = []
    for variant, (div, seq, wall, accepted) in (('baseline', base), ('candidate', cand)):
        rows.append(run(variant, 'd', 'divisible', div, wall, passed=accepted >= 1))
        rows.append(run(variant, 's', 'sequential', seq, wall, passed=accepted >= 2))
    return {'runs': rows}


class VerdictTest(unittest.TestCase):
    BASE = (100, 100, 100, 2)

    def test_pass_when_cheaper_faster_and_no_less_accepted(self):
        out = verdict(results(self.BASE, (60, 100, 80, 2)))
        self.assertEqual(out['verdict'], 'PASS')
        self.assertFalse(out['delegation_overuse'])

    def test_equal_credits_per_accepted_still_passes_but_equal_wall_time_does_not(self):
        self.assertEqual(verdict(results(self.BASE, (100, 100, 99, 2)))['verdict'], 'PASS')
        self.assertEqual(verdict(results(self.BASE, (100, 100, 100, 2)))['verdict'], 'FAIL')

    def test_fail_when_credits_per_accepted_rise(self):
        out = verdict(results(self.BASE, (101, 100, 50, 2)))
        self.assertEqual(out['verdict'], 'FAIL')
        self.assertIn('credits_per_accepted not met', out['reasons'])

    def test_fail_when_fewer_tasks_accepted_even_if_cheaper_per_accepted(self):
        out = verdict(results(self.BASE, (10, 10, 50, 1)))
        self.assertEqual(out['verdict'], 'FAIL')
        self.assertEqual(out['reasons'], ['accepted_count not met'])

    def test_incomplete_accounting_never_passes(self):
        data = results(self.BASE, (60, 60, 50, 2))
        data['runs'][2]['accounting_complete'] = False
        self.assertEqual(verdict(data)['verdict'], 'INCOMPLETE')
        data['runs'][2].update(accounting_complete=True, credits=None)
        self.assertEqual(verdict(data)['verdict'], 'INCOMPLETE')

    def test_mismatched_run_sets_and_empty_baseline_are_incomplete(self):
        data = results(self.BASE, (60, 60, 50, 2))
        data['runs'].pop()
        self.assertEqual(verdict(data)['verdict'], 'INCOMPLETE')
        self.assertEqual(verdict(results((1, 1, 1, 0), (1, 1, 1, 2)))['verdict'], 'INCOMPLETE')

    def test_delegation_overuse_boundary_is_ten_percent_of_sequential_credits(self):
        at_limit = verdict(results(self.BASE, (10, 110, 50, 2)))
        over = verdict(results(self.BASE, (10, 111, 50, 2)))
        self.assertFalse(at_limit['delegation_overuse'])
        self.assertTrue(over['delegation_overuse'])
        self.assertAlmostEqual(over['sequential_credit_ratio'], 1.11)

    def test_delegation_overuse_ignores_divisible_credits(self):
        self.assertFalse(verdict(results(self.BASE, (500, 100, 50, 2)))['delegation_overuse'])


class DryRunTest(unittest.TestCase):
    def test_seeded_three_repetitions_balance_starts_and_keep_pairs_adjacent(self):
        cohort = {'cases': COHORT['cases'][:4]}
        plan = compare_parallel.plan_runs(cohort, 3, 20261009)
        self.assertEqual(plan, compare_parallel.plan_runs(cohort, 3, 20261009))
        self.assertEqual(len(plan), 24)
        order = [p[1]['id'] for p in plan[:8:2]]
        self.assertNotEqual(order, [c['id'] for c in cohort['cases']])
        for rep in range(3):
            pairs = [plan[i:i + 2] for i in range(rep * 8, (rep + 1) * 8, 2)]
            self.assertEqual([pair[0][1]['id'] for pair in pairs], order)
            self.assertEqual([pair[0][0] for pair in pairs],
                             ['baseline', 'candidate'] * 2 if rep % 2 == 0 else ['candidate', 'baseline'] * 2)
            for first, second in pairs:
                self.assertEqual(first[1:], second[1:])
                self.assertNotEqual(first[0], second[0])

    def test_acp_study_dry_run_needs_no_binaries_or_auth(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out), mock.patch.object(compare_parallel, 'run_acp') as driver, \
                mock.patch.object(compare_parallel, 'subscription_account') as auth:
            code = compare_parallel.main([
                '--baseline-binary', '/old', '--candidate-binary', '/new', '--transport', 'acp',
                '--baseline-ultracode', '--candidate-ultracode', '--baseline-max-depth', '1',
                '--order-seed', '20261009', '--effort', 'medium', '--worker-effort', 'medium',
                '--utility-effort', 'medium', '--timeout', '900', '--repetitions', '3',
                '--work-root', '/unused', '--output', '/unused.json'])
        self.assertEqual(code, 0)
        self.assertIn('"transport": "acp"', out.getvalue())
        self.assertIn('"worker_effort": "medium"', out.getvalue())
        driver.assert_not_called()
        auth.assert_not_called()

    def test_dry_run_prints_plan_and_executes_nothing(self):
        out = io.StringIO()
        with tempfile.TemporaryDirectory() as directory, \
                mock.patch.object(compare_parallel, 'run_one') as run_one, \
                mock.patch.object(subprocess, 'run') as sub, mock.patch.object(subprocess, 'Popen') as popen, \
                contextlib.redirect_stdout(out):
            code = compare_parallel.main(['--baseline-binary', '/nope/a', '--candidate-binary', '/nope/b',
                                          '--repetitions', '2', '--work-root', directory + '/w',
                                          '--output', directory + '/out.json'])
            self.assertFalse((Path(directory) / 'w').exists() or (Path(directory) / 'out.json').exists())
        self.assertEqual(code, 0)
        run_one.assert_not_called()
        sub.assert_not_called()
        popen.assert_not_called()
        self.assertEqual(out.getvalue().count('plan: '), 24)
        self.assertIn('dry run: 24 runs planned, nothing executed', out.getvalue())

    def test_plan_interleaves_variants_per_case(self):
        plan = compare_parallel.plan_runs(COHORT, 1)
        self.assertEqual([p[0] for p in plan[:2]], ['baseline', 'candidate'])
        self.assertEqual(plan[0][1], plan[1][1])

    def test_plan_reverses_pair_order_on_second_repetition(self):
        plan = compare_parallel.plan_runs(COHORT, 2)
        second = plan[len(COHORT['cases']) * 2:]
        self.assertEqual([p[0] for p in second[:2]], ['candidate', 'baseline'])
        self.assertEqual([(p[1]['id'], p[2]) for p in plan if p[0] == 'baseline'],
                         [(p[1]['id'], p[2]) for p in plan if p[0] == 'candidate'])

    def test_same_binary_selected_case_and_explicit_modes_are_visible_in_plan(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out), mock.patch.object(compare_parallel, 'run_one') as run_one:
            code = compare_parallel.main([
                '--baseline-binary', '/same/distill', '--candidate-binary', '/same/distill',
                '--baseline-ultracode', '--baseline-max-depth', '1',
                '--candidate-ultracode', '--candidate-max-depth', '3', '--effort', 'high',
                '--case', 'seq-rename-en', '--repetitions', '2',
                '--work-root', '/unused', '--output', '/unused.json',
            ])
        self.assertEqual(code, 0)
        run_one.assert_not_called()
        self.assertEqual(out.getvalue().count('plan: '), 4)
        self.assertIn('"ultracode": true', out.getvalue())
        self.assertIn('"max_depth": 1', out.getvalue())
        self.assertIn('"max_depth": 3', out.getvalue())
        self.assertIn('"effort": "high"', out.getvalue())

    def test_invalid_case_or_depth_cannot_start_execution(self):
        common = ['--baseline-binary', '/nope', '--candidate-binary', '/nope',
                  '--work-root', '/unused', '--output', '/unused.json', '--execute']
        for extra in (['--case', 'invented-case'], ['--candidate-max-depth', '0'], ['--timeout', 'inf']):
            with contextlib.redirect_stderr(io.StringIO()), \
                    mock.patch.object(compare_parallel, 'run_one') as run_one, \
                    self.assertRaises(SystemExit) as error:
                compare_parallel.main(common + extra)
            self.assertEqual(error.exception.code, 2)
            run_one.assert_not_called()

    def test_aggregate_groups_by_variant_and_kind(self):
        rows = [dict(run('baseline', 'd', 'divisible', 4.0), model_calls=3, **{f: 1 for f in compare_parallel.TOKEN_FIELDS}),
                dict(run('baseline', 's', 'sequential', 2.0, passed=False), model_calls=1,
                     **{f: 1 for f in compare_parallel.TOKEN_FIELDS})]
        agg = compare_parallel.aggregate(rows)
        self.assertEqual(agg['baseline/all']['credits_per_accepted'], 6.0)
        self.assertEqual((agg['baseline/sequential']['accepted'], agg['baseline/sequential']['credits_per_accepted']),
                         (0, None))
        self.assertEqual(agg['baseline/all']['model_calls'], 4)


class ActivationRunTest(unittest.TestCase):
    def test_unverified_teardown_skips_grading_and_ledger_reads(self):
        case = COHORT['cases'][0]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            profile = root / 'profile'
            profile.mkdir()
            (profile / 'codex-auth.json').write_text('{}')
            with mock.patch.object(compare_parallel, 'run_acp', return_value={
                    'session': 'runtime-session', 'completed': False, 'cleanup_complete': False,
                    'error': 'teardown unverified', 'exit_code': None, 'timed_out': True}), \
                    mock.patch.object(compare_parallel, 'run_grader') as grader, \
                    mock.patch.object(compare_parallel, 'distill_accounting') as ledger:
                result = compare_parallel.run_one(Path(sys.executable), 'baseline', case, 1, root, profile, 3,
                    frozen_inputs=compare_parallel.case_hashes(case), binary_hash=compare_parallel._sha256_file(Path(sys.executable)),
                    transport='acp')
            grader.assert_not_called()
            ledger.assert_not_called()
            self.assertFalse(result['passed'] or result['accounting_complete'])
            self.assertFalse(result['accounting_evidence']['complete'])
            self.assertIn('unverified inference cleanup', result['accounting_evidence']['reasons'][0])
            self.assertIsNone(result['credits'])
            self.assertFalse((Path(result['output_dir']) / 'distill-home/codex-auth.json').exists())

    def test_incomplete_acp_cannot_publish_comparison_pass(self):
        case = COHORT['cases'][0]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            def recorded(binary, variant, case, repetition, *args, **kwargs):
                return dict(run(variant, case['id'], case['kind'], 1, wall=2 if variant == 'baseline' else 1),
                    model_calls=1, **{f: 1 for f in compare_parallel.TOKEN_FIELDS},
                    acp={'completed': variant == 'candidate', 'setup_confirmed': variant == 'candidate',
                         'cleanup_complete': True, 'error': 'activation missing'})
            with mock.patch.object(compare_parallel, 'run_one', side_effect=recorded), \
                    mock.patch.object(compare_parallel, 'subscription_account'), contextlib.redirect_stdout(io.StringIO()):
                code = compare_parallel.main(['--baseline-binary', sys.executable, '--candidate-binary', sys.executable,
                    '--transport', 'acp', '--case', case['id'], '--repetitions', '1', '--execute',
                    '--work-root', str(root / 'runs'), '--output', str(root / 'result.json')])
            self.assertEqual(code, 1)
            result = json.loads((root / 'result.json').read_text())
            self.assertEqual(result['verdict']['verdict'], 'INCOMPLETE')
            self.assertTrue(any('activation missing' in reason for reason in result['verdict']['reasons']))
            self.assertIn(str(ROOT / 'acp_driver.py'), result['frozen_support_hashes'])
            self.assertIn(str(ROOT / 'graders/_par_common.py'), result['frozen_support_hashes'])

    def test_between_run_input_or_candidate_binary_mutation_cannot_pass(self):
        case = COHORT['cases'][0]
        for drift in ('prompt', 'candidate-binary', 'helper', 'cohort'):
            with self.subTest(drift=drift), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                inputs = root / 'inputs'
                shutil.copytree(ROOT / case['fixture_ref'], inputs / case['fixture_ref'])
                for ref in (case['prompt_ref'], case['grader']['script_ref']):
                    target = inputs / ref
                    target.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(ROOT / ref, target)
                helper = inputs / 'graders/_par_common.py'
                shutil.copyfile(ROOT / 'graders/_par_common.py', helper)
                cohort_path = root / 'cohort.json'
                cohort_path.write_text(json.dumps({**COHORT, 'cases': [case]}))
                baseline, candidate = root / 'baseline', root / 'candidate'
                baseline.write_text('original baseline binary')
                candidate.write_text('original candidate binary')
                output = root / 'result.json'

                def plan_with_mutation(cohort, repetitions, order_seed=None):
                    yield 'baseline', case, 1
                    # This happens after baseline's post-run check, before candidate.
                    target = {'prompt': inputs / case['prompt_ref'], 'candidate-binary': candidate,
                              'helper': helper, 'cohort': cohort_path}[drift]
                    target.write_text('mutated between paired runs')
                    yield 'candidate', case, 1

                def accept(binary, variant, case, repetition, *args, **kwargs):
                    return dict(run(variant, case['id'], case['kind'], 1.0,
                                    wall=10 if variant == 'baseline' else 5),
                                model_calls=1, **{f: 1 for f in compare_parallel.TOKEN_FIELDS})

                with mock.patch.object(compare_parallel, 'ROOT', inputs), \
                        mock.patch.object(compare_parallel, 'plan_runs', side_effect=plan_with_mutation), \
                        mock.patch.object(compare_parallel, 'subscription_account'), \
                        mock.patch.object(compare_parallel, 'run_one', side_effect=accept) as run_one, \
                        contextlib.redirect_stdout(io.StringIO()):
                    with self.assertRaisesRegex(RuntimeError, 'Frozen comparison inputs or binaries changed before'):
                        compare_parallel.main([
                            '--baseline-binary', str(baseline), '--candidate-binary', str(candidate),
                            '--cohort', str(cohort_path), '--repetitions', '1',
                            '--work-root', str(root / 'runs'), '--output', str(output), '--execute',
                        ])
                run_one.assert_called_once()
                self.assertEqual(run_one.call_args.args[1], 'baseline')
                self.assertEqual(run_one.call_args.kwargs['binary_hash'], compare_parallel._sha256_file(baseline))
                self.assertFalse(output.exists(), 'drift must not publish an unmatched PASS')

    def test_acp_runtime_session_accounting_pins_sanitization_and_auth_cleanup(self):
        case = COHORT['cases'][0]
        accounting = {'accounting_complete': True, 'credit_estimate': 1.0, 'calls': 1,
                      'call_usage': [{'model': 'auxiliary', 'effort': 'auto'}],
                      'accounting_evidence': {'complete': False, 'reasons': ['observed retry absent from ledger']},
                      **{f: 1 for f in compare_parallel.TOKEN_FIELDS}}
        for scenario in ('success', 'activation-false'):
            with self.subTest(scenario=scenario), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                peer = fake_peer(root, scenario)
                profile = root / 'profile'
                profile.mkdir()
                (profile / 'codex-auth.json').write_text('{}')
                with mock.patch.dict(os.environ, {'GROK_SUBAGENTS_MAX_DEPTH': '99',
                        'GROK_SESSION_SUMMARY_MODEL': 'wrong-model', 'GROK_REASONING_EFFORT': 'high'}), \
                        mock.patch.object(compare_parallel, 'distill_accounting', return_value=accounting) as ledger:
                    result = compare_parallel.run_one(peer, 'baseline', case, 1, root / 'runs', profile, 3,
                        frozen_inputs=compare_parallel.case_hashes(case), binary_hash=compare_parallel._sha256_file(peer),
                        transport='acp', ultracode=True, effort='medium', worker_effort='medium', utility_effort='medium')
                output = Path(result['output_dir'])
                ledger.assert_called_once_with(output / 'distill-home', 'runtime-session', strict=True)
                self.assertEqual(result['accounting_evidence'], accounting['accounting_evidence'])
                self.assertFalse(result['accounting_complete'], 'study completeness must include the evidence audit')
                self.assertEqual(json.loads((output / 'worktree/peer-env.json').read_text()), {})
                self.assertEqual(result['acp']['completed'], scenario == 'success')
                self.assertFalse(result['passed'], 'unsolved fixture still fails its external grader')
                self.assertEqual(result['credits'], 1.0, 'failed runs retain accounting')
                self.assertEqual(result['call_usage'], accounting['call_usage'], 'do not invent auxiliary effort parity')
                self.assertEqual(result['settings']['configured_models']['worker_effort'], 'medium')
                self.assertEqual(result['settings']['configured_utility']['effort'], 'medium')
                self.assertIsNone(result['settings']['depth_env'])
                command = json.loads((output / 'command.json').read_text())
                self.assertEqual(result['settings']['launch_command'], command)
                self.assertEqual(command[-1], 'stdio')
                self.assertIn('agent', command)
                self.assertNotIn('--ultracode', command)
                self.assertGreater(command.index('--always-approve'), command.index('agent'))
                self.assertNotIn('--permission-mode', command)
                self.assertEqual(result['settings']['permissions'], 'always-approve')
                self.assertFalse((output / 'distill-home/codex-auth.json').exists())

    def test_grader_loop_is_bounded_and_failure_duration_is_separate(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            script = root / 'loop.py'
            script.write_text('import time\nprint("grader started", flush=True)\nwhile True: time.sleep(.1)\n')
            grade = compare_parallel.run_grader(script, root, root, timeout=.15)
            self.assertTrue(grade['timed_out'])
            self.assertNotEqual(grade['exit_code'], 0)
            self.assertLess(grade['wall_time_s'], 3)
            self.assertIn('grader started', (root / 'grader.txt').read_text())

    def test_run_uses_real_flag_and_depth_and_requires_activation_receipt(self):
        case = next(c for c in COHORT['cases'] if c['id'] == 'seq-rename-en')
        accounting = {'accounting_complete': True, 'credit_estimate': 1.0, 'calls': 2,
                      **{f: 1 for f in compare_parallel.TOKEN_FIELDS}}
        for confirmed in (True, False):
            with self.subTest(confirmed=confirmed), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                profile = root / 'profile'
                profile.mkdir()
                (profile / 'codex-auth.json').write_text('{}')
                real_popen = subprocess.Popen

                def launch(command, **kwargs):
                    if command[0] != sys.executable:
                        return real_popen(command, **kwargs)
                    self.assertIn('--ultracode', command)
                    self.assertEqual(command[command.index('--effort') + 1], 'high')
                    self.assertEqual(kwargs['env']['GROK_SUBAGENTS_MAX_DEPTH'], '3')
                    worktree = Path(kwargs['cwd'])
                    shutil.copytree(ROOT / case['reference_ref'],
                                    worktree / 'tools/task_cost_eval' / case['fixture_ref'], dirs_exist_ok=True)
                    if confirmed:
                        session = command[command.index('--session-id') + 1]
                        kwargs['stderr'].write(f'UltraCode enabled for session {session}\n')
                    return mock.Mock(returncode=0, poll=mock.Mock(return_value=0))

                with mock.patch.object(subprocess, 'Popen', side_effect=launch), \
                        mock.patch.object(compare_parallel, 'distill_accounting', return_value=accounting):
                    result = compare_parallel.run_one(
                        Path(sys.executable), 'candidate', case, 1, root / 'runs', profile, 30,
                        frozen_inputs=compare_parallel.case_hashes(case),
                        binary_hash=compare_parallel._sha256_file(Path(sys.executable)),
                        ultracode=True, max_depth=3, effort='high',
                    )
                self.assertEqual(result['passed'], confirmed)
                self.assertEqual(result['ultracode_activation_confirmed'], confirmed)
                self.assertTrue(result['frozen_inputs_unchanged'] and result['binary_unchanged'])
                self.assertEqual(result['settings']['binary_sha256'], compare_parallel._sha256_file(Path(sys.executable)))
                self.assertEqual(result['settings']['configured_models']['worker'], 'chatgpt/gpt-6-luna')
                self.assertEqual(result['settings']['configured_utility'],
                                 {'model': 'chatgpt/gpt-6-luna', 'effort': 'auto'})
                output = Path(result['output_dir'])
                self.assertFalse((output / 'distill-home/codex-auth.json').exists())
                self.assertEqual(json.loads((output / 'settings.json').read_text()), result['settings'])

    def test_launch_failure_still_removes_isolated_auth(self):
        case = COHORT['cases'][0]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            profile = root / 'profile'
            profile.mkdir()
            (profile / 'codex-auth.json').write_text('{}')
            real_popen = subprocess.Popen

            def launch(command, **kwargs):
                if command[0] == sys.executable:
                    raise OSError('launch failed')
                return real_popen(command, **kwargs)

            with mock.patch.object(subprocess, 'Popen', side_effect=launch), self.assertRaises(OSError):
                compare_parallel.run_one(
                    Path(sys.executable), 'candidate', case, 1, root / 'runs', profile, 30,
                    frozen_inputs=compare_parallel.case_hashes(case),
                    binary_hash=compare_parallel._sha256_file(Path(sys.executable)),
                )
            self.assertFalse((root / 'runs' / f'candidate-{case["id"]}-1/distill-home/codex-auth.json').exists())
if __name__ == '__main__':
    unittest.main()
