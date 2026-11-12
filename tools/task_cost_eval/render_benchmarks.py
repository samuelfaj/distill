#!/usr/bin/env python3
"""Render the generated parts of BENCHMARKS.md and README.md from a requested-cohort summary.

Only text between marker comments is written; every number there comes from the summary.
A mode without a result, or whose arm failed the quality gate, reads `no claim yet`."""
from __future__ import annotations

import argparse
import json
import math
import sys
from pathlib import Path

from .compare_codex import MODE_LABELS
from .requested_cohort import FAMILIES, MODES
from .requested_summary import repo_path

BEGIN, END = '<!-- benchmarks:begin -->', '<!-- benchmarks:end -->'
README_MODES = ('ultrasave', 'ultrafast', 'ultracode', 'auto')
MINUS, DASH = '−', '—'
NO_CLAIM = 'no claim yet'
TABLE_HEAD = ('| Pair | Native baseline | Distill main + worker | Native estimated cost | Distill estimated cost | '
              'Δ cost | Native quality | Distill quality | Δ quality | Native time | Distill time | Δ time |\n'
              '|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|')
DELTAS_NOTE = ('Deltas use `100 * (Distill / native - 1)`. Negative cost and time deltas favor Distill. Quality '
               'delta is relative; percentage-point differences are shown separately. There is no measured '
               'baseline variation, so baseline variation cells are not applicable.')


def readme_markers(mode):
    return f'<!-- benchmarks:{mode}:begin -->', f'<!-- benchmarks:{mode}:end -->'


def signed(value):
    return f'{value:+.1f}'.replace('-', MINUS)


def cost(arm):
    if arm['reference_cost_usd'] is not None:
        return f'${arm["reference_cost_usd"]:.5f}'
    return f'≥${math.floor(arm["reference_cost_lower_bound_usd"] * 1e5 + 1e-9) / 1e5:.5f}'


def time(arm):
    return ('≥' if arm.get('missing_result_cells') else '') + f'{arm["wall_time_s"]:.2f} s'


def cost_delta(delta):
    kind = delta['cost_delta_kind']
    if kind == 'point_estimate':
        return signed(delta['cost_delta_percent']) + '%'
    if kind == 'conservative_upper_bound':
        return f'≤{signed(delta["cost_delta_display_upper_bound"])}%*'
    if kind == 'conservative_lower_bound':
        return f'≥{signed(delta["cost_delta_display_lower_bound"])}%*'
    return 'unknown'


def percent(value):
    return 'unknown' if value is None else signed(value) + '%'


def quality(arm):
    return f'{arm["quality_passes"]}/{arm["runs"]}'


def headline_arm(summary, pair):
    """The Distill column of the native table: the Normal arm unless the summary names another."""
    arms = pair.get('arms')
    if arms is None:
        return pair.get('distill'), pair
    arm = arms.get(summary.get('headline_arm', 'normal'))
    return arm, (arm or {}).get('vs_native')


def claim_reason(arm):
    if arm is None:
        return 'no result'
    gate = arm.get('quality_gate', {'passed': arm['configuration_verified'],
                                    'reason': 'configuration not verified'})
    return None if gate['passed'] else gate['reason']


def native_table(summary):
    rows = [TABLE_HEAD]
    for family, pair in summary['pairs'].items():
        spec, native = FAMILIES[family], pair['native']
        arm, delta = headline_arm(summary, pair)
        reason = claim_reason(arm)
        if reason is None:
            distill = [cost(arm), cost_delta(delta), quality(arm), percent(delta['quality_delta_percent']),
                       time(arm), percent(delta['time_delta_percent'])]
        else:
            distill = [f'{NO_CLAIM} ({reason})', DASH, DASH, DASH, DASH, DASH]
        cells = [spec['label'], spec['native_label'], spec['distill_label'], cost(native), distill[0], distill[1],
                 quality(native), distill[2], distill[3], time(native), distill[4], distill[5]]
        rows.append('| ' + ' | '.join(cells) + ' |')
    return '\n'.join(rows)


def provenance(family, arm, source):
    versions = ', '.join(arm.get('versions') or ['version not recorded'])
    return f'[{FAMILIES[family]["label"]} result]({source}), {versions}'


def mode_section(summary, summary_path):
    tolerance = summary.get('quality_tolerance', 0)
    cohort = summary.get('cohort') or {}
    tasks = len(cohort.get('cases') or [])
    lines = ['## By execution mode', '',
             f'One Distill arm per execution mode ran beside the native arm in each family: '
             f'{tasks} frozen task{"s" * (tasks != 1)}, {cohort.get("repetitions")} repetitions, a '
             f'{cohort.get("timeout_s") or 0:g}-second deadline, {summary["runs"]} executions in all '
             f'([summary]({summary_path}), {summary.get("date")}). Every mode was pinned for the whole run except '
             f'Auto, measured under Auto (Jev picks the mode per request). Deltas use `100 * (arm / baseline - 1)` '
             f'against the native arm and against the Distill Normal arm of the same family. A mode carries a claim '
             f'only when its model, worker and mode activation were verified and its successes are at least the '
             f'Normal arm\'s minus {tolerance}; otherwise its row reads `{NO_CLAIM}` with the reason.', '',
             '| Family (models) | Mode | Measured | Estimated cost | Δ cost vs native | Δ cost vs Normal | Quality | '
             'Δ quality vs native | Δ quality vs Normal | Time | Δ time vs native | Δ time vs Normal | Source |',
             '|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|']
    for family, pair in summary['pairs'].items():
        name = f'{FAMILIES[family]["label"]} ({FAMILIES[family]["models"]})'
        for mode in MODES:
            if mode not in summary.get('modes', []):
                continue
            arm = pair['arms'].get(mode)
            measured = 'under Auto' if mode == 'auto' else 'mode pinned'
            reason = claim_reason(arm)
            if reason is None:
                native, normal = arm['vs_native'], arm['vs_normal']
                numbers = [cost(arm), cost_delta(native), cost_delta(normal) if normal else DASH, quality(arm),
                           percent(native['quality_delta_percent']),
                           percent(normal['quality_delta_percent']) if normal else DASH,
                           time(arm), percent(native['time_delta_percent']),
                           percent(normal['time_delta_percent']) if normal else DASH]
                source = provenance(family, arm, pair['source'])
            else:
                numbers = [f'{NO_CLAIM} ({reason})'] + [DASH] * 8
                source = provenance(family, arm or {}, pair['source'])
            lines.append('| ' + ' | '.join([name, MODE_LABELS[mode], measured, *numbers, source]) + ' |')
    return '\n'.join(lines)


def render_benchmarks_region(summary, summary_path):
    parts = [native_table(summary), DELTAS_NOTE]
    if 'arms' in next(iter(summary['pairs'].values())):
        parts.insert(1, 'The Distill columns are the Normal arm with the mode pinned; versions and result files are '
                        'in the table below.')
        parts.append(mode_section(summary, summary_path))
        if '%*' in parts[0] + parts[-1]:
            parts.append('\\* A call receipt is missing in one of the two arms: the cost is a lower bound and the '
                         'delta a bound in the direction shown, never a point estimate.')
    return '\n\n'.join(parts) + '\n'


def span(values, form):
    low, high = form(min(values)), form(max(values))
    return low if low == high else f'{low}–{high}'


def change(ratios, less, more, noun):
    if all(r < 1 for r in ratios):
        return span([100 * (1 - r) for r in ratios], lambda v: f'{v:.0f}') + f'% {less}'
    if all(r > 1 for r in ratios):
        return span([100 * (r - 1) for r in ratios], lambda v: f'{v:.0f}') + f'% {more}'
    return span(ratios, lambda v: f'{v:.3g}x') + f' {noun}'


def ratios(claims, field):
    return [arm[field] / normal[field] for _, arm, normal in claims]


def readme_claim(summary, summary_path, mode):
    """Headline and the numeric sentence for one README mode cell."""
    label, needs = MODE_LABELS[mode], {'ultrasave': {'cost'}, 'ultrafast': {'time'},
                                       'auto': {'cost', 'time'}, 'ultracode': set()}[mode]
    claims, skipped = [], []
    for family, pair in summary['pairs'].items():
        arm = pair['arms'].get(mode)
        reason = claim_reason(arm)
        if reason is None and 'cost' in needs and arm['vs_normal']['cost_delta_kind'] != 'point_estimate':
            reason = 'cost against Normal is only a bound (a call receipt is missing)'
        if reason is None and 'time' in needs and arm['vs_normal']['time_delta_percent'] is None:
            reason = 'wall time unknown (a cell has no result)'
        if reason is None:
            claims.append((family, arm, pair['arms']['normal']))
        else:
            skipped.append(f'No claim for {FAMILIES[family]["label"]}: {reason}.')
    measured = 'under Auto' if mode == 'auto' else 'with the mode pinned'
    source = f'Source: [summary]({summary_path}), {summary.get("date")}'
    if not claims:
        return f'# No claim yet\n\n{label}: {NO_CLAIM}. {" ".join(skipped)} {source}.\n'
    passes, runs = sum(a['quality_passes'] for _, a, _ in claims), sum(a['runs'] for _, a, _ in claims)
    normal_passes, normal_runs = (sum(n['quality_passes'] for _, _, n in claims), sum(n['runs'] for _, _, n in claims))
    x = lambda v: f'{v:.3g}x'
    if mode == 'ultrasave':
        headline = change(ratios(claims, 'reference_cost_usd'), 'cheaper', 'more expensive', "Normal's cost")
        what = f'UltraSave cost **{span(ratios(claims, "reference_cost_usd"), x)} the estimated cost of Normal**'
    elif mode == 'ultrafast':
        headline = change(ratios(claims, 'wall_time_s'), 'less time', 'more time', "Normal's time")
        what = f'UltraFast took **{span(ratios(claims, "wall_time_s"), x)} the wall time of Normal**'
    elif mode == 'auto':
        headline = (change(ratios(claims, 'reference_cost_usd'), 'cheaper', 'more expensive', "Normal's cost") + ', '
                    + change(ratios(claims, 'wall_time_s'), 'less time', 'more time', "Normal's time"))
        what = (f'Distill took **{span(ratios(claims, "reference_cost_usd"), x)} the estimated cost** and '
                f'**{span(ratios(claims, "wall_time_s"), x)} the wall time of Normal**')
    else:
        headline = f'{passes}/{runs} passed'
        what = f'UltraCode passed **{passes}/{runs} runs against Normal\'s {normal_passes}/{normal_runs}**'
    families = ', '.join(f'{FAMILIES[f]["label"]} ({FAMILIES[f]["models"]})' for f, _, _ in claims)
    versions = ', '.join(sorted({v for _, arm, _ in claims for v in arm.get('versions') or []})) or 'version not recorded'
    passed = '' if mode == 'ultracode' else f'; {passes}/{runs} runs passed against Normal\'s {normal_passes}/{normal_runs}'
    sentence = (f'{what} {measured} on the requested cohort: {families}{passed}. {source}; {versions}.'
                + ''.join(' ' + line for line in skipped))
    return f'# {headline}\n\n{sentence}\n'


def replace(text, begin, end, body, name):
    start, stop = text.find(begin), text.find(end)
    if text.count(begin) != 1 or text.count(end) != 1 or stop < start:
        raise ValueError(f'{name}: expected one {begin} before one {end}')
    return text[:start + len(begin)] + '\n' + body + text[stop:]


def render(summary, summary_path, benchmarks, readme):
    """Return (BENCHMARKS.md, README.md) texts. README cells hold per-mode claims, so a
    summary without per-mode arms leaves them empty."""
    benchmarks = replace(benchmarks, BEGIN, END, render_benchmarks_region(summary, summary_path), 'BENCHMARKS.md')
    per_mode = 'arms' in next(iter(summary['pairs'].values()))
    for mode in README_MODES:
        body = readme_claim(summary, summary_path, mode) if per_mode and mode in summary.get('modes', []) else (
            f'# No claim yet\n\n{MODE_LABELS[mode]}: {NO_CLAIM}. No {MODE_LABELS[mode]} arm in the summary. '
            f'Source: [summary]({summary_path}), {summary.get("date")}.\n' if per_mode else '')
        readme = replace(readme, *readme_markers(mode), body, 'README.md')
    return benchmarks, readme


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--summary', type=Path, required=True)
    parser.add_argument('--benchmarks', type=Path, required=True)
    parser.add_argument('--readme', type=Path, required=True)
    parser.add_argument('--check', action='store_true', help='fail when the files differ from the rendering')
    args = parser.parse_args(argv)
    summary = json.loads(args.summary.read_text())
    try:
        texts = render(summary, repo_path(args.summary), args.benchmarks.read_text(), args.readme.read_text())
    except ValueError as error:
        parser.error(str(error))
    stale = [path for path, text in zip((args.benchmarks, args.readme), texts) if path.read_text() != text]
    if args.check:
        for path in stale:
            print(f'{path}: differs from the rendering of {args.summary}', file=sys.stderr)
        return 1 if stale else 0
    for path, text in zip((args.benchmarks, args.readme), texts):
        if path in stale:
            path.write_text(text)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
