import io
import json
import subprocess
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

from .requested_cohort import CASES, FAMILIES, MODES, execute_family, header, main, plan_family


def plan(family='astra', modes=MODES, root=Path('/nonexistent-root'), **extra):
    return plan_family(family, modes=modes, repetitions=3, output_root=root, timeout=300,
                       distill=Path('build/distill'), **extra)


def run_main(*argv):
    out, err = io.StringIO(), io.StringIO()
    with redirect_stdout(out), redirect_stderr(err):
        try:
            code = main(list(argv))
        except SystemExit as exit:
            code = exit.code
    return code, out.getvalue(), err.getvalue()


class PlanTest(unittest.TestCase):
    def test_one_family_runs_every_arm_once_per_task_and_repetition_with_alternating_order(self):
        cells = plan()
        self.assertEqual(len(cells), 3 * len(CASES) * 6)
        groups = [cells[i:i + 6] for i in range(0, len(cells), 6)]
        arms = [('native', None)] + [('distill', mode) for mode in MODES]
        for index, group in enumerate(groups):
            # Adjacent arms share a task and repetition, so drift between arms stays small.
            self.assertEqual({(c['case'], c['repetition']) for c in group},
                             {(CASES[index % 3], index // 3 + 1)})
            order = [(c['arm'], c['mode']) for c in group]
            # Position effects (warm caches, provider load) cancel across groups.
            self.assertEqual(order, arms if index % 2 == 0 else arms[::-1])
        self.assertEqual(len({c['output'] for c in cells}), len(cells))
        for cell in cells:
            command = cell['command']
            spec = FAMILIES['astra'][cell['arm']]
            self.assertEqual(command[3:3 + len(spec)], spec)
            self.assertEqual(command[-6:], ['--case', cell['case'], '--output', str(cell['output']), '--timeout', '300'])
            if cell['arm'] == 'distill':
                self.assertEqual(command[command.index('--mode') + 1], cell['mode'])
                self.assertEqual(command[command.index('--distill') + 1], 'build/distill')
            else:
                self.assertNotIn('--mode', command)

    def test_every_family_keeps_its_published_arms_and_credential_flags(self):
        for family, spec in FAMILIES.items():
            cells = plan(family, claude_auth=Path('/secret/claude-auth.json'))
            self.assertEqual(len(cells), 54)
            for cell in cells:
                self.assertEqual('--claude-auth' in cell['command'], family == 'opus')
        self.assertTrue(all(case in {c['id'] for c in json.loads(
            (Path(__file__).parent / 'cohort-v1.json').read_text())['cases']} for case in CASES))

    def test_plan_for_all_families_prints_216_cells_and_writes_nothing(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            code, out, _ = run_main('--results-dir', str(root / 'results'), '--output-root', str(root / 'cells'),
                                    '--date', '20990101')
            self.assertEqual(code, 0)
            self.assertEqual(sum(1 for line in out.splitlines() if not line.startswith('#')), 216)
            self.assertEqual(list(root.iterdir()), [])
            code, out, _ = run_main('--families', 'opus', '--modes', 'ultrasave,normal', '--repetitions', '1',
                                    '--results-dir', str(root), '--output-root', str(root / 'cells'))
            cells = [line for line in out.splitlines() if not line.startswith('#')]
            self.assertEqual((code, len(cells)), (0, 9))
            self.assertIn('--mode normal', cells[1])  # modes keep the canonical arm order
            for argv in (['--modes', 'turbo'], ['--families', 'gpt'], ['--modes', 'auto,auto'],
                         ['--repetitions', '0']):
                self.assertEqual(run_main(*argv, '--results-dir', str(root))[0], 2)

    def test_existing_result_or_cell_directory_is_never_overwritten(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            existing = root / 'requested-20990101-sol.json'
            existing.write_text('{"kept": true}\n')
            code, out, err = run_main('--families', 'sol', '--date', '20990101', '--results-dir', str(root),
                                      '--output-root', str(root / 'cells'))
            self.assertEqual((code, out), (2, ''))
            self.assertIn(str(existing), err)
            cells = plan('sol', root=root / 'cells')
            cells[5]['output'].mkdir(parents=True)
            code, _, err = run_main('--families', 'sol', '--date', '20990102', '--results-dir', str(root),
                                    '--output-root', str(root / 'cells'))
            self.assertEqual(code, 2)
            self.assertIn(str(cells[5]['output']), err)
            calls = []
            with self.assertRaises(FileExistsError):
                execute_family(cells, {}, existing, run=lambda *a, **k: calls.append(a))
            self.assertEqual((calls, existing.read_text()), ([], '{"kept": true}\n'))


class ExecuteTest(unittest.TestCase):
    def test_each_cell_runs_once_and_failures_stay_in_the_result(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cells = plan('opus', modes=('normal', 'ultrafast'), root=root / 'cells',
                         claude_auth=Path.home() / 'private/claude-auth.json')
            calls = []

            def fake_runner(command, **kwargs):
                # The third cell crashes before writing a result; the fourth times out
                # and leaves a credential behind. Neither is retried.
                calls.append(command)
                index = len(calls)
                output = Path(command[command.index('--output') + 1])
                output.mkdir(parents=True)
                if index != 3:
                    (output / 'result.json').write_text(json.dumps(
                        {'session': 's', 'timed_out': index == 4, 'grader_passed': index != 4}))
                    (output / 'command.json').write_text('["distill"]')
                if index == 4:
                    (output / 'isolated-home').mkdir()
                    (output / 'isolated-home/claude-auth.json').write_text('{}')
                return subprocess.CompletedProcess(command, 1 if index in (3, 4) else 0, '', 'boom' if index == 3 else '')

            path = root / 'requested-20990101-opus.json'
            record = header('opus', cells, date='20990101', modes=('normal', 'ultrafast'), repetitions=3,
                            timeout=300, output_root=root / 'cells')
            execute_family(cells, record, path, run=fake_runner)
            self.assertEqual(calls, [cell['command'] for cell in cells])
            saved = json.loads(path.read_text())
            self.assertEqual((saved['status'], saved['executed_executions'], len(saved['runs'])), ('complete', 27, 27))
            self.assertFalse(saved['outcome_based_reruns'])
            crashed, timed_out = saved['runs'][2], saved['runs'][3]
            self.assertEqual((crashed['runner_exit_code'], crashed['result'], crashed['driver_stderr']), (1, None, 'boom'))
            self.assertTrue(timed_out['result']['timed_out'])
            self.assertFalse(timed_out['credentials_cleaned'])
            self.assertTrue(saved['runs'][0]['credentials_cleaned'])
            self.assertEqual([(r['arm'], r['mode']) for r in saved['runs'][:3]],
                             [('native', None), ('distill', 'normal'), ('distill', 'ultrafast')])
            command = saved['runs'][0]['command']
            self.assertEqual(command[command.index('--claude-auth') + 1], '<private-credential-path>')
            self.assertNotIn(str(Path.home()), path.read_text())
            self.assertEqual([a['mode'] for a in saved['requested_arms']], [None, 'normal', 'ultrafast'])


if __name__ == '__main__':
    unittest.main()
