"""Shared gate statistics for paired and N-arm campaigns; executes nothing.

The median of paired ratios is biased upward for noisy identical arms (two
campaigns of the same build score about 1.1), so gates decide on the ratio of
totals and report a per-case bootstrap interval next to it. The median stays in
reports for continuity only.
"""
from __future__ import annotations

import random

# A gate verdict needs pooled evidence; fewer repetitions report a smoke only.
MIN_GATE_REPETITIONS = 3
# An A/A arm whose ratio of totals leaves this band makes the campaign INVALID.
AA_BAND = (0.90, 1.10)
BOOTSTRAP_SAMPLES = 2000
BOOTSTRAP_SEED = 20261111
BOOTSTRAP_LEVEL = 0.90


def ratio_of_totals(pairs, baseline_value, candidate_value):
    """Sum of candidate values over sum of baseline values; None without a positive base."""
    base = sum(baseline_value(pair) for pair in pairs)
    if base <= 0:
        return None
    return sum(candidate_value(pair) for pair in pairs) / base


def _quantile(values, q):
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, round(q * (len(ordered) - 1))))
    return ordered[index]


def case_bootstrap(pairs, baseline_value, candidate_value, *, level=BOOTSTRAP_LEVEL,
                   samples=BOOTSTRAP_SAMPLES, seed=BOOTSTRAP_SEED):
    """Two-sided percentile interval of the ratio of totals, resampling whole cases.

    pairs: dicts with a 'case' key; every repetition of a case moves together, so
    repetitions are never treated as independent tasks. Deterministic by seed.
    """
    by_case = {}
    for pair in pairs:
        by_case.setdefault(pair['case'], []).append(pair)
    cases = sorted(by_case)
    if not cases:
        return None
    rng = random.Random(seed)
    ratios = []
    for _ in range(samples):
        drawn = [pair for _ in cases for pair in by_case[rng.choice(cases)]]
        ratio = ratio_of_totals(drawn, baseline_value, candidate_value)
        if ratio is not None:
            ratios.append(ratio)
    if not ratios:
        return None
    tail = (1 - level) / 2
    return {'level': level, 'lower': _quantile(ratios, tail), 'upper': _quantile(ratios, 1 - tail),
            'samples': samples, 'resampled_unit': 'case'}


def one_sided_lower_bound(per_case_values, *, level=BOOTSTRAP_LEVEL, samples=BOOTSTRAP_SAMPLES,
                          seed=BOOTSTRAP_SEED):
    """One-sided bootstrap lower bound of the mean of per-case values (e.g. paired gains)."""
    values = list(per_case_values)
    if not values:
        return None
    rng = random.Random(seed)
    means = sorted(sum(rng.choice(values) for _ in values) / len(values) for _ in range(samples))
    return _quantile(means, 1 - level)


def apply_repetition_rule(report, repetitions):
    """Below MIN_GATE_REPETITIONS a finished report is a smoke, never a gate verdict."""
    report['minimum_gate_repetitions'] = MIN_GATE_REPETITIONS
    if report['verdict'] in ('PASS', 'FAIL') and (repetitions or 0) < MIN_GATE_REPETITIONS:
        report['smoke_verdict'] = report['verdict']
        report['verdict'] = 'SMOKE'
        report['reasons'].append(f'{repetitions} repetition(s): smoke only; a gate verdict needs '
                                 f'{MIN_GATE_REPETITIONS} repetitions with pooled failures')
    return report


def aa_check(baseline_rows, aa_rows, credits, seconds):
    """Compare a control arm with its identical A/A copy on credits and time totals."""
    pairs = []
    aa_index = {(row['case'], row['repetition']): row for row in aa_rows}
    for row in baseline_rows:
        twin = aa_index.get((row['case'], row['repetition']))
        if twin is not None:
            pairs.append({'case': row['case'], 'baseline': row, 'candidate': twin})
    report = {'runs': len(pairs), 'band': list(AA_BAND), 'credit_ratio': None, 'time_ratio': None,
              'valid': False, 'deviation': None, 'reasons': []}
    if not pairs or len(pairs) != len(baseline_rows) or len(pairs) != len(aa_rows):
        report['reasons'].append('A/A arm does not pair with every control run')
        return report
    if any(credits(pair[side]) is None for pair in pairs for side in ('baseline', 'candidate')):
        report['reasons'].append('A/A credits incomplete')
        return report
    report['credit_ratio'] = ratio_of_totals(pairs, lambda p: credits(p['baseline']), lambda p: credits(p['candidate']))
    report['time_ratio'] = ratio_of_totals(pairs, lambda p: seconds(p['baseline']), lambda p: seconds(p['candidate']))
    for name in ('credit_ratio', 'time_ratio'):
        value = report[name]
        if value is None or not AA_BAND[0] <= value <= AA_BAND[1]:
            report['reasons'].append(f'A/A {name.replace("_", " ")} {value if value is None else round(value, 3)} '
                                     f'outside {AA_BAND[0]:.2f}-{AA_BAND[1]:.2f}')
    report['valid'] = not report['reasons']
    report['deviation'] = {'credits': abs(1 - report['credit_ratio']) if report['credit_ratio'] is not None else None,
                           'time': abs(1 - report['time_ratio']) if report['time_ratio'] is not None else None}
    return report


def jev_unavailable_reason(row):
    """Explicit reason for a run whose Jev calls failed; None when none failed."""
    failed, total = row.get('jev_failed_calls'), row.get('jev_calls')
    if failed:
        return f'Jev unavailable ({failed} of {total} calls failed)'
    return None
