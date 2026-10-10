"""Performance gate for one preregistered study phase; GO is not a merge decision."""
from __future__ import annotations

import argparse
import json
import math
import re


BASELINE_SOURCE = 'ac7d11b7a4a97c9a1ffadbe6c288ed9783259414'
MAIN_MODEL = 'chatgpt/gpt-6.1-sol'
LUNA_MODEL = 'chatgpt/gpt-6-luna'


def analyze(runs, substantial_case_ids, control_case_ids, expected_repetitions, *, binary_provenance=None):
    """Analyze compare_parallel run rows. Blind review remains an external receipt."""
    substantial, controls = set(substantial_case_ids), set(control_case_ids)
    cases = substantial | controls
    reasons, invalid = [], False
    utility_request_policy_used = False

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
    if not isinstance(binary_provenance, dict):
        inconclusive('missing binary_provenance')
    else:
        for variant in ('baseline', 'candidate'):
            record = binary_provenance.get(variant)
            if not isinstance(record, dict):
                inconclusive(f'missing {variant} binary provenance')
                continue
            source, digest = record.get('source_sha'), record.get('binary_sha256')
            if not isinstance(source, str) or not re.fullmatch(r'[0-9a-f]{40}', source):
                inconclusive(f'invalid {variant} source_sha')
            elif variant == 'baseline' and source != BASELINE_SOURCE:
                inconclusive('baseline source_sha does not match preregistration')
            if not isinstance(digest, str) or not re.fullmatch(r'[0-9a-fA-F]{64}', digest):
                inconclusive(f'invalid {variant} provenance binary_sha256')
            if not isinstance(record.get('build_profile'), str) or not record['build_profile']:
                inconclusive(f'missing {variant} build_profile')
            if not isinstance(record.get('compiler_version'), str) or not record['compiler_version']:
                inconclusive(f'missing {variant} compiler_version')
        if all(isinstance(binary_provenance.get(v), dict) for v in ('baseline', 'candidate')):
            b, c = binary_provenance['baseline'], binary_provenance['candidate']
            if b.get('source_sha') == c.get('source_sha'):
                inconclusive('candidate source_sha must differ from baseline')
            if b.get('build_profile') != c.get('build_profile') or b.get('build_profile') != 'cargo-default-dev':
                inconclusive('build_profile mismatch or not cargo-default-dev')
            if b.get('compiler_version') != c.get('compiler_version'):
                inconclusive('compiler_version mismatch')
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
            if isinstance(binary_provenance, dict) and isinstance(binary_provenance.get(variant), dict):
                prov = binary_provenance[variant]
                if digest != prov.get('binary_sha256'):
                    inconclusive(f'{variant} {case}#{rep}: row binary_sha256 does not match provenance')
            if (settings.get('transport') != 'acp' or settings.get('ultracode') is not True or
                    settings.get('timeout_s') != 900 or settings.get('order_seed') != 20261009 or
                    settings.get('model') != MAIN_MODEL or settings.get('effort') != 'medium' or
                    settings.get('permissions') != 'always-approve'):
                inconclusive(f'{variant} {case}#{rep}: settings differ from preregistration')
            launch = settings.get('launch_command')
            if (not isinstance(launch, list) or not all(isinstance(arg, str) for arg in launch) or
                    'agent' not in launch or '--always-approve' not in launch or
                    launch.index('agent') >= launch.index('--always-approve') or
                    not launch or launch[-1] != 'stdio'):
                inconclusive(f'{variant} {case}#{rep}: missing or invalid launch command policy corroboration')
            if settings.get('configured_models') != {'default': MAIN_MODEL, 'worker': LUNA_MODEL,
                    'worker_effort': 'medium', 'session_summary': LUNA_MODEL} or settings.get('configured_utility') != {'model': LUNA_MODEL, 'effort': 'medium'} or settings.get('configured_main_effort_auto') is not True:
                inconclusive(f'{variant} {case}#{rep}: configured model policy mismatch')
            depth_ok = ((variant == 'baseline' and settings.get('max_depth') == 1 and settings.get('depth_env') == '1') or
                        (variant == 'candidate' and settings.get('max_depth') in (None, 2) and
                         settings.get('depth_env') == ('2' if settings.get('max_depth') == 2 else None)))
            if not depth_ok:
                inconclusive(f'{variant} {case}#{rep}: depth differs from preregistration')
            acp = row.get('acp')
            model_receipt = acp.get('model_receipt') if isinstance(acp, dict) else None
            model_result = model_receipt.get('result') if isinstance(model_receipt, dict) else None
            model_meta = model_result.get('_meta') if isinstance(model_result, dict) else None
            activation_receipt = acp.get('activation_receipt') if isinstance(acp, dict) else None
            activation_result = activation_receipt.get('result') if isinstance(activation_receipt, dict) else None
            if (not isinstance(acp, dict) or acp.get('setup_confirmed') is not True or
                    acp.get('cleanup_complete') is not True or
                    not isinstance(model_meta, dict) or model_meta.get('canonicalModelId') != MAIN_MODEL or
                    model_meta.get('reasoningEffort') != 'medium' or model_meta.get('reasoningEffortAuto') is not False or
                    not isinstance(activation_result, dict) or activation_result.get('enabled') is not True):
                inconclusive(f'{variant} {case}#{rep}: missing or failed ACP setup/model/activation/cleanup receipt')
            if row.get('passed') is True and (not isinstance(acp, dict) or acp.get('completed') is not True or acp.get('exit_code') != 0 or
                    acp.get('timed_out') is not False or acp.get('error') is not None or
                    not isinstance(acp.get('prompt_receipt'), dict) or
                    not isinstance(acp['prompt_receipt'].get('result'), dict) or
                    acp['prompt_receipt']['result'].get('stopReason') != 'end_turn'):
                inconclusive(f'{variant} {case}#{rep}: passing row lacks successful ACP completion/prompt receipt')
            calls = row.get('call_usage')
            if not isinstance(calls, list) or not calls:
                inconclusive(f'{variant} {case}#{rep}: missing per-call applied policy evidence')
            else:
                for call in calls:
                    call_fields = ('attempt', 'model', 'role', 'status', 'agent', 'endpoint', 'effort',
                                   'complete', 'credits', 'input_tokens', 'cached_input_tokens',
                                   'output_tokens', 'reasoning_tokens')
                    if (not isinstance(call, dict) or any(field not in call for field in call_fields) or
                            not call.get('complete')):
                        inconclusive(f'{variant} {case}#{rep}: incomplete call usage')
                        continue
                    agent, model, effort = call.get('agent'), call.get('model'), call.get('effort')
                    role = str(call.get('role', '')).lower()
                    if str(call.get('attempt', '')).startswith('initial-title:') and role == 'auxiliary':
                        expected_call = (LUNA_MODEL, 'low')
                    elif agent == 'worker':
                        expected_call = (LUNA_MODEL, 'medium')
                    elif agent == 'main' and role == 'main':
                        expected_call = (MAIN_MODEL, 'medium')
                    elif agent == 'main' and role == 'main_retry':
                        expected_call = (MAIN_MODEL, 'medium')
                    elif ('jev' in role or 'utility' in role) and isinstance(settings, dict) and settings.get('configured_utility') == {'model': LUNA_MODEL, 'effort': 'medium'}:
                        expected_call = (LUNA_MODEL, 'medium')
                    else:
                        inconclusive(f'{variant} {case}#{rep}: unproven applied call policy')
                        continue
                    normalized_model = {'gpt-6-luna': LUNA_MODEL, 'gpt-6.1-sol': MAIN_MODEL}.get(model, model)
                    normalized_effort = {'effort:medium': 'medium', 'effort:low': 'low'}.get(effort, effort)
                    request_policy_utility = (
                        expected_call == (LUNA_MODEL, 'medium') and
                        role == 'utility' and agent in ('main', 'worker') and
                        call.get('endpoint') == 'https://chatgpt.com/backend-api/codex/responses' and
                        normalized_model == LUNA_MODEL and normalized_effort is None and
                        call.get('requested_effort') == 'medium' and
                        isinstance(settings, dict) and
                        settings.get('configured_utility') == {'model': LUNA_MODEL, 'effort': 'medium'}
                    )
                    if request_policy_utility:
                        utility_request_policy_used = True
                        continue
                    if (normalized_model, normalized_effort) != expected_call:
                        inconclusive(f'{variant} {case}#{rep}: applied call policy mismatch')
        evidence = row.get('accounting_evidence')
        if not isinstance(evidence, dict) or evidence.get('complete') is not True or not isinstance(evidence.get('reasons'), list):
            inconclusive(f'{variant} {case}#{rep}: missing or incomplete accounting_evidence')
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
    if utility_request_policy_used:
        report['policy_disclosures'] = [
            'Sampler utility uses source-backed persisted requested effort; applied marker unavailable.'
        ]
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
    result = analyze(data['runs'], args.substantial, args.control, args.repetitions,
                     binary_provenance=data.get('binary_provenance'))
    print(json.dumps(result, indent=2))
    return 0 if result['performance_verdict'] == 'GO' else 1


if __name__ == '__main__':
    raise SystemExit(main())
