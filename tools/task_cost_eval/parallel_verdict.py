"""Pure verdict over compare_parallel results; executes nothing."""
from __future__ import annotations

OVERUSE_NUM, OVERUSE_DEN = 11, 10  # candidate sequential credits may exceed baseline by at most 10%


def _totals(runs, kind=None):
    rows = [r for r in runs if kind is None or r['kind'] == kind]
    return {'runs': len(rows), 'accepted': sum(1 for r in rows if r['passed']),
            'credits': sum(r['credits'] for r in rows), 'wall_time_s': sum(r['wall_time_s'] for r in rows)}


def verdict(results):
    """results: {'runs': [row, ...]} with variant 'baseline' | 'candidate' per row."""
    runs = results['runs']
    by_variant = {v: [r for r in runs if r['variant'] == v] for v in ('baseline', 'candidate')}
    reasons = []
    for name, rows in by_variant.items():
        if not rows:
            reasons.append(f'no {name} runs')
        reasons += [f'{name} {r["case"]}#{r["repetition"]}: accounting incomplete'
                    for r in rows if not r['accounting_complete'] or r['credits'] is None]
    keys = {v: sorted((r['case'], r['repetition']) for r in rows) for v, rows in by_variant.items()}
    if by_variant['baseline'] and by_variant['candidate'] and keys['baseline'] != keys['candidate']:
        reasons.append('baseline and candidate ran different (case, repetition) sets')
    report = {'verdict': 'INCOMPLETE', 'delegation_overuse': False, 'sequential_credit_ratio': None,
              'reasons': reasons, 'baseline': None, 'candidate': None}
    if reasons:
        return report
    base, cand = _totals(by_variant['baseline']), _totals(by_variant['candidate'])
    if base['accepted'] == 0:
        report['reasons'] = ['baseline accepted no task; no cost-per-accepted basis']
        return report
    base_cpa = base['credits'] / base['accepted']
    cand_cpa = cand['credits'] / cand['accepted'] if cand['accepted'] else float('inf')
    base_seq = _totals(by_variant['baseline'], 'sequential')['credits']
    cand_seq = _totals(by_variant['candidate'], 'sequential')['credits']
    report['delegation_overuse'] = cand_seq * OVERUSE_DEN > base_seq * OVERUSE_NUM
    report['sequential_credit_ratio'] = cand_seq / base_seq if base_seq else None
    checks = {'credits_per_accepted': cand_cpa <= base_cpa,
              'accepted_count': cand['accepted'] >= base['accepted'],
              'wall_time': cand['wall_time_s'] < base['wall_time_s']}
    report['reasons'] = [f'{name} not met' for name, ok in checks.items() if not ok]
    report['verdict'] = 'PASS' if all(checks.values()) else 'FAIL'
    report['baseline'] = {**base, 'credits_per_accepted': base_cpa}
    report['candidate'] = {**cand, 'credits_per_accepted': cand_cpa}
    return report
