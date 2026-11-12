"""Mechanical signature of a pinned mode, per benchmark run; reads run rows only, executes nothing.

A mode's signature is what its policy row makes the harness do, checked on
the run's own records: `turn_outcomes` and `policy_counts` (attached by
decision_quality.run_record), `decision_events`, `escalations` and
`call_usage`. Each check returns PASS, FAIL or NO DATA with the counts it
read. Policy rows are named `<mode>:<rule>`.

- UltraSave `effort_ceiling`: no main round above high in a run without an
  escalation. Reads the applied effort of root main calls in `call_usage`
  and the run's `escalations`; the call rows carry no turn, so a run with an
  escalation excuses every round above the ceiling. NO DATA without a main
  call of known effort, or with rounds above the ceiling and no escalation
  count.
- UltraSave `no_1h_cache_writes`: zero `cache_creation_1h_prompt_tokens` over
  the run's call rows (a top-level key or inside `usage`). NO DATA when no
  row carries the field.
- UltraFast `jev_not_awaited`: every turn of a run that asked Jev has at least
  one `ultrafast:jev_wait_skipped` row. The turn record does not say which
  turn asked, so every turn of such a run counts. NO DATA without turn
  outcomes or without Jev decision events. Awaited Jev time before the first
  main call is not checked: the run row carries no such timing.
- UltraFast `no_utility_selection`: zero utility calls whose task is
  SELECTION_TASK, with an `ultrafast:utility_selection_skipped` row present.
  NO DATA without call rows or without that row.
- UltraCode `review_on_edit_turns`: a `ultracode:review_verdict:*` row on
  every turn whose outcome shows the review ran (`verification` reviewed, or
  unverified with reason `review_unverified`). NO DATA without such turns.
- UltraCode `no_lowering_pass`: zero questions of the effort-lowering pass
  (LOWERING_LEVER). NO DATA without Jev decision events.

An arm's check is FAIL when any run fails it, PASS when at least one run
passes and none fails, NO DATA otherwise; the runs in each state are
reported beside it. The arm is FAIL when a check fails, PASS when every
check passes, NO DATA otherwise. The signature is reported, never gated.
"""
from __future__ import annotations

from collections import Counter

PASS, FAIL, NO_DATA = 'PASS', 'FAIL', 'NO DATA'
EFFORT_ORDER = ('none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max')
SAVE_CEILING = 'high'
MAIN_ROLES = ('main', 'main_retry')
# The utility task that selects what of a tool result reaches the next request (jev_cheap.rs).
SELECTION_TASK = 'select_units'
# The pass that may only lower a round's effort (jev_apply_model_tier asks this lever).
LOWERING_LEVER = 'b2_model_tier'
REVIEW_RULE = 'ultracode:review_verdict:'
REVIEW_REASON = 'review_unverified'


def _count(value):
    return value if isinstance(value, int) and not isinstance(value, bool) else None


def _effort_rank(label):
    label = label.removeprefix('effort:') if isinstance(label, str) else None
    return EFFORT_ORDER.index(label) if label in EFFORT_ORDER else None


def _turn_policy(outcome):
    policy = outcome.get('policy') if isinstance(outcome, dict) else None
    return policy if isinstance(policy, dict) else {}


def _result(status, **counts):
    return {'status': status, **counts}


def save_effort_ceiling(row):
    calls = [call for call in row.get('call_usage') or []
             if call.get('agent', 'main') == 'main' and call.get('role') in MAIN_ROLES]
    known = [rank for rank in map(_effort_rank, (call.get('effort') for call in calls)) if rank is not None]
    above = sum(rank > EFFORT_ORDER.index(SAVE_CEILING) for rank in known)
    escalations = _count(row.get('escalations'))
    counts = {'main_calls_with_effort': len(known), 'above_ceiling': above, 'escalations': escalations,
              'ceiling_rows': (row.get('policy_counts') or {}).get('ultrasave:effort_ceiling', 0)}
    if not known or (above and escalations is None):
        return _result(NO_DATA, **counts)
    return _result(FAIL if above and not escalations else PASS, **counts)


def _one_hour_writes(call):
    value = call.get('cache_creation_1h_prompt_tokens', (call.get('usage') or {}).get('cache_creation_1h_prompt_tokens'))
    return _count(value)


def save_no_1h_cache_writes(row):
    known = [value for value in map(_one_hour_writes, row.get('call_usage') or []) if value is not None]
    counts = {'calls_with_field': len(known), 'one_hour_write_tokens': sum(known),
              'calls_with_one_hour_writes': sum(value > 0 for value in known)}
    if not known:
        return _result(NO_DATA, **counts)
    return _result(FAIL if counts['one_hour_write_tokens'] else PASS, **counts)


def fast_jev_not_awaited(row):
    outcomes = row.get('turn_outcomes') or []
    asked = sum(_count((item or {}).get('asked')) or 0 for item in (row.get('decision_events') or {}).values())
    skipped = sum((_count(_turn_policy(outcome).get('ultrafast:jev_wait_skipped')) or 0) > 0 for outcome in outcomes)
    counts = {'turns': len(outcomes), 'turns_with_skip_row': skipped, 'jev_questions': asked,
              'not_checked': 'awaited Jev time before the first main call (no such timing on the run row)'}
    if not outcomes or not row.get('decision_events_found'):
        return _result(NO_DATA, **counts)
    return _result(PASS if skipped == len(outcomes) else FAIL, **counts)


def fast_no_utility_selection(row):
    calls = row.get('call_usage') or []
    selections = sum(call.get('role') == 'utility' and call.get('task_id') == SELECTION_TASK for call in calls)
    skipped = _count((row.get('policy_counts') or {}).get('ultrafast:utility_selection_skipped')) or 0
    counts = {'selection_calls': selections, 'skip_rows': skipped}
    if selections:
        return _result(FAIL, **counts)
    return _result(PASS if calls and skipped else NO_DATA, **counts)


def code_review_on_edit_turns(row):
    reviewed = [outcome for outcome in row.get('turn_outcomes') or [] if isinstance(outcome, dict) and (
        outcome.get('verification') == 'reviewed'
        or (outcome.get('verification') == 'unverified' and outcome.get('verification_reason') == REVIEW_REASON))]
    covered = sum(any(key.startswith(REVIEW_RULE) and (_count(value) or 0) > 0
                      for key, value in _turn_policy(outcome).items()) for outcome in reviewed)
    counts = {'edit_turns': len(reviewed), 'turns_with_verdict_row': covered}
    if not reviewed:
        return _result(NO_DATA, **counts)
    return _result(PASS if covered == len(reviewed) else FAIL, **counts)


def code_no_lowering_pass(row):
    asked = _count(((row.get('decision_events') or {}).get(LOWERING_LEVER) or {}).get('asked')) or 0
    counts = {'lowering_questions': asked}
    if not row.get('decision_events_found'):
        return _result(NO_DATA, **counts)
    return _result(FAIL if asked else PASS, **counts)


CHECKS = {
    'ultrasave': (('effort_ceiling', save_effort_ceiling), ('no_1h_cache_writes', save_no_1h_cache_writes)),
    'ultrafast': (('jev_not_awaited', fast_jev_not_awaited), ('no_utility_selection', fast_no_utility_selection)),
    'ultracode': (('review_on_edit_turns', code_review_on_edit_turns), ('no_lowering_pass', code_no_lowering_pass)),
}


def run_signature(mode, row):
    """{check: result} for one run of a pinned `mode`; None for a mode without a signature."""
    checks = CHECKS.get(mode)
    return {name: check(row) for name, check in checks} if checks else None


def _combined(statuses):
    if FAIL in statuses:
        return FAIL
    return PASS if PASS in statuses else NO_DATA


def arm_signature(mode, rows):
    """Per check over the arm's runs: status, runs by status and the summed counts; None without a signature."""
    if mode not in CHECKS:
        return None
    per_run = [run_signature(mode, row) for row in rows]
    checks = {}
    for name, _ in CHECKS[mode]:
        results = [run[name] for run in per_run]
        runs = Counter(result['status'] for result in results)
        totals = Counter()
        for result in results:
            totals.update({key: value for key, value in result.items() if _count(value) is not None})
        notes = sorted({result['not_checked'] for result in results if 'not_checked' in result})
        checks[name] = {'status': _combined(runs), 'runs': {state: runs[state] for state in (PASS, FAIL, NO_DATA)},
                        'counts': dict(totals), **({'not_checked': notes} if notes else {})}
    statuses = [check['status'] for check in checks.values()]
    overall = FAIL if FAIL in statuses else PASS if all(status == PASS for status in statuses) else NO_DATA
    return {'status': overall, 'checks': checks}
