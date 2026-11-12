import hashlib
import io
import json
import re
import shutil
import tempfile
import unittest
from contextlib import redirect_stderr
from pathlib import Path

from .compare_codex import MODE_LABELS
from .render_benchmarks import BEGIN, END, README_MODES, main, readme_markers, render
from .requested_cohort import FAMILIES
from .requested_summary import build
from .test_requested_summary import family, run, synthetic

REPO = Path(__file__).resolve().parents[2]
SUMMARY = REPO / 'tools/task_cost_eval/results/requested-20261016-summary.json'
SUMMARY_LINK = 'tools/task_cost_eval/results/requested-20261016-summary.json'
# SHA-256 of both documents before the markers existed.
README_SHA = 'd847f8b197c6ce1d00e258de0eaae23e53a85c78bcdfc09c2881881cf8181a8a'
BENCHMARKS_SHA = '7703493baf5c96481a54e60f261b258c0d81dabc9d312dff98aeb55c161a2170'
MARKER = re.compile(r'<!-- benchmarks:[a-z]*:?(begin|end) -->\n')
# Reproduction lines added with the per-mode tooling (the only visible additions allowed).
ADDED = ('requested_cohort', 'requested_summary', 'render_benchmarks', ' --mode ', '# Per-mode cohorts',
         '# --mode and ran Normal', '# The cohort driver', '# alternating arm order')


def strip(text, added=()):
    return ''.join(line for line in text.splitlines(True)
                   if not MARKER.fullmatch(line) and not any(token in line for token in added))


def region(text, begin, end):
    return text[text.index(begin) + len(begin) + 1:text.index(end)]


def per_mode_summary(directory):
    """Astra: every arm, UltraFast fails the gate, Auto lost a receipt. DeepSeek: no UltraFast arm."""
    runs = synthetic() + [{**r, 'mode': 'ultracode', 'result': {**r['result'], 'execution_mode': 'ultracode'}}
                          for r in synthetic() if r['mode'] == 'normal']
    deepseek = [r for r in synthetic() if r['mode'] not in ('ultrafast', 'auto')]
    deepseek += [run('distill', 'auto', cost=.4, ticks=[4 * 10**9], wall=5) for _ in range(3)]
    return build([family(Path(directory) / 'astra.json', runs, 'astra'),
                  family(Path(directory) / 'deepseek.json', deepseek, 'deepseek')])


class RoundTripTest(unittest.TestCase):
    def test_committed_documents_are_what_the_committed_summary_renders(self):
        summary = json.loads(SUMMARY.read_text())
        benchmarks, readme = (REPO / 'BENCHMARKS.md').read_text(), (REPO / 'README.md').read_text()
        self.assertEqual(render(summary, SUMMARY_LINK, benchmarks, readme), (benchmarks, readme))
        self.assertIn('| DeepSeek |', region(benchmarks, BEGIN, END))
        self.assertEqual(main(['--summary', str(SUMMARY), '--benchmarks', str(REPO / 'BENCHMARKS.md'),
                               '--readme', str(REPO / 'README.md'), '--check']), 0)

    def test_markers_change_no_visible_character(self):
        # Hidden comments only; BENCHMARKS.md also gains the per-mode reproduction commands.
        readme, benchmarks = (REPO / 'README.md').read_text(), (REPO / 'BENCHMARKS.md').read_text()
        self.assertEqual(hashlib.sha256(strip(readme).encode()).hexdigest(), README_SHA)
        self.assertEqual(hashlib.sha256(strip(benchmarks, ADDED).encode()).hexdigest(), BENCHMARKS_SHA)
        self.assertEqual(len(MARKER.findall(readme)), 2 * len(README_MODES))

    def test_check_fails_on_drift_and_a_render_restores_only_the_regions(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            benchmarks, readme = root / 'BENCHMARKS.md', root / 'README.md'
            shutil.copyfile(REPO / 'BENCHMARKS.md', benchmarks)
            shutil.copyfile(REPO / 'README.md', readme)
            original = benchmarks.read_text()
            drifted = original.replace('$1.00374', '$0.90374').replace('## Cohort interpretation', '## Cohorts')
            benchmarks.write_text(drifted)
            argv = ['--summary', str(SUMMARY), '--benchmarks', str(benchmarks), '--readme', str(readme)]
            with redirect_stderr(io.StringIO()) as err:
                self.assertEqual(main(argv + ['--check']), 1)
            self.assertIn('BENCHMARKS.md', err.getvalue())
            self.assertEqual(benchmarks.read_text(), drifted)  # --check never writes
            self.assertEqual(main(argv), 0)
            self.assertEqual(benchmarks.read_text(), original.replace('## Cohort interpretation', '## Cohorts'))
            readme.write_text(readme.read_text().replace(readme_markers('auto')[1], ''))
            with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                main(argv)


class PerModeTest(unittest.TestCase):
    def rendered(self):
        with tempfile.TemporaryDirectory() as directory:
            summary = per_mode_summary(directory)
        benchmarks, readme = render(summary, 'results/s.json', (REPO / 'BENCHMARKS.md').read_text(),
                                    (REPO / 'README.md').read_text())
        return summary, region(benchmarks, BEGIN, END), {m: region(readme, *readme_markers(m)) for m in README_MODES}

    def mode_rows(self, text):
        rows = [line.split(' | ') for line in text.splitlines() if line.startswith('| ') and ' (' in line.split(' | ')[0]]
        return [row for row in rows if len(row) == 13 and row[1] in MODE_LABELS.values()]

    def test_no_claim_rows_and_cells_carry_a_reason_never_a_number(self):
        _, benchmarks, readme = self.rendered()
        rows = {(row[0], row[1]): row for row in self.mode_rows(benchmarks)}
        astra, deepseek = (f'| {FAMILIES[f]["label"]} ({FAMILIES[f]["models"]})' for f in ('astra', 'deepseek'))
        failed, missing = rows[(astra, 'UltraFast')], rows[(deepseek, 'UltraFast')]
        self.assertEqual(failed[3], 'no claim yet (quality gate: 1/3 successes against Normal 3/3 with tolerance 0)')
        self.assertEqual(missing[3], 'no claim yet (no result)')
        for row in (failed, missing):
            self.assertEqual(row[4:12], ['—'] * 8)
        # UltraFast has no claim anywhere; Auto is claimed only where its cost is a point estimate.
        self.assertTrue(readme['ultrafast'].startswith('# No claim yet\n'))
        self.assertIn('No claim for Astra: quality gate', readme['ultrafast'])
        self.assertIn('No claim for DeepSeek: no result', readme['ultrafast'])
        self.assertNotRegex(readme['ultrafast'].split('Source:')[0], r'\d+(\.\d+)?x|\d+%')
        self.assertIn('No claim for Astra: cost against Normal is only a bound', readme['auto'])
        self.assertTrue(readme['auto'].startswith('# 20% cheaper, 17% less time\n'))  # DeepSeek: 1.2/1.5 cost, 15/18 time

    def test_every_claim_states_family_models_pinning_result_file_and_version(self):
        summary, benchmarks, readme = self.rendered()
        for row in self.mode_rows(benchmarks):
            family = next(f for f, spec in FAMILIES.items() if row[0] == f'| {spec["label"]} ({spec["models"]})')
            self.assertEqual(row[2], 'under Auto' if row[1] == 'Auto' else 'mode pinned')
            self.assertIn(f'({summary["pairs"][family]["source"]})', row[12])
            if not row[3].startswith('no claim yet'):
                self.assertIn('distill 1.0 (abc123)', row[12])
        claimed = {'ultrasave': ('astra', 'deepseek'), 'ultracode': ('astra',), 'auto': ('deepseek',)}
        for mode, families in claimed.items():
            text = readme[mode]
            self.assertFalse(text.startswith('# No claim yet'), mode)
            for name in families:
                self.assertIn(f'{FAMILIES[name]["label"]} ({FAMILIES[name]["models"]})', text.split(' No claim for ')[0])
            self.assertIn('under Auto' if mode == 'auto' else 'with the mode pinned', text)
            self.assertIn('[summary](results/s.json)', text)
            self.assertIn('distill 1.0 (abc123)', text)
        self.assertTrue(readme['ultrasave'].startswith('# 50% cheaper\n'))
        self.assertIn('**0.5x the estimated cost of Normal**', readme['ultrasave'])
        self.assertTrue(readme['ultracode'].startswith('# 3/3 passed\n'))  # only Astra has an UltraCode arm
        self.assertIn('No claim for DeepSeek: no result', readme['ultracode'])

    def test_numbers_follow_the_summary_and_nothing_claims_every_task_passed(self):
        summary, benchmarks, readme = self.rendered()
        for text in (benchmarks, *readme.values()):
            self.assertNotRegex(text, r'(?i)\ball\b[^.|]*\b(tasks?|runs?)\b[^.|]*\bpassed')
        save = summary['pairs']['astra']['arms']['ultrasave']
        save['reference_cost_usd'] = .375  # the README ratio follows the summary, not the documents
        _, readme_text = render(summary, 'results/s.json', (REPO / 'BENCHMARKS.md').read_text(),
                                (REPO / 'README.md').read_text())
        self.assertIn('**0.25x–0.5x the estimated cost of Normal**', region(readme_text, *readme_markers('ultrasave')))
        self.assertIn('# 50–75% cheaper', region(readme_text, *readme_markers('ultrasave')))


if __name__ == '__main__':
    unittest.main()
