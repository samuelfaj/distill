import contextlib
import io
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from . import compare_parallel
from .parallel_verdict import verdict

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

    def test_aggregate_groups_by_variant_and_kind(self):
        rows = [dict(run('baseline', 'd', 'divisible', 4.0), model_calls=3, **{f: 1 for f in compare_parallel.TOKEN_FIELDS}),
                dict(run('baseline', 's', 'sequential', 2.0, passed=False), model_calls=1,
                     **{f: 1 for f in compare_parallel.TOKEN_FIELDS})]
        agg = compare_parallel.aggregate(rows)
        self.assertEqual(agg['baseline/all']['credits_per_accepted'], 6.0)
        self.assertEqual((agg['baseline/sequential']['accepted'], agg['baseline/sequential']['credits_per_accepted']),
                         (0, None))
        self.assertEqual(agg['baseline/all']['model_calls'], 4)


if __name__ == '__main__':
    unittest.main()
