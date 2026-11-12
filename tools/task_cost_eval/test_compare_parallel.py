import contextlib
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from collections import Counter
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
    def test_mode_comparison_plan_uses_explicit_modes_and_common_capacity(self):
        for mode in ('ultrafast', 'ultrasave'):
            out = io.StringIO()
            with self.subTest(mode=mode), contextlib.redirect_stdout(out), \
                    mock.patch.object(compare_parallel, 'run_one') as run_one:
                code = compare_parallel.main([
                    '--baseline-binary', '/same/distill', '--candidate-binary', '/same/distill',
                    '--baseline-mode', 'normal', '--candidate-mode', mode, '--transport', 'acp',
                    '--max-concurrent', '8', '--order-seed', '20261020', '--verdict', mode,
                    '--work-root', '/unused', '--output', '/unused.json'])
            self.assertEqual(code, 0)
            self.assertIn('"execution_mode": "normal"', out.getvalue())
            self.assertIn(f'"execution_mode": "{mode}"', out.getvalue())
            self.assertIn('"max_concurrent": 8', out.getvalue())
            run_one.assert_not_called()

    def test_conflicting_mode_flags_are_rejected_before_execution(self):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit), \
                mock.patch.object(compare_parallel, 'run_one') as run_one:
            compare_parallel.main(['--baseline-binary', '/same', '--candidate-binary', '/same',
                '--candidate-ultracode', '--candidate-mode', 'ultrafast',
                '--work-root', '/unused', '--output', '/unused.json', '--execute'])
        run_one.assert_not_called()

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
                '--worker-model', 'chatgpt/gpt-6.1-sol',
                '--work-root', '/unused', '--output', '/unused.json'])
        self.assertEqual(code, 0)
        self.assertIn('"transport": "acp"', out.getvalue())
        self.assertIn('"worker_effort": "medium"', out.getvalue())
        self.assertIn('"worker_model": "chatgpt/gpt-6.1-sol"', out.getvalue())
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
    def test_ultrafast_acknowledgement_and_fixed_capacity_are_recorded(self):
        case = COHORT['cases'][0]
        accounting = {'accounting_complete': True, 'credit_estimate': 1.0, 'calls': 1,
                      'call_usage': [{'complete': True}], 'accounting_evidence': {'complete': True},
                      **{field: 1 for field in compare_parallel.TOKEN_FIELDS}}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            peer = fake_peer(root)
            profile = root / 'profile'
            profile.mkdir()
            (profile / 'codex-auth.json').write_text('{}')
            with mock.patch.object(compare_parallel, 'distill_accounting', return_value=accounting), \
                    mock.patch.object(compare_parallel, 'run_grader', return_value={
                        'exit_code': 0, 'timed_out': False, 'wall_time_s': .01}), \
                    mock.patch.dict(os.environ, {key: '99' for key in compare_parallel.CAPACITY_ENV}):
                result = compare_parallel.run_one(peer, 'candidate', case, 1, root / 'runs', profile, 3,
                    frozen_inputs=compare_parallel.case_hashes(case), binary_hash=compare_parallel._sha256_file(peer),
                    transport='acp', execution_mode='ultrafast', max_concurrent=8)
            self.assertTrue(result['passed'])
            self.assertTrue(result['execution_mode_activation_confirmed'])
            self.assertFalse(result['ultracode_activation_confirmed'])
            self.assertEqual(result['settings']['execution_mode'], 'ultrafast')
            self.assertEqual(result['settings']['capacity_env'], {key: '8' for key in compare_parallel.CAPACITY_ENV})
            self.assertEqual((result['effective_mode_mix'], result['mode_switches']), ({'ultrafast': 1}, 0))

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
                        transport='acp', ultracode=True, model='chatgpt/gpt-6-astra', effort='medium',
                        worker_model='chatgpt/gpt-6.1-sol', worker_effort='medium', utility_effort='medium')
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
                self.assertEqual(result['settings']['configured_models']['default'], 'chatgpt/gpt-6-astra')
                self.assertEqual(result['settings']['configured_models']['worker'], 'chatgpt/gpt-6.1-sol')
                self.assertEqual(result['settings']['configured_utility'],
                                 {'model': 'chatgpt/gpt-6-luna', 'effort': 'medium'})
                self.assertIsNone(result['settings']['depth_env'])
                command = json.loads((output / 'command.json').read_text())
                self.assertEqual(result['settings']['launch_command'], command)
                self.assertEqual(json.loads((output / 'settings.json').read_text()), result['settings'])
                self.assertEqual(command[-1], 'stdio')
                self.assertEqual(command[command.index('--model') + 1], 'chatgpt/gpt-6-astra')
                receipt_model = result['settings']['model_receipt']['result']['_meta']['canonicalModelId']
                self.assertEqual(receipt_model, 'chatgpt/gpt-6-astra')
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
    def test_enable_jev_stages_owner_only_key_sets_env_and_removes_it_even_on_failure(self):
        case = COHORT['cases'][0]
        seen = {}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            profile = root / 'profile'
            profile.mkdir()
            (profile / 'codex-auth.json').write_text('{}')
            (profile / 'openrouter-auth.json').write_text('{"key": "sk-or-test"}')

            def acp(command, *, cwd, environment, **kwargs):
                staged = cwd.parent / 'distill-home/openrouter-auth.json'
                seen.update(key=environment.get('OPENROUTER_API_KEY'), mode=staged.stat().st_mode & 0o777,
                            body=staged.read_text())
                raise OSError('launch failed')

            with mock.patch.object(compare_parallel, 'run_acp', side_effect=acp), self.assertRaises(OSError):
                compare_parallel.run_one(
                    Path(sys.executable), 'candidate', case, 1, root / 'runs', profile, 30,
                    frozen_inputs=compare_parallel.case_hashes(case),
                    binary_hash=compare_parallel._sha256_file(Path(sys.executable)),
                    transport='acp', enable_jev=True, utility_model='chatgpt/gpt-6-terra')
            home = root / 'runs' / f'candidate-{case["id"]}-1/distill-home'
            self.assertEqual(seen, {'key': 'sk-or-test', 'mode': 0o600, 'body': '{"key": "sk-or-test"}'})
            self.assertFalse((home / 'openrouter-auth.json').exists())
            self.assertFalse((home / 'codex-auth.json').exists())
            config = (home / 'config.toml').read_text()
            self.assertIn('api_key_env = "OPENROUTER_API_KEY"', config)
            self.assertIn('[jev.local]\nmodel = "chatgpt/gpt-6-terra"', config)
            self.assertIn('session_summary = "chatgpt/gpt-6-terra"', config)
            settings = json.loads((root / 'runs' / f'candidate-{case["id"]}-1/settings.json').read_text())
            self.assertEqual((settings['jev_enabled'], settings['utility_model']), (True, 'chatgpt/gpt-6-terra'))

    def test_default_config_keeps_jev_off_and_luna_utility(self):
        config = compare_parallel.distill_config()
        self.assertIn('api_key_env = "DISTILL_BENCH_NO_EXTERNAL_MODEL_KEY"', config)
        self.assertIn('[jev.local]\nmodel = "chatgpt/gpt-6-luna"', config)
        self.assertIn('session_summary = "chatgpt/gpt-6-luna"', config)

    def test_enable_jev_without_key_never_starts_a_model(self):
        case = COHORT['cases'][0]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            profile = root / 'profile'
            profile.mkdir()
            (profile / 'codex-auth.json').write_text('{}')
            (profile / 'openrouter-auth.json').write_text('{}')
            with mock.patch.object(compare_parallel, 'run_acp') as acp, self.assertRaises(ValueError):
                compare_parallel.run_one(
                    Path(sys.executable), 'candidate', case, 1, root / 'runs', profile, 30,
                    frozen_inputs=compare_parallel.case_hashes(case),
                    binary_hash=compare_parallel._sha256_file(Path(sys.executable)),
                    transport='acp', enable_jev=True)
            acp.assert_not_called()
            self.assertFalse((root / 'runs' / f'candidate-{case["id"]}-1/distill-home/openrouter-auth.json').exists())

    def test_jev_usd_is_separate_from_credits_and_unpriced_jev_is_incomplete(self):
        case = COHORT['cases'][0]
        for ticks, complete in ((25_000_000, True), (None, False)):
            with self.subTest(ticks=ticks), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                peer = fake_peer(root)
                profile = root / 'profile'
                profile.mkdir()
                (profile / 'codex-auth.json').write_text('{}')
                (profile / 'openrouter-auth.json').write_text('{"key": "k"}')
                accounting = {'accounting_complete': True, 'credit_estimate': 1.0, 'calls': 2,
                              'call_usage': [{'role': 'main', 'cost_usd_ticks': None},
                                             {'role': 'jev', 'cost_usd_ticks': ticks}],
                              'accounting_evidence': {'complete': True},
                              **{f: 1 for f in compare_parallel.TOKEN_FIELDS}}
                with mock.patch.object(compare_parallel, 'distill_accounting', return_value=accounting), \
                        mock.patch.object(compare_parallel, 'run_grader', return_value={
                            'exit_code': 0, 'timed_out': False, 'wall_time_s': .01}):
                    result = compare_parallel.run_one(peer, 'candidate', case, 1, root / 'runs', profile, 3,
                        frozen_inputs=compare_parallel.case_hashes(case),
                        binary_hash=compare_parallel._sha256_file(peer), transport='acp', enable_jev=True)
                self.assertEqual(result['credits'], 1.0, 'Jev is not folded into credits')
                self.assertEqual(result['jev_usd'], 0.0025 if complete else None)
                self.assertEqual(result['accounting_complete'], complete)

    def test_compare_versions_dry_run_allows_same_mode_and_different_binaries(self):
        for mode in ('ultrafast', 'ultrasave'):
            out = io.StringIO()
            with self.subTest(mode=mode), contextlib.redirect_stdout(out), \
                    mock.patch.object(compare_parallel, 'run_one') as run_one:
                code = compare_parallel.main([
                    '--baseline-binary', '/old/distill', '--candidate-binary', '/new/distill',
                    '--baseline-mode', mode, '--candidate-mode', mode, '--transport', 'acp',
                    '--max-concurrent', '8', '--order-seed', '1', '--verdict', mode, '--compare-versions',
                    '--enable-jev', '--utility-model', 'chatgpt/gpt-6-terra',
                    '--work-root', '/unused', '--output', '/unused.json'])
            self.assertEqual(code, 0)
            self.assertIn('"utility_model": "chatgpt/gpt-6-terra"', out.getvalue())
            self.assertIn('"enable_jev": true', out.getvalue())
            run_one.assert_not_called()

    def test_strict_mode_rejects_same_mode_pair_and_compare_versions_needs_a_paired_verdict(self):
        base = ['--baseline-binary', '/a', '--candidate-binary', '/b', '--transport', 'acp',
                '--max-concurrent', '8', '--order-seed', '1', '--work-root', '/u', '--output', '/u.json']
        for extra in (['--baseline-mode', 'ultrasave', '--candidate-mode', 'ultrasave', '--verdict', 'ultrasave'],
                      ['--baseline-mode', 'normal', '--candidate-mode', 'ultrasave', '--compare-versions']):
            with self.subTest(extra=extra), contextlib.redirect_stderr(io.StringIO()), \
                    self.assertRaises(SystemExit) as error:
                compare_parallel.main(base + extra)
            self.assertEqual(error.exception.code, 2)


class AggregateEffortTest(unittest.TestCase):
    def test_main_effort_mix_strips_prefix_and_switches_ignore_jev_and_old_runs(self):
        first = run('candidate', 'a', 'divisible', 1.0)
        first['call_usage'] = [{'role': 'main', 'effort': 'effort:low'}, {'role': 'main', 'effort': 'effort:medium'},
                               {'role': 'main', 'effort': 'effort:medium'}, {'role': 'jev', 'effort': 'effort:high'},
                               {'role': 'main', 'effort': 'effort:low'}]
        second = run('candidate', 'b', 'sequential', 1.0)
        second['call_usage'] = [{'role': 'main', 'effort': 'absent'}, {'role': 'main', 'effort': None}]
        legacy = run('candidate', 'c', 'sequential', 1.0)
        out = compare_parallel.aggregate([first, second, legacy])['candidate/all']
        self.assertEqual(out['main_effort_mix'], {'absent': 1, 'low': 2, 'medium': 2, 'unknown': 1})
        self.assertEqual(out['main_effort_switches'], 3)

    def test_mode_mix_switches_durations_and_rounds_sum_next_to_effort_mix(self):
        first = dict(run('candidate', 'a', 'divisible', 1.0), effective_mode_mix={'normal': 1, 'ultrafast': 1},
                     mode_switches=1, model_time_ms=900, jev_time_ms=100, root_rounds=3, child_calls=2)
        second = dict(run('candidate', 'b', 'sequential', 1.0), effective_mode_mix={'normal': 2, 'ultrasave': 1},
                      mode_switches=2, model_time_ms=100, jev_time_ms=50, root_rounds=1, child_calls=0)
        out = compare_parallel.aggregate([first, second, run('candidate', 'c', 'sequential', 1.0)])['candidate/all']
        self.assertEqual(out['effective_mode_mix'], {'normal': 3, 'ultrafast': 1, 'ultrasave': 1})
        self.assertEqual((out['mode_switches'], out['model_time_ms'], out['jev_time_ms']), (3, 1000, 150))
        self.assertEqual((out['root_rounds'], out['child_calls']), (4, 2))


def campaign_row(variant, case, rep, credits, wall, passed=True, **extra):
    return dict(run(variant, case['id'], case['kind'], credits, wall=wall, passed=passed, rep=rep),
                model_calls=1, **{f: 1 for f in compare_parallel.TOKEN_FIELDS}, **extra)


class YardstickTest(unittest.TestCase):
    """S-005: Jev preflight, A/A arm, binary commit, kept failures and per-call time."""

    def profile(self, root, key='sk-or-test'):
        profile = root / 'profile'
        profile.mkdir()
        (profile / 'codex-auth.json').write_text('{}')
        (profile / 'openrouter-auth.json').write_text(json.dumps({'key': key}))
        return profile

    def campaign(self, root, extra, recorded, preflight=None):
        argv = ['--baseline-binary', sys.executable, '--candidate-binary', sys.executable,
                '--case', COHORT['cases'][0]['id'], '--case', COHORT['cases'][4]['id'], '--repetitions', '3',
                '--distill-profile', str(self.profile(root)), '--work-root', str(root / 'runs'),
                '--output', str(root / 'result.json'), '--execute', *extra]
        preflight = preflight or {'ok': True, 'status': 200, 'error': None}
        with mock.patch.object(compare_parallel, 'run_one', side_effect=recorded) as run_one, \
                mock.patch.object(compare_parallel, 'jev_preflight', return_value=preflight) as probe, \
                mock.patch.object(compare_parallel, 'subscription_account'), \
                contextlib.redirect_stdout(io.StringIO()):
            code = compare_parallel.main(argv)
        return code, json.loads((root / 'result.json').read_text()), run_one, probe

    def test_failed_jev_preflight_aborts_before_the_first_run_with_the_http_status(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            err = io.StringIO()
            with mock.patch.object(compare_parallel, 'run_one') as run_one, \
                    mock.patch.object(compare_parallel, 'jev_preflight',
                                      return_value={'ok': False, 'status': 401, 'error': None}) as probe, \
                    mock.patch.object(compare_parallel, 'subscription_account'), \
                    contextlib.redirect_stderr(err), self.assertRaises(SystemExit) as error:
                compare_parallel.main(['--baseline-binary', sys.executable, '--candidate-binary', sys.executable,
                                       '--enable-jev', '--distill-profile', str(self.profile(root, 'dead-key')),
                                       '--work-root', str(root / 'runs'), '--output', str(root / 'out.json'),
                                       '--execute'])
            self.assertEqual(error.exception.code, 2)
            probe.assert_called_once_with('dead-key')
            run_one.assert_not_called()
            self.assertIn('Jev preflight failed (HTTP 401)', err.getvalue())
            self.assertFalse((root / 'runs').exists() or (root / 'out.json').exists())

    def test_preflight_mirrors_the_rust_decision_request_without_network(self):
        seen = {}

        class Reply:
            status = 200

            def read(self):
                return b'{"answers": {}}'

            def __enter__(self):
                return self

            def __exit__(self, *args):
                return False

        def urlopen(request, timeout):
            seen.update(url=request.full_url, method=request.get_method(), timeout=timeout,
                        auth=request.get_header('Authorization'), body=json.loads(request.data))
            return Reply()

        with mock.patch.object(compare_parallel.urllib.request, 'urlopen', side_effect=urlopen):
            self.assertEqual(compare_parallel.jev_preflight('k'), {'ok': True, 'status': 200, 'error': None})
        self.assertEqual((seen['url'], seen['method'], seen['auth']),
                         ('https://openrouter.ai/api/alpha/decisions', 'POST', 'Bearer k'))
        self.assertEqual(seen['body']['model'], '~typesafe/jev-latest')
        self.assertEqual(seen['body']['questions']['ready']['type'], 'noul')
        self.assertNotIn('k', json.dumps(seen['body']).replace('"', ' ').split(), 'the key travels only in the header')

    def test_identical_aa_arm_is_neutral_and_a_drifting_one_invalidates_the_campaign(self):
        for drift, expected in ((1.0, 'valid'), (1.2, 'INVALID')):
            with self.subTest(drift=drift), tempfile.TemporaryDirectory() as directory:
                def recorded(binary, variant, case, repetition, *args, **kwargs):
                    credits = 10 * (drift if variant == compare_parallel.AA_VARIANT else 1)
                    # Uneven split across cases: the median of paired ratios would not read 1.
                    wall = 50 if (case['kind'] == 'divisible') == (variant == 'candidate') else 100
                    return campaign_row(variant, case, repetition, credits, wall)

                code, result, run_one, _ = self.campaign(Path(directory), ['--aa-arm'], recorded)
                self.assertEqual(run_one.call_count, 18, 'three arms x two cases x three repetitions')
                check = result['aa_check']
                self.assertEqual(check['valid'], expected == 'valid')
                self.assertAlmostEqual(check['credit_ratio'], drift)
                self.assertAlmostEqual(check['time_ratio'], 1.0)
                if expected == 'INVALID':
                    self.assertEqual(result['verdict']['verdict'], 'INVALID')
                    self.assertTrue(any('A/A credit ratio 1.2' in r for r in result['verdict']['reasons']))
                    self.assertEqual(code, 1)
                else:
                    self.assertNotEqual(result['verdict']['verdict'], 'INVALID')
                self.assertEqual(Counter(r['variant'] for r in result['runs'])[compare_parallel.AA_VARIANT], 6)

    def test_three_arms_rotate_so_each_arm_leads_equally(self):
        variants = ('baseline', 'candidate', compare_parallel.AA_VARIANT)
        plan = compare_parallel.plan_runs({'cases': COHORT['cases'][:3]}, 3, None, variants)
        leaders = Counter(plan[i][0] for i in range(0, len(plan), 3))
        self.assertEqual(leaders, {variant: 3 for variant in variants})
        for i in range(0, len(plan), 3):
            self.assertEqual({p[0] for p in plan[i:i + 3]}, set(variants))
            self.assertEqual(len({(p[1]['id'], p[2]) for p in plan[i:i + 3]}), 1)

    def test_result_records_binary_commits_preflight_and_kept_failure_folders(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kept = root / 'kept'

            def recorded(binary, variant, case, repetition, *args, **kwargs):
                folder = root / 'runs' / f'{variant}-{case["id"]}-{repetition}'
                (folder / 'distill-home').mkdir(parents=True)
                (folder / 'acp-transcript.jsonl').write_text('{}\n')
                (folder / 'distill-home' / 'openrouter-auth.json').write_text('{"key": "leak"}')
                return campaign_row(variant, case, repetition, 1.0, 10, passed=variant == 'baseline',
                                    output_dir=str(folder))

            code, result, _, probe = self.campaign(
                root, ['--enable-jev', '--keep-failed-runs', str(kept), '--candidate-commit', 'feedbeef'], recorded)
            probe.assert_called_once()
            self.assertEqual(result['jev_preflight']['status'], 200)
            self.assertEqual(result['binaries']['candidate']['source_commit'], 'feedbeef')
            self.assertEqual(result['binaries']['candidate']['commit_source'], 'argument')
            self.assertEqual(result['binaries']['baseline']['source_commit'], 'unknown')
            failed = [r for r in result['runs'] if not r['passed']]
            self.assertEqual(len(failed), 6)
            self.assertEqual(sorted(result['failed_run_dirs']), sorted(r['kept_run_dir'] for r in failed))
            for row in failed:
                copy = Path(row['kept_run_dir'])
                self.assertTrue((copy / 'acp-transcript.jsonl').is_file(), 'evidence is kept')
                self.assertFalse((copy / 'distill-home' / 'openrouter-auth.json').exists(), 'credentials are not')
            self.assertFalse(any(kept.glob('baseline-*')), 'passing runs are not copied')

    def test_binary_commit_reads_version_output_before_the_declared_argument(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / 'distill'
            binary.write_text(f'#!{sys.executable}\nprint("distill 2.0.68 (7c2e6174abcd)")\n')
            binary.chmod(0o700)
            self.assertEqual(compare_parallel.binary_commit(binary, 'other'),
                             {'source_commit': '7c2e6174abcd', 'commit_source': 'version', 'declared_commit': 'other'})
            self.assertEqual(compare_parallel.binary_commit(Path(directory) / 'missing'),
                             {'source_commit': 'unknown', 'commit_source': 'unknown'})

    def test_call_summary_splits_root_rounds_child_calls_and_model_from_jev_time(self):
        calls = [{'role': 'main', 'agent': 'main', 'api_duration_ms': 1000},
                 {'role': 'main_retry', 'agent': 'main', 'api_duration_ms': 200},
                 {'role': 'main', 'agent': 'worker', 'api_duration_ms': 500},
                 {'role': 'jev', 'agent': 'main', 'api_duration_ms': 80, 'status': 'completed'},
                 {'role': 'jev', 'agent': 'main', 'api_duration_ms': None, 'status': 'rejected'},
                 {'role': 'utility', 'agent': 'main'}]
        self.assertEqual(compare_parallel.call_summary(calls), {
            'model_time_ms': 1700, 'jev_time_ms': 80, 'calls_without_duration': 2,
            'root_rounds': 2, 'child_calls': 1, 'jev_failed_calls': 1})


class MatrixCampaignTest(unittest.TestCase):
    """S-029: N arms on one binary with a shared Normal arm and an A/A arm."""
    COHORT = ROOT / 'cohort-ultrafast-v2.json'

    def dry_run(self, arms, *extra):
        out = io.StringIO()
        with contextlib.redirect_stdout(out), mock.patch.object(compare_parallel, 'run_one') as run_one, \
                mock.patch.object(subprocess, 'Popen') as popen:
            code = compare_parallel.main(['--arms', arms, '--cohort', str(self.COHORT), '--repetitions', '3', *extra])
        self.assertEqual(code, 0)
        run_one.assert_not_called()
        popen.assert_not_called()
        return [line for line in out.getvalue().splitlines() if line.startswith('plan: ')], out.getvalue()

    def test_dry_run_prints_the_plan_for_four_and_six_arms_without_a_binary(self):
        cases = len(json.loads(self.COHORT.read_text())['cases'])
        for arms, labels in (('normal,normal,ultrasave,ultrafast', ['normal', 'normal_aa', 'ultrasave', 'ultrafast']),
                             ('normal,normal,ultrasave,ultrafast,ultracode,auto',
                              ['normal', 'normal_aa', 'ultrasave', 'ultrafast', 'ultracode', 'auto'])):
            with self.subTest(arms=arms):
                plan, text = self.dry_run(arms)
                self.assertEqual(len(plan), len(labels) * cases * 3)
                self.assertIn(f'dry run: {len(plan)} runs planned over {len(labels)} arms ({", ".join(labels)})', text)
                self.assertIn('model profile single', text)
                groups = [plan[i:i + len(labels)] for i in range(0, len(plan), len(labels))]
                for group in groups:
                    self.assertEqual(sorted(line.split()[1] for line in group), sorted(labels))
                    self.assertEqual(len({tuple(line.split()[2:6]) for line in group}), 1, 'one case per group')
                leaders = Counter(group[0].split()[1] for group in groups)
                self.assertEqual(set(leaders), set(labels))
                self.assertLessEqual(max(leaders.values()) - min(leaders.values()), 1, 'leading position rotates')
                self.assertIn('"execution_mode": "normal"', plan[0] + plan[1])
                self.assertIn('"worker_model": ""', plan[0], 'the single profile has no worker model')

    def test_tiered_profile_writes_the_ultrasave_model_into_every_arm(self):
        plan, text = self.dry_run('normal,normal,ultrasave,ultrafast', '--ultrasave-model', 'vendor/cheap')
        self.assertIn('model profile tiered', text)
        self.assertTrue(all('"ultrasave_model": "vendor/cheap"' in line for line in plan))
        config = compare_parallel.distill_config(worker_model='', ultrasave_model='vendor/cheap')
        self.assertIn('[modes.ultrasave]\nmodel = "vendor/cheap"', config)
        self.assertNotIn('\nworker = ', config)

    def test_tiered_run_config_records_the_ultrasave_model_and_no_worker(self):
        case = COHORT['cases'][0]
        accounting = {'accounting_complete': True, 'credit_estimate': 1.0, 'calls': 1,
                      'call_usage': [{'complete': True}], 'accounting_evidence': {'complete': True},
                      **{field: 1 for field in compare_parallel.TOKEN_FIELDS}}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            peer = fake_peer(root)
            (root / 'profile').mkdir()
            (root / 'profile' / 'codex-auth.json').write_text('{}')
            with mock.patch.object(compare_parallel, 'distill_accounting', return_value=accounting), \
                    mock.patch.object(compare_parallel, 'run_grader', return_value={
                        'exit_code': 0, 'timed_out': False, 'wall_time_s': .01}):
                result = compare_parallel.run_one(peer, 'ultrasave', case, 1, root / 'runs', root / 'profile', 3,
                    frozen_inputs=compare_parallel.case_hashes(case), binary_hash=compare_parallel._sha256_file(peer),
                    transport='acp', execution_mode='ultrasave', worker_model='', ultrasave_model='vendor/cheap')
            self.assertEqual(result['settings']['configured_modes'], {'ultrasave': {'model': 'vendor/cheap'}})
            self.assertNotIn('worker', result['settings']['configured_models'])
            config = (Path(result['output_dir']) / 'distill-home/config.toml').read_text()
            self.assertIn('[modes.ultrasave]\nmodel = "vendor/cheap"', config)

    def test_invalid_arm_lists_and_paired_flags_are_rejected(self):
        for extra in (['--arms', 'normal'], ['--arms', 'normal,normal,normal'], ['--arms', 'normal,fast'],
                      ['--arms', 'normal,auto,auto'], ['--arms', 'ultrafast,auto'],
                      ['--arms', 'normal,auto', '--baseline-binary', '/a'],
                      ['--arms', 'normal,auto', '--aa-arm'], ['--arms', 'normal,auto', '--verdict', 'ultrafast'],
                      ['--arms', 'normal,auto', '--substantial', 'bench-lru'],
                      ['--arms', 'normal,auto', '--execute', '--binary', '/a', '--work-root', '/w', '--output', '/o']):
            with self.subTest(extra=extra), contextlib.redirect_stderr(io.StringIO()), \
                    mock.patch.object(compare_parallel, 'run_one') as run_one, self.assertRaises(SystemExit) as error:
                compare_parallel.main(['--cohort', str(self.COHORT), *extra])
            self.assertEqual(error.exception.code, 2)
            run_one.assert_not_called()

    def test_normal_verdict_runs_old_against_new_normal_only_with_compare_versions(self):
        base = ['--baseline-binary', '/old', '--candidate-binary', '/new', '--baseline-mode', 'normal',
                '--candidate-mode', 'normal', '--transport', 'acp', '--max-concurrent', '8', '--order-seed', '1',
                '--verdict', 'normal', '--work-root', '/u', '--output', '/u.json']
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(compare_parallel.main(base + ['--compare-versions']), 0)
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as error:
            compare_parallel.main(base)
        self.assertEqual(error.exception.code, 2)

    def test_executed_identical_arms_give_a_valid_neutral_modes_verdict(self):
        from .test_modes_verdict import row as arm_row
        for arms, drift, expected in (('normal,normal', 1.0, 'PASS'), ('normal,normal', 1.2, 'INVALID'),
                                      ('normal,normal,ultrafast,auto', 1.0, None)):
            with self.subTest(arms=arms, drift=drift), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / 'profile').mkdir()
                (root / 'profile' / 'codex-auth.json').write_text('{}')
                seen = []

                def recorded(binary, variant, case, repetition, *args, **kwargs):
                    seen.append((variant, kwargs['execution_mode'], kwargs['worker_model'], kwargs['ultrasave_model']))
                    credits = 10 * (drift if variant == 'normal_aa' else 1)
                    return arm_row(variant, case['id'], repetition, credits=credits,
                                   wall=50.0 if variant == 'ultrafast' else 100.0)

                with mock.patch.object(compare_parallel, 'run_one', side_effect=recorded), \
                        mock.patch.object(compare_parallel, 'subscription_account'), \
                        contextlib.redirect_stdout(io.StringIO()):
                    code = compare_parallel.main([
                        '--arms', arms, '--binary', sys.executable, '--transport', 'acp',
                        '--case', 'bench-semver', '--case', 'bench-lru', '--cohort', str(self.COHORT),
                        '--distill-profile', str(root / 'profile'), '--work-root', str(root / 'runs'),
                        '--output', str(root / 'result.json'), '--execute'])
                result = json.loads((root / 'result.json').read_text())
                labels = compare_parallel.arm_labels(arms.split(','))
                self.assertEqual(result['arms'], labels)
                self.assertEqual(result['model_profile'], 'single')
                self.assertEqual(len(seen), len(labels) * 2 * 3)
                self.assertEqual({entry[1] for entry in seen if entry[0] == 'normal_aa'}, {'normal'})
                self.assertEqual({entry[2:] for entry in seen}, {('', None)})
                self.assertEqual(set(result['binaries']), set(labels))
                report = result['verdict']
                if expected == 'PASS':
                    self.assertEqual((report['verdict'], code), ('PASS', 0), report['reasons'])
                    self.assertAlmostEqual(report['aa_check']['credit_ratio'], 1.0)
                elif expected == 'INVALID':
                    self.assertEqual((report['verdict'], code), ('INVALID', 1))
                else:
                    # Silent failures are unknown only for failing runs; all pass here.
                    self.assertEqual(report['claims']['ultrafast']['status'], 'PASS', report['claims'])
                    self.assertEqual(report['auto']['verdict'], 'PASS')


ACCOUNTED = {'accounting_complete': True, 'credit_estimate': 1.0, 'calls': 1,
             'call_usage': [{'complete': True}], 'accounting_evidence': {'complete': True},
             **{field: 1 for field in compare_parallel.TOKEN_FIELDS}}


def plain_case():
    return next(c for c in COHORT['cases'] if c['id'] == 'seq-rename-en')


def two_prompt_case(**extra):
    return {**plain_case(), 'id': 'two-prompt', 'followup_prompt_ref': 'prompts/par-bugs-en.txt', **extra}


class CaseOptionsTest(unittest.TestCase):
    """Optional case fields of the newer cohorts: follow-up prompt, own timeout, realistic profile."""

    def run_case(self, root, case, **kwargs):
        peer = fake_peer(root)
        profile = root / 'profile'
        profile.mkdir(exist_ok=True)
        (profile / 'codex-auth.json').write_text('{}')
        with mock.patch.object(compare_parallel, 'distill_accounting', return_value=ACCOUNTED), \
                mock.patch.object(compare_parallel, 'run_grader', return_value={
                    'exit_code': 0, 'timed_out': False, 'wall_time_s': .01}):
            return compare_parallel.run_one(peer, 'normal', case, 1, root / 'runs', profile, 3,
                frozen_inputs=compare_parallel.case_hashes(case), binary_hash=compare_parallel._sha256_file(peer),
                transport='acp', execution_mode='auto', **kwargs)

    def write_cohort(self, directory, cases):
        path = Path(directory) / 'cohort.json'
        path.write_text(json.dumps({'schema_version': 1, 'cohort_id': 'options-test', 'cases': cases}))
        return path

    def test_followup_case_sends_its_second_prompt_in_the_same_acp_session(self):
        case = two_prompt_case()
        with tempfile.TemporaryDirectory() as directory:
            result = self.run_case(Path(directory), case)
            self.assertTrue(result['passed'], result['acp'])
            self.assertTrue(result['settings']['followup_prompt'])
            output = Path(result['output_dir'])
            followup = (ROOT / 'prompts/par-bugs-en.txt').read_text()
            self.assertEqual((output / 'followup-prompt.txt').read_text(), followup)
            sent = [json.loads(line)['message'] for line in (output / 'acp-transcript.jsonl').read_text().splitlines()]
            prompts = [m['params']['prompt'][0]['text'] for m in sent if m.get('method') == 'session/prompt']
            self.assertEqual(len(prompts), 2)
            self.assertTrue(prompts[0].startswith((ROOT / case['prompt_ref']).read_text()))
            self.assertEqual(prompts[1], followup)
            self.assertEqual(result['acp']['followup_prompt_receipt']['result'], {'stopReason': 'end_turn'})

    def test_followup_needs_acp_and_is_part_of_the_frozen_inputs(self):
        case = two_prompt_case()
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(subprocess, 'Popen') as popen, \
                self.assertRaisesRegex(ValueError, 'ACP'):
            compare_parallel.run_one(Path(sys.executable), 'normal', case, 1, Path(directory), Path(directory), 3,
                                     frozen_inputs=None, binary_hash=None, transport='cli')
        popen.assert_not_called()
        self.assertEqual(len(compare_parallel.case_hashes(plain_case())), 3)
        self.assertEqual(compare_parallel.case_hashes(case)[:3], compare_parallel.case_hashes(plain_case()))
        self.assertEqual(compare_parallel.case_hashes(case)[3],
                         compare_parallel._sha256_file(ROOT / 'prompts/par-bugs-en.txt'))

    def test_case_timeout_replaces_the_campaign_timeout_and_mixed_budgets_are_rejected(self):
        from .test_modes_verdict import row as arm_row
        long_cases = [{**plain_case(), 'id': 'long-a', 'timeout_s': 5400},
                      {**plain_case(), 'id': 'long-b', 'timeout_s': 5400}]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'profile').mkdir()
            (root / 'profile' / 'codex-auth.json').write_text('{}')
            seen = []

            def recorded(binary, variant, case, repetition, work_root, profile, timeout, **kwargs):
                seen.append(timeout)
                return arm_row(variant, case['id'], repetition, credits=10, wall=100.0)

            with mock.patch.object(compare_parallel, 'run_one', side_effect=recorded), \
                    mock.patch.object(compare_parallel, 'subscription_account'), \
                    contextlib.redirect_stdout(io.StringIO()):
                compare_parallel.main(['--arms', 'normal,auto', '--binary', sys.executable, '--transport', 'acp',
                                       '--cohort', str(self.write_cohort(directory, long_cases)), '--repetitions', '1',
                                       '--timeout', '300', '--distill-profile', str(root / 'profile'),
                                       '--work-root', str(root / 'runs'), '--output', str(root / 'r.json'), '--execute'])
            self.assertEqual(seen, [5400] * 4)
            # Verdicts compare one timeout across runs, so a cohort cannot mix budgets.
            for cases in ([{**plain_case(), 'timeout_s': 5400}, {**plain_case(), 'id': 'other'}],
                          [{**plain_case(), 'timeout_s': 0}], [{**plain_case(), 'timeout_s': True}]):
                with self.subTest(cases=cases), contextlib.redirect_stderr(io.StringIO()), \
                        mock.patch.object(compare_parallel, 'run_one') as run_one, self.assertRaises(SystemExit) as error:
                    compare_parallel.main(['--arms', 'normal,auto', '--cohort', str(self.write_cohort(directory, cases))])
                self.assertEqual(error.exception.code, 2)
                run_one.assert_not_called()
        self.assertEqual(compare_parallel.case_timeout(plain_case(), 300), 300)

    def test_dry_run_shows_case_timeout_and_followup_and_execution_needs_acp(self):
        with tempfile.TemporaryDirectory() as directory:
            cohort = self.write_cohort(directory, [two_prompt_case(), {**plain_case(), 'id': 'single'}])
            out = io.StringIO()
            with contextlib.redirect_stdout(out), mock.patch.object(compare_parallel, 'run_one') as run_one:
                self.assertEqual(compare_parallel.main(['--arms', 'normal,auto', '--cohort', str(cohort),
                                                        '--repetitions', '1', '--timeout', '600']), 0)
            run_one.assert_not_called()
            plan = [line for line in out.getvalue().splitlines() if line.startswith('plan: ')]
            self.assertEqual(len(plan), 4)
            self.assertEqual([line.split()[2] for line in plan if 'followup=prompts/par-bugs-en.txt' in line],
                             ['two-prompt', 'two-prompt'])
            self.assertTrue(all(' timeout_s=600' in line for line in plan))
            with contextlib.redirect_stderr(io.StringIO()), mock.patch.object(compare_parallel, 'run_one') as run_one, \
                    self.assertRaises(SystemExit) as error:
                compare_parallel.main(['--arms', 'normal,auto', '--cohort', str(cohort), '--binary', sys.executable,
                                       '--work-root', directory + '/w', '--output', directory + '/o.json',
                                       '--transport', 'cli', '--execute'])
            self.assertEqual(error.exception.code, 2)
            run_one.assert_not_called()

    def test_realistic_profile_seeds_skills_agents_rules_and_one_mcp_server(self):
        with tempfile.TemporaryDirectory() as directory:
            result = self.run_case(Path(directory), plain_case(), workspace_profile='realistic')
            self.assertTrue(result['passed'], result['acp'])
            self.assertEqual(result['settings']['workspace_profile'], 'realistic')
            output = Path(result['output_dir'])
            home, worktree = output / 'distill-home', output / 'worktree'
            self.assertEqual(len(list((home / 'skills').glob('*/SKILL.md'))), 12)
            # AGENTS.md is frozen task input: committed before the agent starts.
            tracked = subprocess.run(['git', 'ls-files'], cwd=worktree, capture_output=True, text=True).stdout.split()
            self.assertIn('AGENTS.md', tracked)
            config = compare_parallel.tomllib.loads((home / 'config.toml').read_text())
            server = config['mcp_servers'][compare_parallel.REALISTIC_MCP_NAME]
            self.assertEqual(server['command'], 'python3')
            self.assertTrue(Path(server['args'][0]).is_file())
            self.assertTrue(Path(server['args'][0]).is_relative_to(home))
        with tempfile.TemporaryDirectory() as directory:
            result = self.run_case(Path(directory), plain_case())
            output = Path(result['output_dir'])
            self.assertEqual(result['settings']['workspace_profile'], 'plain')
            self.assertFalse((output / 'distill-home' / 'skills').exists())
            self.assertFalse((output / 'worktree' / 'AGENTS.md').exists())
            self.assertNotIn('mcp_servers', (output / 'distill-home' / 'config.toml').read_text())

    def test_realistic_profile_files_join_the_frozen_support_hashes(self):
        cohort = {'cases': [plain_case()]}
        plain = compare_parallel.comparison_hashes(cohort, {})
        realistic = compare_parallel.comparison_hashes(cohort, {}, workspace_profile='realistic')
        added = set(realistic['support_files']) - set(plain['support_files'])
        self.assertIn(str(compare_parallel.REALISTIC_PROFILE / 'AGENTS.md'), added)
        self.assertIn(str(compare_parallel.REALISTIC_PROFILE / 'notes_server.py'), added)

    def test_mcp_stub_lists_and_answers_its_two_tools_over_stdio(self):
        requests = [{'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {'protocolVersion': '2025-06-18'}},
                    {'jsonrpc': '2.0', 'method': 'notifications/initialized'},
                    {'jsonrpc': '2.0', 'id': 2, 'method': 'tools/list'},
                    {'jsonrpc': '2.0', 'id': 3, 'method': 'tools/call',
                     'params': {'name': 'lookup_glossary', 'arguments': {'term': 'SKU'}}},
                    {'jsonrpc': '2.0', 'id': 4, 'method': 'tools/call',
                     'params': {'name': 'count_words', 'arguments': {'text': 'one two  three'}}},
                    {'jsonrpc': '2.0', 'id': 5, 'method': 'resources/list'}]
        run = subprocess.run([sys.executable, str(compare_parallel.REALISTIC_PROFILE / 'notes_server.py')],
                             input=''.join(json.dumps(r) + '\n' for r in requests), capture_output=True, text=True,
                             timeout=10, env={**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'})
        replies = {reply['id']: reply for reply in map(json.loads, run.stdout.splitlines())}
        self.assertEqual(sorted(replies), [1, 2, 3, 4, 5], 'notifications get no reply')
        self.assertEqual(replies[1]['result']['protocolVersion'], '2025-06-18')
        self.assertEqual([tool['name'] for tool in replies[2]['result']['tools']], ['lookup_glossary', 'count_words'])
        self.assertIn('Stock keeping unit', replies[3]['result']['content'][0]['text'])
        self.assertEqual(replies[4]['result']['content'][0]['text'], '3')
        self.assertEqual(replies[5]['error']['code'], -32601)


class DecisionRecordRunTest(unittest.TestCase):
    """S-029/S-030: every run row carries its root session's decision and outcome records."""

    def run_auto(self, root, passed):
        from .test_decision_quality import AUTO_RUN
        case = plain_case()
        peer = fake_peer(root)
        (root / 'profile').mkdir()
        (root / 'profile' / 'codex-auth.json').write_text('{}')

        def accounting(home, session, **_):
            folder = home / 'sessions' / 'project' / session
            folder.mkdir(parents=True)
            (folder / 'events.jsonl').write_text(''.join(json.dumps(event) + '\n' for event in AUTO_RUN))
            return ACCOUNTED
        with mock.patch.object(compare_parallel, 'distill_accounting', side_effect=accounting), \
                mock.patch.object(compare_parallel, 'run_grader', return_value={
                    'exit_code': 0 if passed else 1, 'timed_out': False, 'wall_time_s': .01}):
            return compare_parallel.run_one(peer, 'auto', case, 1, root / 'runs', root / 'profile', 3,
                frozen_inputs=compare_parallel.case_hashes(case), binary_hash=compare_parallel._sha256_file(peer),
                transport='acp', execution_mode='auto', worker_model='')

    def test_passing_and_failing_runs_carry_outcomes_policy_and_decisions(self):
        for passed in (True, False):
            with self.subTest(passed=passed), tempfile.TemporaryDirectory() as directory:
                row = self.run_auto(Path(directory), passed)
                self.assertEqual(row['passed'], passed)
                self.assertTrue(row['decision_events_found'])
                self.assertEqual(len(row['turn_outcomes']), 2)
                self.assertEqual(row['policy_counts'], {'ultrasave:effort_ceiling': 3})
                self.assertEqual(row['decision_events']['b8_execution_mode']['asked'], 2)
                self.assertEqual((row['escalations'], row['failed_check_facts']), (2, 1))
                self.assertEqual(row['first_mode_pick'], 'ultrasave')
                block = compare_parallel.aggregate([row])['auto/all']['decision_quality']
                self.assertEqual(block['levers']['b8_execution_mode']['applied'], {'ultrasave': 1})
                self.assertEqual(block['outcomes']['mode.lean_ended_failing'], 1)
                agreement = block['label_agreement']['mode.lean_ended_failing']
                self.assertEqual(agreement['agreement'], 0.0 if passed else 1.0)

    def test_a_run_without_events_keeps_unknown_fields_absent(self):
        case = plain_case()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            peer = fake_peer(root)
            (root / 'profile').mkdir()
            (root / 'profile' / 'codex-auth.json').write_text('{}')
            with mock.patch.object(compare_parallel, 'distill_accounting', return_value=ACCOUNTED), \
                    mock.patch.object(compare_parallel, 'run_grader', return_value={
                        'exit_code': 1, 'timed_out': False, 'wall_time_s': .01}):
                row = compare_parallel.run_one(peer, 'normal', case, 1, root / 'runs', root / 'profile', 3,
                    frozen_inputs=compare_parallel.case_hashes(case), binary_hash=compare_parallel._sha256_file(peer),
                    transport='acp', execution_mode='normal', worker_model='')
        self.assertFalse(row['decision_events_found'])
        self.assertEqual((row['turn_outcomes'], row['policy_counts'], row['decision_events']), ([], {}, {}))
        self.assertNotIn('escalations', row, 'unknown, so the silent-failure count stays unknown')
        self.assertNotIn('failed_check_facts', row)


if __name__ == '__main__':
    unittest.main()
