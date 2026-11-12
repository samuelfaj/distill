"""UltraCode gate against Normal on one campaign; derived from study_verdict.py. GO is not a merge decision.

Criteria, all pooled over every repetition:
- substantial cases: at least +15 points of accepted runs over Normal, with a
  one-sided 90% bootstrap lower bound of the per-case paired gain above 0;
- controls: no extra failures over Normal (UltraCode failures <= Normal's);
- substantial cases: credits at most 3.0x and charged time at most 2.5x Normal,
  as ratios of totals;
- controls: review overhead at most 240 s p90 per code-edit turn (optional run
  field `review_overhead_s`, a list of seconds per code-edit turn);
- at least half of the extra passes show a refutation that preceded the fix
  (optional run field `refutation_before_fix`, a bool on UltraCode rows).
An optional field that is absent is unknown, never zero: the verdict is then
INCONCLUSIVE rather than GO.
"""
from __future__ import annotations

import argparse
import json
import math

from .gates import one_sided_lower_bound, ratio_of_totals
from .ultrafast_verdict import charged_completion_s, profile_of, row_reasons

MIN_POINT_GAIN = 15.0
MAX_CREDIT_RATIO = 3.0
MAX_TIME_RATIO = 2.5
MAX_REVIEW_OVERHEAD_P90_S = 240
MIN_REFUTED_SHARE = 0.5


def p90(values):
    ordered = sorted(values)
    return ordered[math.ceil(0.9 * len(ordered)) - 1] if ordered else None


def analyze(runs, substantial_case_ids, control_case_ids, repetitions, *,
            baseline_arm='normal', candidate_arm='ultracode'):
    """Analyze compare_parallel run rows labelled by arm (or baseline/candidate)."""
    substantial, controls = set(substantial_case_ids), set(control_case_ids)
    cases = substantial | controls
    reasons = []
    if not substantial or not controls or substantial & controls or repetitions < 1:
        reasons.append('invalid declared case sets or repetition count')
    modes = {baseline_arm: ('normal',), candidate_arm: ('ultracode',)}
    index, profiles = {}, []
    for row in runs:
        arm, key = row.get('variant'), (row.get('case'), row.get('repetition'))
        if arm not in modes:
            continue
        label = f'{arm} {key[0]}#{key[1]}'
        if key[0] not in cases or (arm, *key) in index:
            reasons.append(f'{label}: unexpected or duplicate run')
            continue
        index[arm, *key] = row
        profiles.append(profile_of(row))
        reasons.extend(row_reasons(row, label, modes[arm]))
    expected = {(arm, case, rep) for arm in modes for case in cases for rep in range(1, repetitions + 1)}
    if set(index) != expected:
        reasons.append(f'expected {len(expected)} arm/case/repetition runs, got {len(set(index) & expected)}')
    if not profiles or any(profile != profiles[0] for profile in profiles):
        reasons.append('binary/model/effort/capacity/timeout settings differ between runs')
    if reasons:
        return {'performance_verdict': 'INCONCLUSIVE', 'reasons': list(dict.fromkeys(reasons)), 'checks': [],
                'performance': None}

    def rows(arm, ids):
        return [index[arm, case, rep] for case in sorted(ids) for rep in range(1, repetitions + 1)]

    def accepted(arm, ids):
        return sum(row['passed'] for row in rows(arm, ids))

    pairs = {label: [{'case': case, 'baseline': index[baseline_arm, case, rep],
                      'candidate': index[candidate_arm, case, rep]}
                     for case in sorted(ids) for rep in range(1, repetitions + 1)]
             for label, ids in (('substantial', substantial), ('controls', controls))}
    sub_runs = len(pairs['substantial'])
    gain = 100 * (accepted(candidate_arm, substantial) - accepted(baseline_arm, substantial)) / sub_runs
    per_case_gain = [100 * (sum(index[candidate_arm, case, rep]['passed'] for rep in range(1, repetitions + 1)) -
                            sum(index[baseline_arm, case, rep]['passed'] for rep in range(1, repetitions + 1))) / repetitions
                     for case in sorted(substantial)]
    lower = one_sided_lower_bound(per_case_gain)
    credit_ratio = ratio_of_totals(pairs['substantial'], lambda p: p['baseline']['credits'],
                                   lambda p: p['candidate']['credits'])
    time_ratio = ratio_of_totals(pairs['substantial'], lambda p: charged_completion_s(p['baseline']),
                                 lambda p: charged_completion_s(p['candidate']))
    control_extra_failures = ((len(pairs['controls']) - accepted(candidate_arm, controls)) -
                              (len(pairs['controls']) - accepted(baseline_arm, controls)))
    control_regressions = sum(p['baseline']['passed'] and not p['candidate']['passed'] for p in pairs['controls'])
    overheads = [row.get('review_overhead_s') for row in rows(candidate_arm, controls)]
    overhead_known = all(isinstance(values, list) for values in overheads)
    overhead_p90 = p90([value for values in overheads for value in values]) if overhead_known else None
    extra_passes = [p['candidate'] for p in pairs['substantial'] if p['candidate']['passed'] and not p['baseline']['passed']]
    refuted = [row.get('refutation_before_fix') for row in extra_passes]
    refuted_share = (sum(refuted) / len(refuted) if refuted and all(isinstance(v, bool) for v in refuted)
                     else None)

    checks = []

    def check(name, ok, unknown=None):
        checks.append({'gate': name, 'passed': None if unknown else bool(ok)})
        if unknown:
            reasons.append(f'{name}: {unknown}')
        elif not ok:
            reasons.append(f'{name} not met')

    check(f'substantial accepted gain >= {MIN_POINT_GAIN:g} points', gain >= MIN_POINT_GAIN)
    check('one-sided 90% bootstrap lower bound of the per-case gain > 0', lower is not None and lower > 0)
    check('no extra control failures', control_extra_failures <= 0)
    check(f'substantial credits <= {MAX_CREDIT_RATIO:g}x Normal', credit_ratio is not None and credit_ratio <= MAX_CREDIT_RATIO)
    check(f'substantial charged time <= {MAX_TIME_RATIO:g}x Normal', time_ratio is not None and time_ratio <= MAX_TIME_RATIO)
    check(f'control review overhead p90 <= {MAX_REVIEW_OVERHEAD_P90_S} s per code-edit turn',
          # Known and empty: no code-edit turn on the controls, so no review overhead.
          overhead_p90 is None or overhead_p90 <= MAX_REVIEW_OVERHEAD_P90_S,
          None if overhead_known else 'review_overhead_s not recorded on every control run')
    check(f'at least {MIN_REFUTED_SHARE:.0%} of extra passes refuted before the fix',
          refuted_share is not None and refuted_share >= MIN_REFUTED_SHARE,
          'refutation_before_fix not recorded on every extra pass' if extra_passes and refuted_share is None else None)
    performance = {'substantial_runs': sub_runs, 'accepted_point_gain': gain, 'per_case_gain_points': per_case_gain,
                   'gain_lower_bound_90': lower, 'substantial_credit_ratio_of_totals': credit_ratio,
                   'substantial_time_ratio_of_totals': time_ratio,
                   'control_extra_failures': control_extra_failures, 'control_regressions': control_regressions,
                   'control_review_overhead_p90_s': overhead_p90,
                   'extra_passes': len(extra_passes), 'refuted_share_of_extra_passes': refuted_share,
                   'accepted': {arm: {'substantial': accepted(arm, substantial), 'controls': accepted(arm, controls)}
                                for arm in modes}}
    verdict = ('INCONCLUSIVE' if any(c['passed'] is None for c in checks) else
               'GO' if all(c['passed'] for c in checks) else 'NO_GO')
    return {'performance_verdict': verdict, 'reasons': reasons, 'checks': checks, 'performance': performance}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('comparison_json')
    parser.add_argument('--substantial', action='append', required=True)
    parser.add_argument('--control', action='append', required=True)
    parser.add_argument('--repetitions', type=int, help='default: the campaign repetitions')
    parser.add_argument('--baseline-arm', default='normal', help='use "baseline" for a two-arm campaign')
    parser.add_argument('--candidate-arm', default='ultracode', help='use "candidate" for a two-arm campaign')
    args = parser.parse_args(argv)
    with open(args.comparison_json) as handle:
        data = json.load(handle)
    result = analyze(data['runs'], args.substantial, args.control, args.repetitions or data['repetitions'],
                     baseline_arm=args.baseline_arm, candidate_arm=args.candidate_arm)
    print(json.dumps(result, indent=2))
    return 0 if result['performance_verdict'] == 'GO' else 1


if __name__ == '__main__':
    raise SystemExit(main())
