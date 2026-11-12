"""Fixture proofs for the mode cohorts (auto, long, ultracode-hard): seeded fails, reference passes.

Graders and references are never copied into the task workspace; JavaScript
cases need Node.js on PATH, exactly as their graders do.
"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parent
MODES = ('normal', 'ultrasave', 'ultrafast', 'ultracode')
CASE_KEYS = ('id', 'kind', 'size', 'prompt_ref', 'fixture_ref', 'grader', 'reference_ref')


def load(name):
    return json.loads((ROOT / name).read_text())


def stage(case, workspace, *layers):
    """Copy the fixture, then each overlay directory on top, into the workspace slice."""
    target = Path(workspace) / 'tools/task_cost_eval' / case['fixture_ref']
    shutil.copytree(ROOT / case['fixture_ref'], target)
    for layer in layers:
        shutil.copytree(ROOT / layer, target, dirs_exist_ok=True)
    if (Path(workspace) / 'tools/task_cost_eval/reference').exists() or \
            (Path(workspace) / 'tools/task_cost_eval/graders').exists():
        raise AssertionError('private artifacts leaked into workspace')
    return target


def run_grader(case, *layers):
    with tempfile.TemporaryDirectory(prefix='mode-cohort-proof-') as tmp:
        stage(case, tmp, *layers)
        env = {**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'}
        return subprocess.run([sys.executable, str(ROOT / case['grader']['script_ref']), tmp],
                              cwd=tmp, env=env, capture_output=True, text=True, timeout=60)


def run_visible_tests(directory):
    """The test command a user of the fixture would run: unittest for Python, node --test for JavaScript."""
    env = {**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'}
    if any(Path(directory).glob('*.test.js')):
        return subprocess.run(['node', '--test'], cwd=directory, env=env, capture_output=True, text=True, timeout=60)
    return subprocess.run([sys.executable, '-m', 'unittest', 'discover'], cwd=directory, env=env,
                          capture_output=True, text=True, timeout=60)


def uses_node(case):
    return 'run_node' in (ROOT / case['grader']['script_ref']).read_text()


class CohortProofs:
    """Mixin: shape, seeded/reference proofs and visible tests for one cohort file."""
    COHORT = None
    SIZE = None

    @classmethod
    def cases(cls):
        return load(cls.COHORT)['cases']

    def own_cases(self):
        # Cases reused from older cohorts are proven by those cohorts' own tests.
        return [case for case in self.cases() if not case.get('reused')]

    def test_shape(self):
        seen = set()
        cases = self.cases()
        self.assertTrue(self.SIZE[0] <= len(cases) <= self.SIZE[1], len(cases))
        for case in cases:
            for key in CASE_KEYS:
                self.assertIn(key, case, case.get('id'))
            for key in ('fixture_ref', 'reference_ref'):
                self.assertTrue((ROOT / case[key]).is_dir(), (case['id'], key))
            for key in ('prompt_ref', 'followup_prompt_ref'):
                if key in case:
                    self.assertTrue((ROOT / case[key]).is_file(), (case['id'], key))
            self.assertTrue((ROOT / case['grader']['script_ref']).is_file(), case['id'])
            self.assertNotIn(case['id'], seen)
            seen.add(case['id'])
        self.assertIn('pilot', load(self.COHORT)['description'], 'unconfirmed design targets must say so')

    def test_node_is_available_for_javascript_cases(self):
        if any(uses_node(case) for case in self.own_cases()):
            self.assertIsNotNone(shutil.which('node'), 'JavaScript cases need Node.js on PATH')

    def test_seeded_fails_and_reference_passes(self):
        for case in self.own_cases():
            with self.subTest(case=case['id']):
                seeded = run_grader(case)
                self.assertEqual(seeded.returncode, 1, case['id'] + ': seeded grader did not fail\n' + seeded.stderr)
                self.assertNotIn('Traceback', seeded.stderr, case['id'] + ': grader crashed on seed')
                self.assertNotIn('import:', seeded.stderr, case['id'] + ': seed did not import')
                reference = run_grader(case, case['reference_ref'])
                self.assertEqual(reference.returncode, 0, case['id'] + ': reference failed\n' + reference.stderr)
                self.assertEqual(reference.stderr, '', case['id'] + ': reference emitted diagnostics')

    def test_reference_visible_tests(self):
        for case in self.own_cases():
            with self.subTest(case=case['id']):
                result = run_visible_tests(ROOT / case['reference_ref'])
                self.assertEqual(result.returncode, 0, case['id'] + ': visible tests failed\n' + result.stderr[-2000:])


class AutoCohortTests(CohortProofs, unittest.TestCase):
    COHORT = 'cohort-auto-v1.json'
    SIZE = (10, 14)

    def test_expected_modes_cover_every_target_and_two_prompt_sessions_exist(self):
        cases = self.cases()
        for case in cases:
            self.assertIn(case['expected_mode'], MODES, case['id'])
            self.assertEqual('followup_prompt_ref' in case, 'expected_followup_mode' in case, case['id'])
        picks = [case['expected_mode'] for case in cases] + [
            case['expected_followup_mode'] for case in cases if 'expected_followup_mode' in case]
        for mode in MODES:
            self.assertGreaterEqual(picks.count(mode), 3, mode)
        two_prompt = [case for case in cases if 'followup_prompt_ref' in case]
        self.assertGreaterEqual(len(two_prompt), 3)
        # The follow-up changes the request class, so the expected mode changes with it.
        for case in two_prompt:
            self.assertNotEqual(case['expected_mode'], case['expected_followup_mode'], case['id'])

    def test_two_prompt_graders_need_the_followup_work(self):
        # Doing only the first request must not pass: the grade is taken after the second prompt.
        first_only = {'auto-save-then-plan-retry': {'config.py': 'RETRY_ATTEMPTS = 5\nBACKOFF_S = 0.5\n'}}
        for case in self.cases():
            if case['id'] not in first_only:
                continue
            with self.subTest(case=case['id']), tempfile.TemporaryDirectory() as overlay:
                for name, text in first_only[case['id']].items():
                    (Path(overlay) / name).write_text(text)
                self.assertEqual(run_grader(case, overlay).returncode, 1)


if __name__ == '__main__':
    unittest.main()
