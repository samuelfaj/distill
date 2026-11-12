"""N-arm mode verdict: every mode arm against the shared Normal arm of one campaign.

One binary, one model-set profile per campaign (`single`: no worker model;
`tiered`: a cheaper model through `[modes.ultrasave] model`), reported
separately. The campaign needs a Normal arm and an A/A arm (Normal twice).
It is INVALID when the A/A ratio of totals leaves 0.90-1.10 on credits or on
charged time, and every savings claim must clear twice the A/A deviation.

Ratios are ratios of totals over all cases and repetitions, each with a 90%
bootstrap interval that resamples cases. Credits charge a failed attempt at
least Normal's credits; time charges a failure the timeout.

Claims (a mode that misses its own gate shows 'no claim yet'). Each own-metric
gate must clear twice the A/A deviation; each declared loss stays under its
ceiling:
- UltraSave, tiered profile only: credits per accepted run at most 0.45x Normal
  and 0.75x UltraFast (when that arm runs); charged time at most 1.5x Normal.
  The single-model profile makes no UltraSave money claim.
- UltraFast: charged time at most 0.90x Normal; credits at most 1.15x Normal,
  or 2.5x when the campaign declares `ultrafast_provider_tier: "fast"`.
- UltraCode (needs the declared case split): the accepted-run gain over Normal
  on substantial cases beyond twice the A/A deviation of accepted runs
  (|Normal - A/A| accepted over runs); credits at most 3.0x and charged time at
  most 2.5x Normal on substantial cases.
- All three: accepted at least Normal minus 1 in 60 runs, and silent failures
  at most 1 in 60. A silent failure is a failed grader on a run with no
  escalation row and no failed-check fact (optional run fields `escalations`
  and `failed_check_facts`; absent means unknown, never zero).
- Separation of UltraSave and UltraFast is reported, not gated.
Merge rule (`merge` lines, also in `reasons`; reported, never applied): when
one lean mode beats the other on credits AND on charged time by more than the
A/A deviation, `MERGE: <mode> is dominated by <mode>`; when two modes stay
within the A/A deviation on credits, charged time and accepted runs,
`MERGE: <mode> duplicates <mode>`, naming as the duplicate the later mode in
MERGE_KEEP_ORDER. A merged mode's claim becomes 'no claim yet'.
Each pinned UltraSave, UltraFast and UltraCode arm also gets its mechanical
signature (`mode_signature`, see mode_signature.py), reported only.
Auto is judged by auto_verdict.matrix_verdict and UltraCode's full gate by
ultracode_verdict.analyze (when the campaign declares its case split).
Lever-off arms (`<mode>-no-<lever>`, --jev-off) are compared with Normal and
reported, with no claim. Each arm also gets a decision gate line (its
decision metrics against Normal within the A/A deviation: PASS, FAIL or NO
DATA); no claim here depends on it, so it is reported only.
"""
from __future__ import annotations

import argparse
import json

from .auto_verdict import acceptance_shortfall, matrix_verdict
from .decision_quality import LEVER_OFF_TAG, decision_gate
from .gates import AA_BAND, aa_check, apply_repetition_rule, case_bootstrap, ratio_of_totals
from .mode_signature import CHECKS as SIGNATURE_CHECKS, arm_signature
from .ultracode_verdict import MAX_CREDIT_RATIO as CODE_MAX_CREDITS, MAX_TIME_RATIO as CODE_MAX_TIME
from .ultracode_verdict import analyze as ultracode_analyze
from .ultrafast_verdict import charged_completion_s, profile_of, row_reasons, validated_pairs
from .ultrasave_verdict import gate_credits

AA_ARM = 'normal_aa'
SAVE_MAX_CREDITS_PER_ACCEPTED = 0.45
SAVE_MAX_VS_FAST = 0.75
SAVE_MAX_TIME = 1.5
FAST_MAX_TIME = 0.90
FAST_MAX_CREDITS = 1.15
FAST_PATH_MAX_CREDITS = 2.5
LEAN_MODES = ('ultrasave', 'ultrafast')
# Of two duplicate modes the later one is named the duplicate: Normal is never
# merged away, UltraCode folds into Normal and UltraFast into UltraSave.
MERGE_KEEP_ORDER = ('normal', 'ultrasave', 'ultracode', 'ultrafast')
SEPARATION_TIME = 0.90
SEPARATION_CREDITS = 0.75
AA_MARGIN_FACTOR = 2
SILENT_SLACK_RUNS = 60


def arm_labels(arms):
    """Arm labels for an --arms list: the second 'normal' is the A/A arm."""
    labels = []
    for arm in arms:
        labels.append(AA_ARM if arm == 'normal' and 'normal' in labels else arm)
    return labels


def arm_mode(label):
    label = label.split(LEVER_OFF_TAG)[0]
    return 'normal' if label == AA_ARM else label


def validated_arms(results):
    """Every declared arm x case x repetition exactly once, with the shared run evidence."""
    arms = list(results.get('arms') or [])
    expected = {(arm, case, rep) for arm in arms for case in results['cases']
                for rep in range(1, results['repetitions'] + 1)}
    reasons, index, profiles = [], {}, []
    for row in results['runs']:
        key = (row['variant'], row['case'], row['repetition'])
        label = f'{key[0]} {key[1]}#{key[2]}'
        if key not in expected or key in index:
            reasons.append(f'{label}: unexpected or duplicate run')
            continue
        index[key] = row
        profiles.append(profile_of(row))
        reasons.extend(row_reasons(row, label, (arm_mode(key[0]),)))
    if not expected or set(index) != expected:
        reasons.append('missing declared arm/case/repetition runs')
    if not profiles or any(profile != profiles[0] for profile in profiles):
        reasons.append('binary/model/effort/capacity/timeout settings differ between runs')
    return reasons, {arm: [row for key, row in sorted(index.items()) if key[0] == arm] for arm in arms}


def silent_failures(rows):
    """Failed graders with no escalation row and no failed-check fact; absent fields are unknown."""
    silent = unknown = 0
    for row in rows:
        if (row.get('grader') or {}).get('exit_code') == 0:
            continue
        escalations, facts = row.get('escalations'), row.get('failed_check_facts')
        if escalations is None or facts is None:
            unknown += 1
        elif not escalations and not facts:
            silent += 1
    return {'silent': silent, 'unknown': unknown}


def pair_rows(control, arm):
    control_index = {(row['case'], row['repetition']): row for row in control}
    return [{'case': row['case'], 'baseline': control_index[row['case'], row['repetition']], 'candidate': row}
            for row in arm]


def compare(arm_rows, control_rows):
    """One arm against the control arm on ratios of totals, quality and silent failures."""
    pairs = pair_rows(control_rows, arm_rows)
    credits = (lambda p: p['baseline']['credits'], lambda p: gate_credits(p['baseline'], p['candidate']))
    times = (lambda p: charged_completion_s(p['baseline']), lambda p: charged_completion_s(p['candidate']))
    accepted = sum(row['passed'] for row in arm_rows)
    control_accepted = sum(row['passed'] for row in control_rows)
    arm_credits, control_credits = sum(row['credits'] for row in arm_rows), sum(row['credits'] for row in control_rows)
    per_accepted = (arm_credits / accepted) / (control_credits / control_accepted) if (
        accepted and control_accepted and control_credits > 0) else None
    return {'runs': len(arm_rows), 'accepted': accepted, 'control_accepted': control_accepted,
            'quality_regressions': sum(p['baseline']['passed'] and not p['candidate']['passed'] for p in pairs),
            'credits': arm_credits, 'control_credits': control_credits,
            'credit_ratio_of_totals': ratio_of_totals(pairs, *credits),
            'credit_ratio_bootstrap': case_bootstrap(pairs, *credits),
            'credits_per_accepted_ratio': per_accepted,
            'time_ratio_of_totals': ratio_of_totals(pairs, *times),
            'time_ratio_bootstrap': case_bootstrap(pairs, *times),
            'silent_failures': silent_failures(arm_rows)}


def saving_met(ratio, target, margin):
    """A saving counts when it meets the target and clears the A/A margin."""
    return ratio is not None and ratio <= target and 1 - ratio > margin


def within(ratio, ceiling):
    return ratio is not None and ratio <= ceiling


def claim(mode, result, *, profile, margins, fast_result=None, fast_path=False, ultracode=None):
    checks, notes = [], []

    def check(name, ok, unknown=False):
        checks.append({'gate': name, 'passed': None if unknown else bool(ok)})

    runs = result['runs']
    check(f'accepted >= Normal - runs/{SILENT_SLACK_RUNS}',
          not acceptance_shortfall(result['accepted'], result['control_accepted'], runs))
    silent = result['silent_failures']
    check(f'silent failures <= runs/{SILENT_SLACK_RUNS}', silent['silent'] * SILENT_SLACK_RUNS <= runs,
          unknown=silent['unknown'] > 0)
    if mode == 'ultrasave':
        if profile != 'tiered':
            return {'status': 'no claim', 'reason': 'single-model profile: no cheaper model to save with',
                    'checks': checks}
        check(f'credits per accepted <= {SAVE_MAX_CREDITS_PER_ACCEPTED}x Normal beyond the A/A margin',
              saving_met(result['credits_per_accepted_ratio'], SAVE_MAX_CREDITS_PER_ACCEPTED, margins['credits']))
        if fast_result is not None:
            ratio = (result['credits_per_accepted_ratio'] / fast_result['credits_per_accepted_ratio']
                     if result['credits_per_accepted_ratio'] and fast_result['credits_per_accepted_ratio'] else None)
            notes.append({'credits_per_accepted_vs_ultrafast': ratio})
            check(f'credits per accepted <= {SAVE_MAX_VS_FAST}x UltraFast beyond the A/A margin',
                  saving_met(ratio, SAVE_MAX_VS_FAST, margins['credits']))
        check(f'charged time <= {SAVE_MAX_TIME:g}x Normal', within(result['time_ratio_of_totals'], SAVE_MAX_TIME))
    elif mode == 'ultrafast':
        check(f'charged time <= {FAST_MAX_TIME}x Normal beyond the A/A margin',
              saving_met(result['time_ratio_of_totals'], FAST_MAX_TIME, margins['time']))
        ceiling = FAST_PATH_MAX_CREDITS if fast_path else FAST_MAX_CREDITS
        check(f'credits <= {ceiling:g}x Normal' + (' on the fast provider path' if fast_path else ''),
              within(result['credit_ratio_of_totals'], ceiling))
    elif mode == 'ultracode':
        performance = (ultracode or {}).get('performance') or {}
        if not performance:
            notes.append({'reason': 'declare --substantial and --control case IDs to judge UltraCode'})
        gain = performance.get('accepted_point_gain')
        check('substantial accepted gain beyond the A/A margin of accepted runs',
              gain is not None and gain / 100 > margins['accepted'], unknown=gain is None)
        for name, ratio, ceiling in (
                ('credits', performance.get('substantial_credit_ratio_of_totals'), CODE_MAX_CREDITS),
                ('charged time', performance.get('substantial_time_ratio_of_totals'), CODE_MAX_TIME)):
            check(f'substantial {name} <= {ceiling:g}x Normal', within(ratio, ceiling), unknown=not performance)
    else:
        return {'status': 'reported', 'checks': checks}
    status = ('no claim yet' if any(c['passed'] is not True for c in checks) else 'PASS')
    return {'status': status, 'checks': checks, 'notes': notes}


def separation(save_rows, fast_rows, margins):
    """UltraFast against UltraSave on time, UltraSave against UltraFast on credits."""
    times = (lambda p: charged_completion_s(p['baseline']), lambda p: charged_completion_s(p['candidate']))
    credits = (lambda p: p['baseline']['credits'], lambda p: p['candidate']['credits'])
    fast_over_save_time = ratio_of_totals(pair_rows(save_rows, fast_rows), *times)
    save_over_fast_credits = ratio_of_totals(pair_rows(fast_rows, save_rows), *credits)
    return {'ultrafast_over_ultrasave_time': fast_over_save_time,
            'ultrasave_over_ultrafast_credits': save_over_fast_credits,
            'distinct': saving_met(fast_over_save_time, SEPARATION_TIME, margins['time']) or
                        saving_met(save_over_fast_credits, SEPARATION_CREDITS, margins['credits'])}


def accepted_deviation(control_rows, aa_rows):
    """|Normal - A/A| accepted runs over the runs of one arm."""
    runs = len(control_rows)
    return abs(sum(row['passed'] for row in control_rows) - sum(row['passed'] for row in aa_rows)) / runs if runs else None


def merge_lines(arm_rows, deviation):
    """{mode: [MERGE line]} for dominated or duplicate modes (see the module docstring); never applied."""
    credits = (lambda p: p['baseline']['credits'], lambda p: p['candidate']['credits'])
    times = (lambda p: charged_completion_s(p['baseline']), lambda p: charged_completion_s(p['candidate']))

    def over(base, other):
        """`other` over `base`: credit and charged-time ratios of totals, and the accepted-run difference."""
        pairs = pair_rows(arm_rows[base], arm_rows[other])
        accepted = abs(sum(row['passed'] for row in arm_rows[other]) - sum(row['passed'] for row in arm_rows[base]))
        return (ratio_of_totals(pairs, *credits), ratio_of_totals(pairs, *times),
                accepted / len(pairs) if pairs else None)

    def beats(ratio, key):
        return ratio is not None and ratio < 1 - deviation[key]

    def inside(ratio, key):
        return ratio is not None and abs(1 - ratio) <= deviation[key] + 1e-12

    merged = {}
    if all(mode in arm_rows for mode in LEAN_MODES):
        for winner, loser in (LEAN_MODES, LEAN_MODES[::-1]):
            credit, time, _ = over(loser, winner)
            if beats(credit, 'credits') and beats(time, 'time'):
                merged.setdefault(loser, []).append(f'MERGE: {loser} is dominated by {winner}')
    modes = [mode for mode in MERGE_KEEP_ORDER if mode in arm_rows]
    for index, kept in enumerate(modes):
        for other in modes[index + 1:]:
            credit, time, accepted = over(kept, other)
            if (inside(credit, 'credits') and inside(time, 'time') and accepted is not None
                    and accepted <= deviation['accepted'] + 1e-12):
                merged.setdefault(other, []).append(f'MERGE: {other} duplicates {kept}')
    return merged


def verdict(results):
    reasons, arm_rows = validated_arms(results)
    arms = list(results.get('arms') or [])
    report = {'verdict': 'INCOMPLETE', 'reasons': reasons, 'model_profile': results.get('model_profile'),
              'arms': arms, 'aa_check': None, 'comparisons': {}, 'claims': {},
              'scope': 'bounded observed sample on one binary and one model-set profile'}
    if 'normal' not in arms or AA_ARM not in arms:
        reasons.append('a modes verdict needs a Normal arm and an A/A arm (normal listed twice)')
    if reasons:
        return apply_repetition_rule(report, results.get('repetitions'))
    report['mode_signature'] = {arm: arm_signature(arm, arm_rows[arm]) for arm in arms if arm in SIGNATURE_CHECKS}
    aa = report['aa_check'] = aa_check(arm_rows['normal'], arm_rows[AA_ARM], lambda row: row['credits'],
                                       charged_completion_s)
    if not aa['valid']:
        report['verdict'] = 'INVALID'
        reasons.extend(aa['reasons'])
        return report
    aa['deviation']['accepted'] = accepted_deviation(arm_rows['normal'], arm_rows[AA_ARM])
    margins = {key: AA_MARGIN_FACTOR * value for key, value in aa['deviation'].items()}
    report['aa_margins'] = margins
    report['decision_gate'] = {}
    for arm in arms:
        if arm not in ('normal', AA_ARM):
            report['comparisons'][arm] = compare(arm_rows[arm], arm_rows['normal'])
            report['decision_gate'][arm] = decision_gate(arm_rows[arm], arm_rows['normal'], arm_rows[AA_ARM])
    if 'ultracode' in arms:
        split = results.get('ultracode_cases') or {}
        report['ultracode'] = (ultracode_analyze(results['runs'], split['substantial'], split['control'],
                                                 results['repetitions'])
                               if split.get('substantial') and split.get('control') else
                               {'performance_verdict': 'NOT_EVALUATED',
                                'reasons': ['declare --substantial and --control case IDs to evaluate UltraCode']})
    for arm, result in report['comparisons'].items():
        report['claims'][arm] = claim(arm, result, profile=results.get('model_profile'), margins=margins,
                                      fast_result=report['comparisons'].get('ultrafast'),
                                      fast_path=results.get('ultrafast_provider_tier') == 'fast',
                                      ultracode=report.get('ultracode'))
    merged = merge_lines({arm: arm_rows[arm] for arm in arms if arm in MERGE_KEEP_ORDER}, aa['deviation'])
    report['merge'] = [line for lines in merged.values() for line in lines]
    for mode, lines in merged.items():
        report['claims'][mode].update(status='no claim yet', merge=lines)
    reasons.extend(report['merge'])
    if 'ultrasave' in arms and 'ultrafast' in arms:
        report['separation'] = separation(arm_rows['ultrasave'], arm_rows['ultrafast'], margins)
    if 'auto' in arms:
        report['auto'] = matrix_verdict({arm: arm_rows[arm] for arm in arms}, results['repetitions'],
                                        tie_band={'credits': aa['deviation']['credits'],
                                                  'charged_completion_s': aa['deviation']['time']},
                                        expected_modes=results.get('expected_modes'))
    for arm, result in report['claims'].items():
        if result['status'] == 'no claim yet':
            reasons.append(f'{arm}: no claim yet')
    if report.get('auto', {}).get('verdict') not in (None, 'PASS'):
        reasons.append(f'auto: {report["auto"]["verdict"]}')
    if report.get('ultracode', {}).get('performance_verdict') not in (None, 'GO', 'NOT_EVALUATED'):
        reasons.append(f'ultracode: {report["ultracode"]["performance_verdict"]}')
    report['verdict'] = 'FAIL' if reasons else 'PASS'
    return apply_repetition_rule(report, results.get('repetitions'))


def normal_verdict(results):
    """Normal old against new (--compare-versions): ratios of totals stay inside the A/A band."""
    reasons, pairs = validated_pairs(results, candidate_mode='normal', baseline_modes=('normal',),
                                     compare_versions=True)
    if not results.get('compare_versions'):
        reasons.append('the normal verdict compares versions; pass --compare-versions')
    report = {'verdict': 'INCOMPLETE', 'reasons': reasons, 'compare_versions': True,
              'gate_statistic': 'ratios of totals',
              'target': {'maximum_credit_ratio_of_totals': AA_BAND[1], 'maximum_time_ratio_of_totals': AA_BAND[1],
                         'minimum_candidate_accepted': f'baseline accepted - runs/{SILENT_SLACK_RUNS}'}}
    if reasons:
        return apply_repetition_rule(report, results.get('repetitions'))
    rows = {variant: [pair[variant] for _, pair in sorted(pairs.items())] for variant in ('baseline', 'candidate')}
    result = report['comparison'] = compare(rows['candidate'], rows['baseline'])
    if not result['control_accepted']:
        reasons.append('baseline accepted no task; no quality comparison basis')
        return apply_repetition_rule(report, results.get('repetitions'))
    for name in ('credit_ratio_of_totals', 'time_ratio_of_totals'):
        if result[name] is None or result[name] > AA_BAND[1]:
            reasons.append(f'{name.replace("_", " ")} {result[name]} exceeds {AA_BAND[1]}')
    if acceptance_shortfall(result['accepted'], result['control_accepted'], result['runs']):
        reasons.append(f'new Normal accepted {result["accepted"]} of {result["runs"]} against '
                       f'{result["control_accepted"]}')
    report['verdict'] = 'FAIL' if reasons else 'PASS'
    return apply_repetition_rule(report, results.get('repetitions'))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('comparison_json', nargs='+', help='one result file per model-set profile')
    args = parser.parse_args(argv)
    reports = {}
    for path in args.comparison_json:
        with open(path) as handle:
            data = json.load(handle)
        reports[f'{data.get("model_profile")}: {path}'] = verdict(data)
    print(json.dumps(reports, indent=2))
    return 0 if all(report['verdict'] == 'PASS' for report in reports.values()) else 1


if __name__ == '__main__':
    raise SystemExit(main())
