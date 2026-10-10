"""External self-tests: graders/references are never copied into task workspaces."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parent
PROOF = {}
MUTATIONS = {
    'ultra-dev-d1': [
        ('revenue.py', "row['units'] += item['shipped']", "row['units'] += item['quantity']"),
        ('backlog.py', "if order['status'] != 'open':", "if order['status'] == 'shipped':"),
        ('aging.py', 'age <= 7', 'age < 7'),
    ],
    'ultra-dev-d2': [
        ('api.py', "if format == 'csv':", "if format == 'disabled':"),
        ('jsonl_feed.py', 'enumerate(text.splitlines(), 1)', 'enumerate(text.splitlines(), 0)'),
        ('aggregate.py', 'if record != seen[record[\'id\']]:', 'if False:'),
    ],
    'ultra-dev-d3': [
        ('service.py', 'self.catalog.revision, self.rates.revision,', '0, 0,'),
        ('cache.py', 'return copy.deepcopy(self.values.get(key))', 'return self.values.get(key)'),
        ('catalog.py', 'self.entries = copy.deepcopy(entries)', 'self.entries = dict(entries)'),
    ],
    'ultra-heldout-h1': [
        ('index.py', 'counts[token] += 2', 'counts[token] += 1'),
        ('search.py', 'all(term in counts for term in terms)', 'any(term in counts for term in terms)'),
        ('filters.py', "(include_archived or not document['archived'])", 'True'),
    ],
    'ultra-heldout-h2': [
        ('api.py', 'for text in json_texts:', 'for text in reversed(list(json_texts)):'),
        ('merge.py', 'original[section][key] if leaf is None else leaf', 'base[section][key] if leaf is None else leaf'),
        ('export.py', "'true' if config['features']['debug'] else 'false'", "'True' if config['features']['debug'] else 'False'"),
    ],
    'ultra-heldout-h3': [
        ('projector.py', "if ledger.seen[event['id']] != event:", 'if False:'),
        ('projector.py', 'balances = dict(ledger.balances)', 'balances = ledger.balances'),
        ('journal.py', 'return copy.deepcopy(ledger.journal)', 'return ledger.journal'),
    ],
    'ultra-heldout-h4': [
        ('query.py', 'for key,value in sorted(values)', 'for key,value in values'),
        ('query.py', 'key.casefold() in keys', 'key in keys'),
    ],
}


def cases():
    return [case for phase in ('dev', 'heldout')
            for case in json.loads((ROOT / f'cohort-ultracode-{phase}-v1.json').read_text())['cases']]


def output_proof(result):
    return {'returncode': result.returncode,
            'stdout_sha256': hashlib.sha256(result.stdout.encode()).hexdigest(),
            'stderr_sha256': hashlib.sha256(result.stderr.encode()).hexdigest(),
            'diagnostic_lines': len(result.stderr.splitlines())}


def run_grader(case, source, mutation=None):
    with tempfile.TemporaryDirectory(prefix='ultracode-fixture-proof-') as tmp:
        workspace = Path(tmp)
        target = workspace / 'tools/task_cost_eval' / case['fixture_ref']
        shutil.copytree(ROOT / source, target)
        if mutation:
            filename, before, after = mutation
            path = target / filename
            content = path.read_text()
            if content.count(before) != 1:
                raise AssertionError('mutation must match exactly one site')
            path.write_text(content.replace(before, after))
            for script in target.glob('*.py'):
                compile(script.read_text(), str(script), 'exec')
        # Only the repository slice is visible to the evaluated workspace.
        if (workspace / 'tools/task_cost_eval/reference').exists() or (workspace / 'tools/task_cost_eval/graders').exists():
            raise AssertionError('private artifacts leaked into workspace')
        env = {**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'}
        return subprocess.run([sys.executable, str(ROOT / case['grader']['script_ref']), str(workspace)],
                              cwd=workspace, env=env, capture_output=True, text=True, timeout=30)


class UltraCodeFixtureTests(unittest.TestCase):
    def test_seeded_fail_reference_pass(self):
        for case in cases():
            with self.subTest(case=case['id']):
                seeded = run_grader(case, case['fixture_ref'])
                reference = run_grader(case, case['reference_ref'])
                PROOF.setdefault(case['id'], {}).update(seeded=output_proof(seeded), reference=output_proof(reference))
                self.assertEqual(seeded.returncode, 1, case['id'] + ': seeded grader did not fail normally')
                self.assertNotIn('Traceback', seeded.stderr, case['id'] + ': grader crashed on seed')
                self.assertEqual(reference.returncode, 0, case['id'] + ': reference grader failed')
                self.assertEqual(reference.stderr, '', case['id'] + ': reference emitted diagnostics')

    def test_meaningful_mutations_rejected(self):
        for case in cases():
            for number, mutation in enumerate(MUTATIONS.get(case['id'], []), 1):
                with self.subTest(case=case['id'], mutation=number):
                    result = run_grader(case, case['reference_ref'], mutation)
                    PROOF.setdefault(case['id'], {}).setdefault('mutations', []).append(
                        {'id': f'M{number}', **output_proof(result)})
                    self.assertEqual(result.returncode, 1, case['id'] + ': mutation was not rejected')
                    self.assertNotIn('Traceback', result.stderr, case['id'] + ': mutation caused grader crash')
                    self.assertNotIn('import:', result.stderr, case['id'] + ': mutation broke imports')
                    self.assertGreater(len(result.stderr), 0)

    def test_reference_local_commands(self):
        for case in cases():
            if case['id'] == 'seq-rename-en':
                continue  # This pre-existing control has no local test command.
            with self.subTest(case=case['id']):
                env = {**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'}
                result = subprocess.run([sys.executable, '-m', 'unittest', 'discover', '-v'],
                                        cwd=ROOT / case['reference_ref'], env=env, capture_output=True, text=True, timeout=30)
                PROOF.setdefault(case['id'], {})['local_reference_tests'] = output_proof(result)
                self.assertEqual(result.returncode, 0, case['id'] + ': advertised local command failed')


if __name__ == '__main__':
    unittest.main()
