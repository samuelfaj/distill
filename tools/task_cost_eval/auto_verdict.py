"""Auto must improve Normal; specialist comparisons report the balance of time and cost.

Ratios are ratios of totals (credits with failed attempts charged at least the
baseline's, completion time with failures charged the timeout); medians of
paired ratios are reported for continuity only. Quality: Auto must accept at
least the compared arm's count minus 1 in 60 runs, pooled over repetitions.
"""
from __future__ import annotations

import statistics

from .gates import apply_repetition_rule, case_bootstrap, ratio_of_totals
from .ultrafast_verdict import charged_completion_s, validated_pairs
from .ultrasave_verdict import gate_credits, summarize, token_reasons

# Auto must not cost more or take longer than Normal, with at least 15% saved on
# one axis. Specialists optimize their own priority; their tradeoffs are reported.
MAX_RATIO = 1.0
MIN_SAVING_RATIO = 0.85
# Accepted runs may trail the best pinned arm by at most one in this many runs.
ACCEPTANCE_SLACK_RUNS = 60


def acceptance_shortfall(accepted, best_accepted, runs):
    """Missed acceptances beyond the 1-in-60 allowance; 0 means the quality rule holds."""
    missing = best_accepted - accepted
    return missing if missing * ACCEPTANCE_SLACK_RUNS > runs else 0


def verdict(results):
    compare_versions = bool(results.get('compare_versions'))
    baseline_modes = ('normal', 'ultrafast', 'ultrasave', 'ultracode') + (('auto',) if compare_versions else ())
    reasons, pairs = validated_pairs(results, candidate_mode='auto', baseline_modes=baseline_modes,
                                     compare_versions=compare_versions)
    observed_baselines = {row['settings'].get('execution_mode') for row in results['runs']
                          if row['variant'] == 'baseline'}
    baseline_mode = next(iter(observed_baselines)) if len(observed_baselines) == 1 else None
    if baseline_mode is None:
        reasons.append('one consistent baseline mode is required')
    require_improvement = baseline_mode in ('normal', 'auto')
    report = {'verdict': 'INCOMPLETE', 'reasons': reasons, 'compare_versions': compare_versions,
              'baseline_mode': baseline_mode,
              'comparison': ('version_improvement' if baseline_mode == 'auto' else
                             'normal_improvement' if baseline_mode == 'normal' else 'specialist_tradeoff'),
              'performance_gate_required': require_improvement,
              'performance_target_met': None,
              'gate_statistic': 'ratios of totals',
              'target': {'maximum_credit_ratio_of_totals': MAX_RATIO if require_improvement else None,
                         'maximum_completion_ratio_of_totals': MAX_RATIO if require_improvement else None,
                         'minimum_saving_on_one_axis_ratio': MIN_SAVING_RATIO if require_improvement else None,
                         'minimum_candidate_accepted': f'baseline accepted - runs/{ACCEPTANCE_SLACK_RUNS}'},
              'credit_basis': results.get('credit_basis'),
              'token_basis': 'sum of provider input_tokens + output_tokens over all physical calls; '
                             'cached input and reasoning are included, not added again',
              'scope': 'bounded observed sample; no statistical equivalence or general superiority claim',
              'pairs': [], 'baseline': None, 'candidate': None}
    if reasons:
        return apply_repetition_rule(report, results.get('repetitions'))
    reasons.extend(token_reasons(results))
    if reasons:
        return apply_repetition_rule(report, results.get('repetitions'))

    for key, pair in sorted(pairs.items()):
        baseline, candidate = pair['baseline'], pair['candidate']
        if baseline['credits'] <= 0:
            reasons.append(f'{key}: nonpositive baseline credits')
            return report
        base_time, cand_time = charged_completion_s(baseline), charged_completion_s(candidate)
        if base_time <= 0:
            reasons.append(f'{key}: nonpositive baseline completion time')
            return report
        raw_credit_ratio = candidate['credits'] / baseline['credits']
        both_passed = baseline['passed'] and candidate['passed']
        report['pairs'].append({'case': key[0], 'repetition': key[1],
            'baseline': summarize([baseline]), 'candidate': summarize([candidate]),
            'baseline_passed': baseline['passed'], 'candidate_passed': candidate['passed'],
            'raw_credit_ratio': raw_credit_ratio,
            'gate_credit_ratio': raw_credit_ratio if both_passed else max(1.0, raw_credit_ratio),
            'baseline_credits': baseline['credits'], 'gate_candidate_credits': gate_credits(baseline, candidate),
            'baseline_completion_s': base_time, 'candidate_completion_s': cand_time,
            'completion_ratio': cand_time / base_time,
            'quality_regression': baseline['passed'] and not candidate['passed']})
    for variant in ('baseline', 'candidate'):
        rows = [pair[variant] for pair in pairs.values()]
        peaks = [row['acp'].get('observed_peak_live_subagents') for row in rows]
        attempts = [row['acp'].get('observed_subagent_attempts') for row in rows]
        efforts = {}
        for row in rows:
            for call in row['call_usage']:
                key = f"{call.get('model')}/{call.get('effort') or 'unknown'}"
                efforts[key] = efforts.get(key, 0) + 1
        report[variant] = {
            **summarize(rows),
            'charged_completion_s': sum(charged_completion_s(row) for row in rows),
            'median_raw_elapsed_s': statistics.median(row['wall_time_s'] for row in rows),
            'slowest_raw_elapsed_s': max(row['wall_time_s'] for row in rows),
            'calls_by_model_effort': efforts,
            'observed_peak_live_subagents': max(peaks) if all(value is not None for value in peaks) else None,
            'observed_subagent_attempts': sum(attempts) if all(value is not None for value in attempts) else None,
        }
    report['median_paired_credit_ratio'] = statistics.median(pair['gate_credit_ratio'] for pair in report['pairs'])
    report['median_paired_completion_ratio'] = statistics.median(pair['completion_ratio'] for pair in report['pairs'])
    credits = (lambda pair: pair['baseline_credits'], lambda pair: pair['gate_candidate_credits'])
    times = (lambda pair: pair['baseline_completion_s'], lambda pair: pair['candidate_completion_s'])
    credit_ratio = report['credit_ratio_of_totals'] = ratio_of_totals(report['pairs'], *credits)
    completion_ratio = report['completion_ratio_of_totals'] = ratio_of_totals(report['pairs'], *times)
    report['credit_ratio_bootstrap'] = case_bootstrap(report['pairs'], *credits)
    report['completion_ratio_bootstrap'] = case_bootstrap(report['pairs'], *times)
    report['quality_regressions'] = sum(pair['quality_regression'] for pair in report['pairs'])
    if not report['baseline']['accepted']:
        reasons.append('baseline accepted no task; no quality comparison basis')
        return apply_repetition_rule(report, results.get('repetitions'))
    if require_improvement:
        report['performance_target_met'] = (credit_ratio <= MAX_RATIO and completion_ratio <= MAX_RATIO
                                           and min(credit_ratio, completion_ratio) <= MIN_SAVING_RATIO)
        if credit_ratio > MAX_RATIO:
            reasons.append(f'credit ratio of totals {credit_ratio:.3f} exceeds {MAX_RATIO}')
        if completion_ratio > MAX_RATIO:
            reasons.append(f'completion ratio of totals {completion_ratio:.3f} exceeds {MAX_RATIO}')
        if min(credit_ratio, completion_ratio) > MIN_SAVING_RATIO:
            reasons.append(f'neither credit ratio nor completion ratio of totals reached {MIN_SAVING_RATIO}')
    runs = report['candidate']['runs']
    if acceptance_shortfall(report['candidate']['accepted'], report['baseline']['accepted'], runs):
        reasons.append(f'Auto accepted {report["candidate"]["accepted"]} of {runs} against '
                       f'{report["baseline"]["accepted"]} for {baseline_mode}; at most 1 in '
                       f'{ACCEPTANCE_SLACK_RUNS} runs may be lost')
    report['verdict'] = 'FAIL' if reasons else 'PASS'
    return apply_repetition_rule(report, results.get('repetitions'))


PINNED_MODES = ('normal', 'ultrafast', 'ultrasave', 'ultracode')
METRICS = ('credits', 'charged_completion_s')


def _metric(row, metric):
    return row['credits'] if metric == 'credits' else charged_completion_s(row)


def _totals(rows):
    return {'runs': len(rows), 'accepted': sum(row['passed'] for row in rows),
            **{metric: sum(_metric(row, metric) for row in rows) for metric in METRICS}}


def per_case_oracle(pinned, auto_rows, repetitions):
    """Per case, pick the pinned arm on odd repetitions and score it on even ones.

    Choosing and scoring on disjoint repetitions keeps the oracle from being
    flattered by the noise it was chosen on. Needs at least two repetitions.
    """
    if repetitions < 2:
        return None
    index = {label: {(row['case'], row['repetition']): row for row in rows} for label, rows in pinned.items()}
    auto = {(row['case'], row['repetition']): row for row in auto_rows}
    odd = range(1, repetitions + 1, 2)
    even = range(2, repetitions + 1, 2)
    cases = sorted({case for case, _ in auto})
    report = {}
    for metric in METRICS:
        chosen, oracle_total, auto_total, oracle_accepted, auto_accepted = {}, 0, 0, 0, 0
        for case in cases:
            def score(label):
                rows = [index[label][case, rep] for rep in odd]
                # Most accepted first, then the lowest total; the label breaks ties deterministically.
                return (sum(row['passed'] for row in rows), -sum(_metric(row, metric) for row in rows), label)
            label = max(index, key=score)
            chosen[case] = label
            for rep in even:
                oracle_total += _metric(index[label][case, rep], metric)
                oracle_accepted += index[label][case, rep]['passed']
                auto_total += _metric(auto[case, rep], metric)
                auto_accepted += auto[case, rep]['passed']
        report[metric] = {'chosen_on_odd_repetitions': chosen, 'scored_repetitions': list(even),
                          'oracle_total': oracle_total, 'auto_total': auto_total,
                          'oracle_accepted': oracle_accepted, 'auto_accepted': auto_accepted,
                          'regret': auto_total / oracle_total if oracle_total > 0 else None}
    return report


def matrix_verdict(arm_rows, repetitions, auto_arm='auto'):
    """Auto against every pinned arm of one N-arm campaign (rows already validated).

    Gate: Auto accepts at least the best pinned arm minus 1 in 60 runs. Reported:
    regret on credits and on charged time against the best single pinned mode
    that meets the same acceptance bar, and a per-case oracle.
    """
    pinned = {label: rows for label, rows in arm_rows.items()
              if label != auto_arm and not label.endswith('_aa') and rows
              and rows[0]['settings'].get('execution_mode') in PINNED_MODES}
    report = {'verdict': 'INCOMPLETE', 'reasons': [], 'auto_arm': auto_arm, 'pinned_arms': sorted(pinned),
              'target': {'minimum_auto_accepted': f'best pinned arm accepted - runs/{ACCEPTANCE_SLACK_RUNS}'},
              'scope': 'regret and oracle are reported, not gated'}
    if not pinned or not arm_rows.get(auto_arm):
        report['reasons'].append('Auto matrix verdict needs an auto arm and at least one pinned arm')
        return report
    totals = {label: _totals(rows) for label, rows in pinned.items()}
    auto = _totals(arm_rows[auto_arm])
    best_accepted = max(t['accepted'] for t in totals.values())
    eligible = [label for label, t in totals.items()
                if not acceptance_shortfall(t['accepted'], best_accepted, t['runs'])]
    report.update(arms=totals, auto=auto, best_pinned_accepted=best_accepted)
    report['regret'] = {}
    for metric in METRICS:
        best = min(eligible, key=lambda label: (totals[label][metric], label))
        report['regret'][metric] = {'best_pinned_arm': best, 'best_total': totals[best][metric],
                                    'auto_total': auto[metric],
                                    'regret': auto[metric] / totals[best][metric] if totals[best][metric] > 0 else None}
    report['per_case_oracle'] = per_case_oracle(pinned, arm_rows[auto_arm], repetitions)
    if acceptance_shortfall(auto['accepted'], best_accepted, auto['runs']):
        report['reasons'].append(f'Auto accepted {auto["accepted"]} of {auto["runs"]} against {best_accepted} for '
                                 f'the best pinned arm; at most 1 in {ACCEPTANCE_SLACK_RUNS} runs may be lost')
    report['verdict'] = 'FAIL' if report['reasons'] else 'PASS'
    return apply_repetition_rule(report, repetitions)
