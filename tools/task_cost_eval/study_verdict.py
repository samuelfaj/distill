"""Performance gate for one preregistered study phase; GO is not a merge decision."""
from __future__ import annotations

import argparse
import json
import math


def analyze(runs, substantial_case_ids, control_case_ids, expected_repetitions):
    """Analyze compare_parallel run rows. Blind review remains an external receipt."""
    substantial, controls = set(substantial_case_ids), set(control_case_ids)
    cases = substantial | controls
    reasons, invalid = [], False

    def inconclusive(reason):
        nonlocal invalid
        invalid = True
        reasons.append(reason)

    if not substantial or not controls or substantial & controls or expected_repetitions < 1:
        inconclusive('invalid declared case sets or repetition count')
    expected = {(case, rep) for case in cases for rep in range(1, expected_repetitions + 1)}
    indexed = {}
    profile_fields = ('model', 'effort', 'configured_models', 'configured_utility', 'configured_main_effort_auto')
    profile = None
    binary_hashes = {}
    for i, row in enumerate(runs):
        variant, case, rep = row.get('variant'), row.get('case'), row.get('repetition')
        key = (variant, case, rep)
        if variant not in ('baseline', 'candidate') or case not in cases or key in indexed:
            inconclusive(f'run {i}: unexpected or duplicate variant/case/repetition')
            continue
        indexed[key] = row
        settings = row.get('settings')
        if not isinstance(settings, dict):
            inconclusive(f'{variant} {case}#{rep}: missing settings/provenance')
        else:
            digest = settings.get('binary_sha256')
            if not isinstance(digest, str) or len(digest) != 64 or any(c not in '0123456789abcdefABCDEF' for c in digest):
                inconclusive(f'{variant} {case}#{rep}: missing or invalid binary_sha256')
            elif variant in binary_hashes and binary_hashes[variant] != digest.lower():
                inconclusive(f'{variant} {case}#{rep}: binary_sha256 mismatch within variant')
            else:
                binary_hashes[variant] = digest.lower()
            current = tuple(settings.get(f) for f in profile_fields)
            if any(v is None for v in current):
                inconclusive(f'{variant} {case}#{rep}: incomplete profile provenance')
            elif profile is None:
                profile = current
            elif profile != current:
                inconclusive(f'{variant} {case}#{rep}: profile mismatch')
        for field in ('passed', 'accounting_complete', 'ultracode_activation_confirmed',
                      'frozen_inputs_unchanged', 'binary_unchanged'):
            if not isinstance(row.get(field), bool) or (field != 'passed' and not row.get(field)):
                inconclusive(f'{variant} {case}#{rep}: missing/failed {field}')
        for field in ('credits', 'wall_time_s'):
            value = row.get(field)
            if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value < 0:
                inconclusive(f'{variant} {case}#{rep}: missing or invalid {field}')
    for variant in ('baseline', 'candidate'):
        actual = {(case, rep) for v, case, rep in indexed if v == variant}
        if actual != expected:
            inconclusive(f'{variant}: expected {len(expected)} case/repetition pairs, got {len(actual)}')
    if invalid:
        return {'performance_verdict': 'INCONCLUSIVE', 'reasons': list(dict.fromkeys(reasons)),
                'performance': None, 'blind_review': 'required external confirmation receipt'}

    def summarize(ids):
        out = {}
        for case in sorted(ids):
            out[case] = {}
            for variant in ('baseline', 'candidate'):
                rows = [indexed[variant, case, rep] for rep in range(1, expected_repetitions + 1)]
                accepted = sum(r['passed'] for r in rows)
                out[case][variant] = {'runs': len(rows), 'accepted': accepted,
                    'credits': sum(r['credits'] for r in rows),
                    'raw_elapsed_s': sum(r['wall_time_s'] for r in rows),
                    'charged_elapsed_s': sum(900 if not r['passed'] else r['wall_time_s'] for r in rows),
                    'credits_per_accepted': sum(r['credits'] for r in rows) / accepted if accepted else None}
        return out

    report = {'all': summarize(cases), 'substantial': summarize(substantial), 'controls': summarize(controls)}
    checks = []
    def check(name, ok):
        checks.append({'gate': name, 'passed': bool(ok)})
        if not ok:
            reasons.append(f'{name} not met')
    all_rows = report['all']
    check('candidate passes every run', all(v['candidate']['accepted'] == expected_repetitions for v in all_rows.values()))
    check('baseline accepts at least one task', sum(v['baseline']['accepted'] for v in all_rows.values()) > 0)
    for label in ('all', 'substantial'):
        rows = report[label]
        base_accepted = sum(v['baseline']['accepted'] for v in rows.values())
        check(f'{label} credits per accepted task', base_accepted > 0 and
              sum(v['candidate']['credits'] for v in rows.values()) / max(1, sum(v['candidate']['accepted'] for v in rows.values())) <=
              sum(v['baseline']['credits'] for v in rows.values()) / base_accepted)
        check(f'{label} charged elapsed <= 0.80 baseline',
              sum(v['candidate']['charged_elapsed_s'] for v in rows.values()) <=
              .8 * sum(v['baseline']['charged_elapsed_s'] for v in rows.values()))
    rows = report['controls']
    check('controls credits <= 1.10 baseline', sum(v['candidate']['credits'] for v in rows.values()) <=
          1.1 * sum(v['baseline']['credits'] for v in rows.values()))
    check('controls charged elapsed <= 1.10 baseline', sum(v['candidate']['charged_elapsed_s'] for v in rows.values()) <=
          1.1 * sum(v['baseline']['charged_elapsed_s'] for v in rows.values()))
    report['checks'] = checks
    return {'performance_verdict': 'NO_GO' if reasons else 'GO', 'reasons': reasons, 'performance': report,
            'blind_review': 'required external confirmation receipt; not evaluated'}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('comparison_json')
    parser.add_argument('--substantial', action='append', required=True)
    parser.add_argument('--control', action='append', required=True)
    parser.add_argument('--repetitions', type=int, default=3)
    args = parser.parse_args(argv)
    data = json.load(open(args.comparison_json))
    result = analyze(data['runs'], args.substantial, args.control, args.repetitions)
    print(json.dumps(result, indent=2))
    return 0 if result['performance_verdict'] == 'GO' else 1


if __name__ == '__main__':
    raise SystemExit(main())
