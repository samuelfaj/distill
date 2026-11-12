#!/usr/bin/env python3
"""Plan, or with --execute run, requested cohorts: per family the native arm and one
Distill arm per execution mode, on the frozen cohort-v1 tasks, into one family JSON."""
from __future__ import annotations

import argparse
import datetime
import json
import shlex
import subprocess
import sys
from pathlib import Path

from .acp_driver import EXECUTION_MODES
from .compare_codex import ROOT
from .evaluate import _sha256_file, _sha256_tree

REPO = ROOT.parents[1]
COHORT = ROOT / 'cohort-v1.json'
CASES = ('tax-bug-en', 'active-records-pt', 'inventory-report-en')
# Arm order within a task/repetition group; the next group runs it reversed.
MODES = ('normal', 'auto', 'ultrasave', 'ultrafast', 'ultracode')
CODEX, ADDITIONAL = 'tools.task_cost_eval.compare_codex', 'tools.task_cost_eval.compare_additional'
# The 2026-10-16 arms from BENCHMARKS.md; a Distill arm adds --mode and --distill.
FAMILIES = {
    'astra': {'label': 'Astra', 'native_label': 'Codex GPT-6 Astra max',
              'distill_label': 'GPT-6 Astra auto + GPT-6 Luna auto',
              'native': [CODEX, '--variant', 'codex', '--main-model', 'gpt-6-astra', '--codex-effort', 'max'],
              'distill': [CODEX, '--variant', 'distill', '--main-model', 'gpt-6-astra',
                          '--worker-model', 'gpt-6-luna', '--enable-jev'],
              'credentials': 'Codex ChatGPT OAuth credentials (the same account for both arms) and the '
                             'OpenRouter key in the private Distill auth store (Jev)'},
    'sol': {'label': 'Sol', 'native_label': 'Codex GPT-6.1 Sol max',
            'distill_label': 'GPT-6.1 Sol auto + GPT-6 Luna auto',
            'native': [CODEX, '--variant', 'codex', '--main-model', 'gpt-6.1-sol', '--codex-effort', 'max'],
            'distill': [CODEX, '--variant', 'distill', '--main-model', 'gpt-6.1-sol',
                        '--worker-model', 'gpt-6-luna', '--enable-jev'],
            'credentials': 'Codex ChatGPT OAuth credentials (the same account for both arms) and the '
                           'OpenRouter key in the private Distill auth store (Jev)'},
    'opus': {'label': 'Opus', 'native_label': 'Claude Code Opus 5.5 max',
             'distill_label': 'Opus 5.5 auto + Haiku 5.5 auto',
             'native': [ADDITIONAL, '--variant', 'opus-native'],
             'distill': [ADDITIONAL, '--variant', 'opus-distill', '--enable-jev'], 'claude_auth': True,
             'credentials': 'the private Claude credential file (--claude-auth, both arms) and the '
                            'OpenRouter key in the private Distill auth store (Jev)'},
    'deepseek': {'label': 'DeepSeek', 'native_label': 'Codex DeepSeek V4.1 Flash max via OpenRouter Responses',
                 'distill_label': 'DeepSeek auto + free models (OpenRouter free router) auto',
                 'native': [CODEX, '--variant', 'codex', '--provider', 'openrouter',
                            '--main-model', 'deepseek/deepseek-v4.1-flash', '--codex-effort', 'max'],
                 'distill': [ADDITIONAL, '--variant', 'deepseek-free-distill', '--enable-jev'],
                 'credentials': 'the OpenRouter key in the private Distill auth store (both arms)'},
}
CREDENTIAL_ARGS = ('--claude-auth', '--codex-auth', '--distill-profile')
CREDENTIAL_FILES = {'codex-auth.json', 'auth.json', 'openrouter-auth.json', 'claude-auth.json', 'campaigns_state.json'}
REDACTION = ('Credential-file arguments are replaced by <private-credential-path> and the operator home '
             'directory by ~; raw task artifacts keep the originals.')
METERING_EVENTS = {'codex.sse_event', 'codex.websocket_request', 'codex.api_request', 'codex.conversation_starts'}


def result_path(results_dir, date, family):
    return Path(results_dir) / f'requested-{date}-{family}.json'


def plan_family(family, *, modes, repetitions, output_root, timeout, distill, claude_auth=None,
                codex=None, distill_profile=None):
    """Cells in execution order; the starting arm alternates across task/repetition groups."""
    spec = FAMILIES[family]
    arms = [('native', None)] + [('distill', mode) for mode in modes]
    cells = []
    for group, (repetition, case) in enumerate((r, c) for r in range(1, repetitions + 1) for c in CASES):
        for arm, mode in (arms if group % 2 == 0 else arms[::-1]):
            output = Path(output_root) / family / f'r{repetition}-{case}-{mode or arm}'
            command = [sys.executable, '-B', '-m', *spec[arm]]
            if arm == 'distill':
                command += ['--mode', mode, '--distill', str(distill)]
            elif codex is not None and spec[arm][0] == CODEX:
                command += ['--codex', str(codex)]
            if spec.get('claude_auth'):
                command += ['--claude-auth', str(claude_auth or '$CLAUDE_AUTH_FILE')]
            if distill_profile is not None:
                command += ['--distill-profile', str(distill_profile)]
            command += ['--case', case, '--output', str(output), '--timeout', f'{timeout:g}']
            cells.append({'family': family, 'repetition': repetition, 'case': case, 'arm': arm,
                          'mode': mode, 'output': output, 'command': command})
    return cells


def redact(command):
    return [('<private-credential-path>' if index and command[index - 1] in CREDENTIAL_ARGS else arg)
            for index, arg in enumerate(command)]


def source_hashes():
    cases = {case['id']: case for case in json.loads(COHORT.read_text())['cases']}
    paths = [ROOT / name for name in ('compare_codex.py', 'compare_additional.py', 'requested_cohort.py',
                                      'cohort-v1.json')]
    paths += [ROOT / cases[case]['grader']['script_ref'] for case in CASES]
    hashes = {str(path.relative_to(REPO)): _sha256_file(path) for path in paths}
    hashes.update({str((ROOT / cases[case]['fixture_ref']).relative_to(REPO)): _sha256_tree(ROOT / cases[case]['fixture_ref'])
                   for case in CASES})
    return hashes


def read(path, *, as_json=False):
    try:
        return json.loads(path.read_text()) if as_json else path.read_text()
    except (OSError, ValueError):
        return None


def cell_record(cell, completed):
    """Everything the runner left for this cell; a failed cell is kept as it is."""
    output = cell['output']
    result = read(output / 'result.json', as_json=True)
    row = {'repetition': cell['repetition'], 'case': cell['case'], 'arm': cell['arm'], 'mode': cell['mode'],
           'command': redact(cell['command']), 'runner_exit_code': completed.returncode, 'result': result,
           'driver_stderr': completed.stderr, 'artifact_dir': str(output)}
    for name in ('diff.patch', 'grader.txt'):
        row[name] = read(output / name)
    row['command.json'] = read(output / 'command.json', as_json=True)
    row['config.toml'] = next((text for text in (read(output / 'config.toml'), read(output / 'isolated-home/config.toml'))
                               if text is not None), None)
    session = (result or {}).get('session')
    ledgers = [path for home in ('distill-home', 'isolated-home') for path in sorted((output / home / 'sessions').rglob('usage.json'))
               if path.parent.name == session]
    row['accounting_receipts'] = (read(ledgers[0], as_json=True) or {}).get('session', {}).get('attributions', []) if ledgers else []
    telemetry = output / 'telemetry.jsonl'
    if cell['arm'] == 'native' and telemetry.is_file():
        events = [json.loads(line) for line in telemetry.read_text().splitlines() if line.strip()]
        kept = [event for event in events if event.get('fields', {}).get('event.name') in METERING_EVENTS]
        row.update(native_telemetry=kept, native_telemetry_total_events=len(events),
                   native_telemetry_omitted_non_metering_events=len(events) - len(kept),
                   native_telemetry_raw_sha256=_sha256_file(telemetry))
    row['credentials_cleaned'] = not any(path.name in CREDENTIAL_FILES for path in output.rglob('*')) if output.exists() else True
    return row


def header(family, cells, *, date, modes, repetitions, timeout, output_root):
    spec = FAMILIES[family]
    return {'date': f'{date[:4]}-{date[4:6]}-{date[6:]}', 'family': family,
            'requested_arms': [{'arm': 'native', 'mode': None, 'command': spec['native']}]
                              + [{'arm': 'distill', 'mode': mode, 'command': spec['distill'] + ['--mode', mode]}
                                 for mode in modes],
            'modes': list(modes), 'cases': list(CASES), 'repetitions': repetitions, 'timeout_s': timeout,
            'planned_executions': len(cells),
            'execution_order': 'Sequential within family; all arms adjacent per case/repetition; the arm order '
                               'reverses on alternate case/repetition groups. Families may overlap.',
            'outcome_based_reruns': False, 'source_hashes': source_hashes(), 'runs': [],
            'artifact_root': str(Path(output_root) / family),
            'cost_metric': 'Standardized reference token rates for both main models; reported Jev overhead; free '
                           'calls zero only with zero-cost receipts. Not invoice or subscription savings.',
            'quality_metric': 'Fraction of executions passing unchanged external behavior graders, successful agent '
                              'exit and frozen inputs; configuration fidelity audited separately.',
            'executed_executions': 0, 'status': 'in_progress', 'credential_path_redaction': REDACTION}


def write(path, record, mode='w'):
    with path.open(mode) as stream:
        stream.write(json.dumps(record, indent=2).replace(str(Path.home()), '~') + '\n')


def execute_family(cells, record, path, *, run=subprocess.run):
    """Run each cell exactly once, in order; the outcome never triggers a rerun."""
    write(path, record, 'x')  # never replaces an existing result file
    for cell in cells:
        completed = run(cell['command'], cwd=REPO, capture_output=True, text=True)
        record['runs'].append(cell_record(cell, completed))
        record['executed_executions'] = len(record['runs'])
        write(path, record)
    from .requested_summary import family_totals  # imports this module
    record.update(totals=family_totals(record), status='complete')
    write(path, record)
    return record


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--families', default=','.join(FAMILIES), help=f'comma list of {", ".join(FAMILIES)}')
    parser.add_argument('--modes', default=','.join(MODES), help=f'comma list of {", ".join(MODES)}')
    parser.add_argument('--repetitions', type=int, default=3)
    parser.add_argument('--timeout', type=float, default=300)
    parser.add_argument('--date', default=datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%d'),
                        help='YYYYMMDD in the result file names')
    parser.add_argument('--output-root', type=Path, help='cell artifacts; default /tmp/requested-DATE')
    parser.add_argument('--results-dir', type=Path, default=ROOT / 'results')
    parser.add_argument('--distill', type=Path, default=Path('target/release/distill'),
                        help='the one Distill binary every Distill arm runs')
    parser.add_argument('--codex', type=Path, help='native Codex binary; default: the runner default')
    parser.add_argument('--distill-profile', type=Path, help='private Distill auth store; default: the runner default')
    parser.add_argument('--claude-auth', type=Path, help='private Claude credential file (opus)')
    parser.add_argument('--execute', action='store_true', help='run the cells (spends credits); without it only the plan is printed')
    args = parser.parse_args(argv)
    families = args.families.split(',')
    modes = args.modes.split(',')
    if not families or any(f not in FAMILIES for f in families) or len(set(families)) != len(families):
        parser.error(f'--families takes distinct values of {", ".join(FAMILIES)}')
    if not modes or any(m not in EXECUTION_MODES for m in modes) or len(set(modes)) != len(modes):
        parser.error(f'--modes takes distinct values of {", ".join(MODES)}')
    modes = [m for m in MODES if m in modes]
    if args.repetitions < 1 or len(args.date) != 8 or not args.date.isdigit():
        parser.error('--repetitions must be positive and --date YYYYMMDD')
    if args.execute and 'opus' in families and args.claude_auth is None:
        parser.error('--execute with opus needs --claude-auth')
    output_root = args.output_root or Path(f'/tmp/requested-{args.date}')
    plans = {family: plan_family(family, modes=modes, repetitions=args.repetitions, output_root=output_root,
                                 timeout=args.timeout, distill=args.distill, claude_auth=args.claude_auth,
                                 codex=args.codex, distill_profile=args.distill_profile)
             for family in families}
    taken = [str(path) for family in families for path in [result_path(args.results_dir, args.date, family)]
             + [cell['output'] for cell in plans[family]] if path.exists()]
    if taken:
        parser.error('refusing to overwrite existing results or cell directories: ' + ', '.join(taken))
    for family, cells in plans.items():
        print(f'# {family}: {len(cells)} cells, agent deadline bound {len(cells) * args.timeout / 3600:g} h '
              f'({len(cells)} x {args.timeout:g} s, plus setup and grading per cell)')
        print(f'# credentials: {FAMILIES[family]["credentials"]}')
        print(f'# result: {result_path(args.results_dir, args.date, family)}')
        for index, cell in enumerate(cells, 1):
            print(f'{index:3d} {cell["arm"]:7s} {cell["mode"] or "-":9s} r{cell["repetition"]} {cell["case"]:20s} '
                  + shlex.join(cell['command']))
    if not args.execute:
        return 0
    for family, cells in plans.items():
        path = result_path(args.results_dir, args.date, family)
        record = header(family, cells, date=args.date, modes=modes, repetitions=args.repetitions,
                        timeout=args.timeout, output_root=output_root)
        execute_family(cells, record, path)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
