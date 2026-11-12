#!/usr/bin/env python3
"""Build a requested-cohort summary from family result files: per family and arm (native and
each Distill mode) cost with its basis, quality and time, with deltas against native and Normal."""
from __future__ import annotations

import argparse
import json
import math
from pathlib import Path

from .compare_additional import comparison_cost
from .compare_codex import CREDIT_RATES, evidence_mode
from .requested_cohort import MODES, REPO, ROOT

PRICING = ROOT / 'results/requested-20261016-pricing.json'
QUALITY_METRIC = ('Successful completion within the deadline, successful agent exit, unchanged frozen inputs and '
                  'passing external behavioral grader. Functional-only grade results are shown separately.')
DELTA_FORMULA = ('100 * (arm / baseline - 1) against the native arm and against the Distill Normal arm; cost and '
                 'time negative is better, quality positive is better. Quality delta is relative; percentage-point '
                 'changes are separate.')
MISSING_COST_POLICY = ('A missing call receipt is unknown, not zero. An arm with one is a lower bound from every '
                       'priceable reported call; a signed cost delta against it is a bound, rounded so that the '
                       'displayed bound still holds.')


def quality_pass(result):
    return bool(result.get('grader_passed') and result.get('exit_code') == 0 and not result.get('timed_out')
                and result.get('frozen_inputs_unchanged'))


def arm_key(row):
    """'native', or the Distill run's mode; rows written before --mode existed ran Normal."""
    if row['arm'] == 'native':
        return 'native'
    mode = row['mode'] if 'mode' in row else evidence_mode(row.get('result') or {'variant': 'distill'})
    return mode or 'unpinned'


def arm_totals(rows):
    results = [row.get('result') or {} for row in rows]
    costs = [result.get('reference_cost_usd') for result in results]
    prices = [comparison_cost([call]) for result in results for call in result.get('call_usage', [])]
    passes = sum(quality_pass(result) for result in results)
    totals = {
        'runs': len(rows), 'quality_passes': passes,
        'configuration_verified': all(result.get('model_verified') and result.get('worker_verified', True)
                                      and result.get('execution_mode_activation_confirmed') is not False
                                      for result in results),
        'accounting_complete': all(result.get('accounting_complete') for result in results),
        'wall_time_s': sum(result.get('wall_time_s') or 0 for result in results),
        'reference_cost_usd': sum(costs) if all(cost is not None for cost in costs) else None,
        'known_cost_usd': sum(cost for cost in costs if cost is not None),
        'missing_cost_cells': sum(cost is None for cost in costs),
        'functional_grader_passes': sum(bool(result.get('grader_passed')) for result in results),
        'timeouts': sum(bool(result.get('timed_out')) for result in results),
        'quality_percent': 100 * passes / len(rows) if rows else None,
        'reference_cost_lower_bound_usd': sum(price for price in prices if price is not None),
        'unpriced_calls': sum(price is None for price in prices),
        'priced_calls': sum(price is not None for price in prices),
        'missing_result_cells': sum(not row.get('result') for row in rows),
        'versions': sorted({result['version'] for result in results if result.get('version')}),
        'source_shas': sorted({result['source_sha'] for result in results if result.get('source_sha')}),
    }
    if rows and rows[0]['arm'] != 'native':
        calls = [call for result in results for call in result.get('call_usage', [])]
        totals['decision_calls'] = sum(call.get('role') == 'jev' for call in calls)
        totals['worker_generation_calls'] = sum(call.get('agent') == 'worker' and call.get('role') != 'jev'
                                                for call in calls)
    return totals


def family_totals(record):
    groups = {}
    for row in record['runs']:
        groups.setdefault(arm_key(row), []).append(row)
    return {key: arm_totals(rows) for key, rows in groups.items()}


def percent(value, base):
    return 100 * (value / base - 1) if value is not None and base else None


def deltas(arm, base):
    """Cost, quality and time of arm against base, with bounds where a receipt is missing."""
    out = {'time_delta_percent': percent(arm['wall_time_s'], base['wall_time_s'])
           if not (arm['missing_result_cells'] or base['missing_result_cells']) else None,
           'quality_delta_percent': percent(arm['quality_passes'], base['quality_passes']),
           'quality_delta_percentage_points': 100 * (arm['quality_passes'] / arm['runs']
                                                     - base['quality_passes'] / base['runs'])}
    cost, base_cost = arm['reference_cost_usd'], base['reference_cost_usd']
    if cost is not None and base_cost is not None:
        out.update(cost_delta_percent=percent(cost, base_cost), cost_delta_kind='point_estimate')
    elif cost is not None and base['reference_cost_lower_bound_usd'] > 0:
        # The baseline costs at least its lower bound, so the signed delta is at most this.
        bound = percent(cost, base['reference_cost_lower_bound_usd'])
        out.update(cost_delta_percent=None, cost_delta_percent_upper_bound=bound,
                   cost_delta_kind='conservative_upper_bound',
                   cost_delta_display_upper_bound=math.ceil(round(bound * 10, 9)) / 10)
    elif base_cost:
        bound = percent(arm['reference_cost_lower_bound_usd'], base_cost)
        out.update(cost_delta_percent=None, cost_delta_percent_lower_bound=bound,
                   cost_delta_kind='conservative_lower_bound',
                   cost_delta_display_lower_bound=math.floor(round(bound * 10, 9)) / 10)
    else:
        out.update(cost_delta_percent=None, cost_delta_kind='unknown')
    if out['cost_delta_percent'] is None and cost is not None and base_cost is not None:
        out['cost_delta_kind'] = 'unknown'  # a zero-cost baseline has no relative delta
    return out


def quality_gate(mode, arm, normal, tolerance):
    if not arm['configuration_verified']:
        return {'passed': False, 'reason': 'configuration not verified (model, worker or mode activation receipt)'}
    if mode == 'normal':
        return {'passed': True, 'reason': None}
    if normal is None:
        return {'passed': False, 'reason': 'no Normal arm to gate against'}
    if arm['quality_passes'] < normal['quality_passes'] - tolerance:
        return {'passed': False, 'reason': f'quality gate: {arm["quality_passes"]}/{arm["runs"]} successes against '
                                           f'Normal {normal["quality_passes"]}/{normal["runs"]} with tolerance {tolerance}'}
    return {'passed': True, 'reason': None}


def check_pricing(record):
    """comparison_cost is the cost model; a pricing record that disagrees with it is refused."""
    usd = record['usd_per_credit_reference']
    checks = []
    for model, rate in record.get('chatgpt_credits_per_million', {}).items():
        if CREDIT_RATES.get(model) != (rate['input'], rate['cached_input'], rate['output']):
            checks.append((model, 'credit rates', None, None))
        checks += [(model, 'input', {'prompt_tokens': 10**6}, rate['input'] * usd),
                   (model, 'cached_input', {'prompt_tokens': 10**6, 'cached_prompt_tokens': 10**6}, rate['cached_input'] * usd),
                   (model, 'output', {'completion_tokens': 10**6}, rate['output'] * usd)]
    for model, rates in record.get('claude_usd_per_million', {}).items():
        tiers = [(10**6, rates)] if 'input' in rates else [(10**5, rates['prompt_tokens_at_most_100000']),
                                                           (10**6, rates['prompt_tokens_over_100000'])]
        for tokens, rate in tiers:
            scale = tokens / 10**6
            checks += [(model, 'input', {'prompt_tokens': tokens}, rate['input'] * scale),
                       (model, 'cached_input', {'prompt_tokens': tokens, 'cached_prompt_tokens': tokens}, rate['cached_input'] * scale),
                       (model, 'cache_write_5m', {'prompt_tokens': tokens, 'cache_creation_prompt_tokens': tokens},
                        rate['cache_write_5m'] * scale),
                       (model, 'cache_write_1h', {'prompt_tokens': tokens, 'cache_creation_prompt_tokens': tokens,
                                                  'cache_creation_1h_prompt_tokens': tokens}, rate['cache_write_1h'] * scale),
                       (model, 'output', {'prompt_tokens': tokens, 'completion_tokens': 10**6},
                        rate['input'] * scale + rate['output'])]
    for model, value in record.get('openrouter_models', {}).items():
        per_token = value['pricing_per_token']
        if model == 'openrouter/free':
            continue  # priced only by zero-cost receipts, never by rate
        checks += [(model, 'input', {'prompt_tokens': 10**6}, float(per_token['prompt']) * 10**6),
                   (model, 'cached_input', {'prompt_tokens': 10**6, 'cached_prompt_tokens': 10**6},
                    float(per_token.get('input_cache_read', per_token['prompt'])) * 10**6),
                   (model, 'output', {'completion_tokens': 10**6}, float(per_token['completion']) * 10**6)]
    wrong = []
    for model, field, usage, expected in checks:
        usage = usage and {'prompt_tokens': 0, 'cached_prompt_tokens': 0, 'completion_tokens': 0, **usage}
        actual = comparison_cost([{'model': model, 'usage': usage}]) if usage else None
        if actual is None or not math.isclose(actual, expected, rel_tol=1e-9, abs_tol=1e-12):
            wrong.append(f'{model} {field}')
    if wrong:
        raise ValueError('pricing record disagrees with comparison_cost: ' + ', '.join(wrong))


def repo_path(path):
    path = Path(path).resolve()
    return str(path.relative_to(REPO)) if path.is_relative_to(REPO) else str(path)


def build(paths, *, pricing=PRICING, tolerance=0):
    check_pricing(json.loads(Path(pricing).read_text()))
    records = [json.loads(Path(path).read_text()) for path in paths]
    families = [record['family'] for record in records]
    if len(set(families)) != len(families):
        raise ValueError('one result file per family')
    pairs, present = {}, set()
    for path, record in zip(paths, records):
        totals = family_totals(record)
        if 'native' not in totals:
            raise ValueError(f'{path}: no native arm')
        native, normal = totals['native'], totals.get('normal')
        arms = {}
        for mode in MODES:
            if mode not in totals:
                continue
            present.add(mode)
            arm = totals[mode]
            arms[mode] = {**arm, 'measured': 'under Auto' if mode == 'auto' else 'mode pinned',
                          'vs_native': deltas(arm, native),
                          'vs_normal': deltas(arm, normal) if normal is not None and mode != 'normal' else None,
                          'quality_gate': quality_gate(mode, arm, normal, tolerance)}
        pair = {'source': repo_path(path), 'date': record.get('date'), 'native': native}
        if normal is not None:
            pair['distill'] = normal
            pair.update(deltas(normal, native))
        pair['arms'] = arms
        pairs[record['family']] = pair
    first = records[0]
    return {'date': max(record.get('date', '') for record in records), 'requested_pairs_only': True,
            'runs': sum(len(record['runs']) for record in records),
            'inference_reruns': any(record.get('outcome_based_reruns', False) for record in records),
            'status': 'complete' if all(r.get('status') == 'complete' for r in records) else 'incomplete',
            'quality_metric': QUALITY_METRIC, 'delta_formula': DELTA_FORMULA,
            'missing_cost_policy': MISSING_COST_POLICY, 'pricing_source': repo_path(pricing),
            'quality_tolerance': tolerance, 'headline_arm': 'normal',
            'modes': [mode for mode in MODES if mode in present],
            'cohort': {'cases': first.get('cases'), 'repetitions': first.get('repetitions'),
                       'timeout_s': first.get('timeout_s')},
            'pairs': pairs, 'credential_path_redaction': first.get('credential_path_redaction')}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--families', nargs='+', type=Path, required=True, help='family result JSON files')
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--pricing', type=Path, default=PRICING)
    parser.add_argument('--quality-tolerance', type=int, default=0,
                        help='successes a mode arm may trail the Normal arm by and still carry a claim')
    args = parser.parse_args(argv)
    if args.output.exists():
        parser.error(f'refusing to overwrite {args.output}')
    summary = build(args.families, pricing=args.pricing, tolerance=args.quality_tolerance)
    with args.output.open('x') as stream:
        stream.write(json.dumps(summary, indent=2) + '\n')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
