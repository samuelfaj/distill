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


def has_visible_tests(case):
    directory = ROOT / case['fixture_ref']
    return any(directory.glob('*.test.js')) or any(directory.glob('test_*.py'))


def run_visible_tests(case, *layers):
    """The fixture's own test command on a staged copy: unittest for Python, node --test for JavaScript."""
    env = {**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'}
    with tempfile.TemporaryDirectory(prefix='mode-cohort-visible-') as tmp:
        directory = stage(case, tmp, *layers)
        if any(directory.glob('*.test.js')):
            command = ['node', '--test']
        else:
            command = [sys.executable, '-m', 'unittest', 'discover']
        return subprocess.run(command, cwd=directory, env=env, capture_output=True, text=True, timeout=60)


def uses_node(case):
    return 'run_node' in (ROOT / case['grader']['script_ref']).read_text()


class CohortProofs:
    """Mixin: shape, seeded/reference proofs and visible tests for one cohort file."""
    COHORT = None
    SIZE = None

    @classmethod
    def cases(cls):
        return load(cls.COHORT)['cases']

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
        if any(uses_node(case) for case in self.cases()):
            self.assertIsNotNone(shutil.which('node'), 'JavaScript cases need Node.js on PATH')

    def test_seeded_fails_and_reference_passes(self):
        for case in self.cases():
            with self.subTest(case=case['id']):
                seeded = run_grader(case)
                self.assertEqual(seeded.returncode, 1, case['id'] + ': seeded grader did not fail\n' + seeded.stderr)
                self.assertNotIn('Traceback', seeded.stderr, case['id'] + ': grader crashed on seed')
                self.assertNotIn('import:', seeded.stderr, case['id'] + ': seed did not import')
                reference = run_grader(case, case['reference_ref'])
                self.assertEqual(reference.returncode, 0, case['id'] + ': reference failed\n' + reference.stderr)
                self.assertEqual(reference.stderr, '', case['id'] + ': reference emitted diagnostics')

    def test_reference_visible_tests(self):
        for case in self.cases():
            if not has_visible_tests(case):
                continue  # Older reused controls ship without a local test command.
            with self.subTest(case=case['id']):
                result = run_visible_tests(case, case['reference_ref'])
                self.assertEqual(result.returncode, 0, case['id'] + ': visible tests failed\n' + result.stderr[-2000:])


class HardCohortProofs(CohortProofs):
    """The difficulty claim made testable: the obvious change passes the visible tests and fails the grader."""
    SIZE = (5, 7)

    def hard_cases(self):
        return [case for case in self.cases() if case['difficulty'] == 'hard']

    def test_difficulty_classes(self):
        cases = self.cases()
        classes = [case['difficulty'] for case in cases]
        self.assertGreaterEqual(classes.count('hard'), 4)
        self.assertEqual(classes.count('control-sequential'), 1)
        self.assertEqual(classes.count('control-small'), 1)
        for case in cases:
            self.assertEqual(case['size'], 'substantial' if case['difficulty'] == 'hard' else 'control', case['id'])
            self.assertEqual('obvious_ref' in case, case['difficulty'] == 'hard', case['id'])
        for case in self.hard_cases():
            self.assertTrue((ROOT / case['obvious_ref']).is_dir(), case['id'])

    def test_obvious_change_passes_visible_tests_but_fails_the_grader(self):
        for case in self.hard_cases():
            with self.subTest(case=case['id']):
                visible = run_visible_tests(case, case['reference_ref'], case['obvious_ref'])
                self.assertEqual(visible.returncode, 0, case['id'] + ': obvious change fails the visible tests, '
                                 'so it is not the trap the case claims\n' + visible.stderr[-2000:])
                graded = run_grader(case, case['reference_ref'], case['obvious_ref'])
                self.assertEqual(graded.returncode, 1, case['id'] + ': grader accepted the obvious change')
                self.assertNotIn('Traceback', graded.stderr)
                self.assertNotIn('import:', graded.stderr)

    def test_obvious_change_differs_from_the_reference(self):
        for case in self.hard_cases():
            with self.subTest(case=case['id']):
                for path in (ROOT / case['obvious_ref']).rglob('*'):
                    if path.is_file():
                        original = ROOT / case['reference_ref'] / path.relative_to(ROOT / case['obvious_ref'])
                        if not original.exists():
                            original = ROOT / case['fixture_ref'] / path.relative_to(ROOT / case['obvious_ref'])
                        self.assertTrue(original.exists(), path)


class UltraCodeHardTests(HardCohortProofs, unittest.TestCase):
    COHORT = 'cohort-ultracode-hard-v1.json'

    def test_ingestion_case_reuses_the_existing_fixture(self):
        ingestion = [case for case in self.cases() if case['fixture_ref'] == 'fixtures/ultra_dev_ingestion']
        self.assertEqual(len(ingestion), 1)
        existing = next(case for case in load('cohort-ultracode-dev-v1.json')['cases'] if case['id'] == 'ultra-dev-d2')
        for key in ('prompt_ref', 'fixture_ref', 'grader', 'reference_ref'):
            self.assertEqual(ingestion[0][key], existing[key])


class UltraCodeHardHeldoutTests(HardCohortProofs, unittest.TestCase):
    COHORT = 'cohort-ultracode-hard-heldout-v1.json'

    def test_halves_are_disjoint_and_together_hold_ten_to_twelve_cases(self):
        development = load(UltraCodeHardTests.COHORT)['cases']
        heldout = self.cases()
        self.assertTrue(10 <= len(development) + len(heldout) <= 12)
        self.assertEqual({c['id'] for c in development} & {c['id'] for c in heldout}, set())
        self.assertEqual({c['fixture_ref'] for c in development} & {c['fixture_ref'] for c in heldout}, set())
        count = lambda cases, kind: sum(case['difficulty'] == kind for case in cases)
        for kind in ('hard', 'control-sequential', 'control-small'):
            self.assertEqual(count(development, kind), count(heldout, kind), kind)


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


# Per-file check of each long task: (directory, file glob, command before the file path).
PER_FILE_CHECKS = {
    'fixtures/long_handler_migration': ('handlers', '[!_]*.py', [sys.executable, 'check.py']),
    'fixtures/long_clean_pipeline': ('normalizers', '[!_]*.py', [sys.executable, 'check.py']),
    'fixtures/long_money_rename': ('billing', '[!_]*.py', [sys.executable, 'check.py']),
    'fixtures/long_cjs_to_esm': ('lib', '*.js', ['node', 'check.mjs']),
}


def per_file_results(case, *layers, limit=None):
    """Run the task's own per-file check on every target file of a staged copy; {file: returncode}."""
    directory, pattern, command = PER_FILE_CHECKS[case['fixture_ref']]
    with tempfile.TemporaryDirectory(prefix='mode-cohort-check-') as tmp:
        target = stage(case, tmp, *layers)
        files = sorted((target / directory).glob(pattern))[:limit]
        env = {**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'}
        return {path.name: subprocess.run([*command, f'{directory}/{path.name}'], cwd=target, env=env,
                                          capture_output=True, text=True, timeout=60).returncode for path in files}


class LongCohortTests(CohortProofs, unittest.TestCase):
    COHORT = 'cohort-long-v1.json'
    SIZE = (4, 6)

    def test_every_case_shares_one_long_timeout(self):
        from .compare_parallel import cohort_timeout_error
        cases = self.cases()
        self.assertIsNone(cohort_timeout_error(cases))
        self.assertGreaterEqual(min(case['timeout_s'] for case in cases), 3600)
        self.assertTrue(all(case['rounds_target'] >= 150 for case in cases))

    def test_reference_visible_tests(self):
        # The visible check of a long task is per file; every reference file passes it.
        for case in self.cases():
            with self.subTest(case=case['id']):
                results = per_file_results(case, case['reference_ref'])
                self.assertGreaterEqual(len(results), 40, 'a long task spreads over at least 40 files')
                self.assertEqual({name for name, code in results.items() if code}, set())

    def test_per_file_check_fails_on_untouched_files(self):
        # Every target file starts out failing its check, so no file can be skipped.
        for case in self.cases():
            with self.subTest(case=case['id']):
                results = per_file_results(case, limit=6)
                self.assertEqual({name for name, code in results.items() if code == 0}, set())

    def test_one_file_left_unconverted_fails_the_grader(self):
        for case in self.cases():
            directory, pattern, _ = PER_FILE_CHECKS[case['fixture_ref']]
            last = sorted((ROOT / case['fixture_ref'] / directory).glob(pattern))[-1]
            with self.subTest(case=case['id']), tempfile.TemporaryDirectory() as overlay:
                (Path(overlay) / directory).mkdir()
                shutil.copyfile(last, Path(overlay) / directory / last.name)
                result = run_grader(case, case['reference_ref'], overlay)
                self.assertEqual(result.returncode, 1, case['id'])
                self.assertNotIn('Traceback', result.stderr)
                # The old handler module cannot even import once register() is gone.
                self.assertTrue(last.stem in result.stderr or "import name 'register'" in result.stderr, result.stderr)

    def test_generator_reproduces_the_committed_files(self):
        with tempfile.TemporaryDirectory(prefix='long-fixtures-') as tmp:
            run = subprocess.run([sys.executable, '-B', '-m', 'tools.task_cost_eval.generate_long_fixtures',
                                  '--root', tmp], cwd=ROOT.parent.parent, capture_output=True, text=True, timeout=300,
                                 env={**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'})
            self.assertEqual(run.returncode, 0, run.stderr)
            generated = {path.relative_to(tmp): path.read_bytes() for path in Path(tmp).rglob('*') if path.is_file()}
            committed = {}
            for case in self.cases():
                name = Path(case['fixture_ref']).name
                for kind in ('fixtures', 'reference'):
                    for path in (ROOT / kind / name).rglob('*'):
                        if path.is_file():
                            committed[path.relative_to(ROOT)] = path.read_bytes()
                data = ROOT / 'graders' / f'{name}_expected.json'
                committed[data.relative_to(ROOT)] = data.read_bytes()
            self.assertEqual(sorted(generated), sorted(committed))
            self.assertEqual([path for path in generated if generated[path] != committed[path]], [])


if __name__ == '__main__':
    unittest.main()
