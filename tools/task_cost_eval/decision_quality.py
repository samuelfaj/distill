"""Jev decision events joined with what happened next; aggregates only, executes nothing.

Reads one session's events.jsonl as turns (a turn runs from `turn_started` to
the next one; rounds are counted from `loop_started` within the turn, so the
events' own `round` field is not needed). Unknown event types and missing
fields are skipped, never an error. Every event type and field name read is in
NAMES below, so a renamed field is changed in one place.

Outcome labels, each a rule over the events of one session:
- Mode pick (a `b8_execution_mode` decision whose label is a mode, or `keep`
  for the current mode):
  `abandoned`: a `mode_switched` away from the picked mode within
  ABANDON_ROUNDS model rounds of the pick; `held`: no such switch, observed
  for ABANDON_ROUNDS rounds or to the end of the turn; `flip_flop`: the first
  switch after the pick goes back to the picked mode, which an earlier switch
  of the same turn had left; `lean_ended_failing` / `lean_ended_clean`: the
  turn's last effective mode is the picked lean mode (ultrasave, ultrafast)
  and its `turn_outcome` has / has not `checks.failed > 0` or
  `escalations > 0`. Rates are over mode picks; flip-flops are also counted
  over mid-turn switches (trigger other than request_start) as a return to a
  mode left earlier in the same turn.
- Final check (unit: a `final_check_reminder`; a final-check decision that
  sent no reminder has nothing to follow): `catch` when its follow-up shows
  `edited`, `worktree_changed` or `delivered`, or `tool_rounds >= 1` with a
  later `check_recorded` in the turn; else `false_alarm` when it shows
  `fixed_token` or zero tool rounds; else `unclear`. No follow-up: unlabelled.
- Skill hint: `followed` when a `skill_read` with the same name hash comes
  later in the same turn; base rate: share of turns with a `skill_read` whose
  hash was not suggested in that turn.
- C4 review verdict (C4_VERDICT_LABELS): an edit tool (EDIT_TOOLS) started in
  the next C4_EDIT_ROUNDS rounds, split by note (C4_NOTE_LABELS) and ok
  verdicts, so the ok split is the base rate. Edits made through the shell and
  per-file joins are not visible in the events.
- D4 and P2: `acted` when the decision label is one that changes what the
  harness does (ACTED_LABELS); a dormant lever shows 0.
- Effort: only `escalated_after_low`: an `effort_floor_raised` later in the
  turn, or a turn outcome with escalations, after a low effort pick
  (EFFORT_LOW_LABELS) or a capping policy row (CAP_RULES) in the same turn.
"""
from __future__ import annotations

import json
import math
import re
import statistics
from collections import Counter, defaultdict
from datetime import date

# Every event type and field name read from events.jsonl and usage.json.
NAMES = {
    'events': {
        'decision': 'jev_decision', 'no_answer': 'jev_no_answer', 'check': 'check_recorded',
        'reminder': 'final_check_reminder', 'followup': 'final_check_followup', 'switch': 'mode_switched',
        'floor': 'effort_floor_raised', 'skill_suggested': 'skill_suggested', 'skill_read': 'skill_read',
        'policy': 'mode_policy', 'outcome': 'turn_outcome', 'turn_started': 'turn_started',
        'loop_started': 'loop_started', 'tool_started': 'tool_started', 'turn_ended': 'turn_ended',
    },
    'fields': {
        'type': 'type', 'ts': 'ts', 'seq': 'seq', 'lever': 'lever', 'label': 'label', 'memo': 'memo',
        'latency': 'latency_ms', 'answers': 'answers', 'applied': 'applied', 'kind': 'kind',
        'outcome': 'outcome', 'reminder_seq': 'reminder_seq', 'tool_rounds': 'tool_rounds', 'edited': 'edited',
        'delivered': 'delivered', 'fixed_token': 'fixed_token', 'worktree_changed': 'worktree_changed',
        'from': 'from', 'to': 'to', 'trigger': 'trigger', 'name_hash': 'name_hash', 'mode': 'mode',
        'rule': 'rule', 'checks': 'checks', 'failed': 'failed', 'escalations': 'escalations', 'policy': 'policy',
        'tool_name': 'tool_name',
    },
    # usage.json session.utilityOutcomes keys: "<prefix><lever or mode>" -> {"decisions": {label: count}}.
    'usage': {'lever_prefix': 'jev:', 'policy_prefix': 'policy:', 'decisions': 'decisions'},
    'triggers': {'request_start': 'request_start'},
}
E, F, U = NAMES['events'], NAMES['fields'], NAMES['usage']

MODE_LEVER = 'b8_execution_mode'
EFFORT_LEVER = 'b2_micro_effort'
C4_LEVER = 'c4_diff_risk'
MODES = ('normal', 'ultrafast', 'ultracode', 'ultrasave')
KEEP_LABEL = 'keep'
LEAN_MODES = ('ultrasave', 'ultrafast')
ABANDON_ROUNDS = 3
C4_EDIT_ROUNDS = 2
# distill-tools tool_taxonomy.rs edit and write wire names.
EDIT_TOOLS = frozenset({'write', 'search_replace', 'edit', 'hashline_edit', 'apply_patch'})
C4_NOTE_LABELS = frozenset({'review:mismatch', 'review:breaks', 'review:incomplete'})
C4_VERDICT_LABELS = C4_NOTE_LABELS | {'review:ok'}
ACTED_LABELS = {'d4_compaction_timing': frozenset({'compact'}), 'p2_read_shortlist': frozenset({'select'})}
EFFORT_LOW_LABELS = frozenset({'effort:none', 'effort:minimal', 'effort:low'})
CAP_RULES = frozenset({'effort_ceiling'})
COVERAGE_TARGET = 0.8
TOP_LABELS = 12
EXPECT_TOLERANCE = 0.03
# Rate name -> (numerator fact, denominator fact).
RATES = {
    'mode.abandoned_rate': ('mode.abandoned', 'mode.picks'),
    'mode.held_rate': ('mode.held', 'mode.picks'),
    'mode.flip_flop_rate': ('mode.flip_flop', 'mode.picks'),
    'mode.lean_ended_failing_rate': ('mode.lean_ended_failing', 'mode.picks'),
    'mode.flip_flop_per_switch': ('switch.flip_flop', 'switch.mid_turn'),
    'final_check.catch_rate': ('final_check.catch', 'final_check.reminders'),
    'final_check.false_alarm_rate': ('final_check.false_alarm', 'final_check.reminders'),
    'final_check.unclear_rate': ('final_check.unclear', 'final_check.reminders'),
    'skill.follow_rate': ('skill.followed', 'skill.suggested'),
    'skill.base_rate': ('skill.unsuggested_read_turns', 'skill.turns'),
    'c4.edit_after_note_rate': ('c4.note_then_edit', 'c4.notes_observed'),
    'c4.edit_after_ok_rate': ('c4.ok_then_edit', 'c4.ok_observed'),
    'effort.escalated_after_low_rate': ('effort.escalated_after_low', 'effort.low_or_capped'),
    **{f'{lever}.acted_rate': (f'{lever}.acted', f'{lever}.decisions') for lever in ACTED_LABELS},
}
COVERAGE = {'mode': ('mode.labelled', 'mode.picks'), 'final_check': ('final_check.labelled', 'final_check.reminders'),
            'c4': ('c4.labelled', 'c4.verdicts')}
_SAFE = re.compile(r'(h:[0-9a-f]{12}|[A-Za-z0-9][A-Za-z0-9_:+-]{0,39})')
_WANTED = tuple(E.values())


def safe_label(value):
    """A short identifier or a hash passes; anything else (paths, prose, model ids) prints as 'other'."""
    return value if isinstance(value, str) and _SAFE.fullmatch(value) else 'other'


# ---------------------------------------------------------------- reading

def _day(value):
    try:
        return date.fromisoformat(str(value)[:10])
    except ValueError:
        return None


def read_turns(path, excluded=()):
    """Stream events.jsonl into turns; None when the file is missing.

    A turn: {'events': [(round, event)], 'rounds': n, 'tools': {round: [name]}, 'ended': bool}.
    Turns that started on an excluded day (START, END ranges) are dropped.
    """
    if path is None or not path.is_file():
        return None
    turns, turn = [], None

    def start(event=None):
        day = _day((event or {}).get(F['ts'])) if event else None
        skipped = day is not None and any(first <= day <= last for first, last in excluded)
        new = {'events': [], 'rounds': 0, 'tools': defaultdict(list), 'ended': False, 'skipped': skipped}
        turns.append(new)
        return new
    with path.open(errors='replace') as handle:
        for line in handle:
            if not any(tag in line for tag in _WANTED):
                continue
            try:
                event = json.loads(line)
            except ValueError:
                continue
            if not isinstance(event, dict):
                continue
            kind = event.get(F['type'])
            if kind == E['turn_started']:
                turn = start(event)
            elif kind in _WANTED:
                turn = turn or start()
                if kind == E['loop_started']:
                    turn['rounds'] += 1
                elif kind == E['tool_started']:
                    turn['tools'][turn['rounds']].append(event.get(F['tool_name']))
                elif kind == E['turn_ended']:
                    turn['ended'] = True
                else:
                    turn['events'].append((turn['rounds'], event))
    return [turn for turn in turns if not turn['skipped']]


def has_decision_events(turns):
    return any(event.get(F['type']) in (E['decision'], E['no_answer'])
               for turn in turns or () for _, event in turn['events'])


def _of(turn, kind):
    return [(pos, rnd, event) for pos, (rnd, event) in enumerate(turn['events']) if event.get(F['type']) == kind]


def _int(value):
    return value if isinstance(value, int) and not isinstance(value, bool) else None


# ---------------------------------------------------------------- per lever

def lever_stats(turns, stats=None):
    """Per lever: decisions, no-answers by kind, memo, labels, applied and latencies (mergeable)."""
    stats = stats if stats is not None else {}

    def entry(lever):
        return stats.setdefault(safe_label(lever), {
            'decisions': 0, 'with_answers': 0, 'no_answer': Counter(), 'memo_known': 0, 'memo': 0,
            'labels': Counter(), 'applied_known': 0, 'applied': Counter(), 'latency_ms': []})
    for turn in turns or ():
        for _, event in turn['events']:
            kind = event.get(F['type'])
            if kind == E['decision']:
                item = entry(event.get(F['lever']))
                item['decisions'] += 1
                item['with_answers'] += bool(event.get(F['answers']))
                label = safe_label(event.get(F['label']))
                item['labels'][label] += 1
                if isinstance(event.get(F['memo']), bool):
                    item['memo_known'] += 1
                    item['memo'] += event[F['memo']]
                applied = event.get(F['applied'])
                if applied is not None:
                    item['applied_known'] += 1
                    if applied is True or applied == 'applied':
                        item['applied'][label] += 1
                latency = event.get(F['latency'])
                if isinstance(latency, (int, float)) and not isinstance(latency, bool):
                    item['latency_ms'].append(latency)
            elif kind == E['no_answer']:
                entry(event.get(F['lever']))['no_answer'][safe_label(event.get(F['kind']))] += 1
    return stats


def _top(counter, limit=TOP_LABELS):
    top = dict(counter.most_common(limit))
    rest = sum(counter.values()) - sum(top.values())
    return {**top, **({'(rest)': rest} if rest else {})}


def _quantiles(values):
    ordered = sorted(values)
    if not ordered:
        return {'n': 0}
    return {'n': len(ordered), 'median': statistics.median(ordered), 'p90': ordered[math.ceil(0.9 * len(ordered)) - 1]}


def lever_report(stats):
    out = {}
    for lever, item in sorted(stats.items()):
        no_answer = sum(item['no_answer'].values())
        out[lever] = {'asked': item['decisions'] + no_answer, 'answered': item['decisions'],
                      'with_answers': item['with_answers'], 'no_answer': dict(item['no_answer']),
                      'memo_share': item['memo'] / item['memo_known'] if item['memo_known'] else None,
                      'labels': _top(item['labels']),
                      'applied': dict(item['applied']) if item['applied_known'] else None,
                      'latency_ms': _quantiles(item['latency_ms'])}
    return out


def compact_levers(stats):
    """Run-row summary per lever: asked, answered, no-answers, applied by label (None when unrecorded), labels."""
    return {lever: {key: value for key, value in report.items() if key in (
        'asked', 'answered', 'no_answer', 'applied', 'labels')} for lever, report in lever_report(stats).items()}


# ---------------------------------------------------------------- outcome labels

def outcome_facts(turns, initial_mode=None, facts=None):
    """Counts of every outcome label of part A over one session's turns (see the module docstring)."""
    facts = facts if facts is not None else Counter()
    for lever in ACTED_LABELS:
        # A dormant lever prints 0, not nothing.
        facts[f'{lever}.acted'] += 0
        facts[f'{lever}.decisions'] += 0
    current = initial_mode
    followups = {}
    for turn in turns or ():
        for _, _, event in _of(turn, E['followup']):
            followups.setdefault(event.get(F['reminder_seq']), event)
    for index, turn in enumerate(turns or ()):
        outcomes = _of(turn, E['outcome'])
        outcome = outcomes[-1][2] if outcomes else None
        complete = turn['ended'] or index + 1 < len(turns) or outcome is not None
        switches = _of(turn, E['switch'])
        facts['skill.turns'] += 1
        _label_modes(turn, switches, outcome, complete, current, facts)
        _label_final_check(turn, followups, facts)
        _label_skills(turn, facts)
        _label_c4(turn, complete, facts)
        _label_effort(turn, outcome, facts)
        for _, _, event in _of(turn, E['decision']):
            lever = event.get(F['lever'])
            if lever in ACTED_LABELS:
                facts[f'{lever}.decisions'] += 1
                facts[f'{lever}.acted'] += event.get(F['label']) in ACTED_LABELS[lever]
        if switches:
            current = switches[-1][2].get(F['to'])
    return facts


def _mode_at(position, switches, start):
    mode = start
    for pos, _, event in switches:
        if pos < position:
            mode = event.get(F['to'])
    return mode


def _label_modes(turn, switches, outcome, complete, start, facts):
    left = set()
    for pos, _, event in switches:
        if event.get(F['trigger']) != NAMES['triggers']['request_start']:
            facts['switch.mid_turn'] += 1
            facts['switch.flip_flop'] += event.get(F['to']) in left
        left.add(event.get(F['from']))
    picks = [(pos, rnd, event) for pos, rnd, event in _of(turn, E['decision'])
             if event.get(F['lever']) == MODE_LEVER and event.get(F['label']) in (*MODES, KEEP_LABEL)]
    final = switches[-1][2].get(F['to']) if switches else start
    lean_failing = None
    if outcome is not None:
        checks = outcome.get(F['checks']) if isinstance(outcome.get(F['checks']), dict) else {}
        lean_failing = bool((_int(checks.get(F['failed'])) or 0) > 0 or (_int(outcome.get(F['escalations'])) or 0) > 0)
    for index, (pos, rnd, event) in enumerate(picks):
        label = event.get(F['label'])
        picked = label if label in MODES else _mode_at(pos, switches, start)
        facts['mode.picks'] += 1
        labels = set()
        later = [(p, r, s) for p, r, s in switches if p > pos]
        if any(r <= rnd + ABANDON_ROUNDS and s.get(F['to']) != picked and (picked is None or s.get(F['from']) == picked)
               for p, r, s in later):
            labels.add('abandoned')
        elif complete or turn['rounds'] >= rnd + ABANDON_ROUNDS:
            labels.add('held')
        next_pick = picks[index + 1][0] if index + 1 < len(picks) else math.inf
        following = [s for p, _, s in later if p < next_pick][:1]
        earlier_left = {s.get(F['from']) for p, _, s in switches if p < pos}
        if following and picked is not None and following[0].get(F['to']) == picked and picked in earlier_left:
            labels.add('flip_flop')
        if final in LEAN_MODES and picked == final and lean_failing is not None:
            labels.add('lean_ended_failing' if lean_failing else 'lean_ended_clean')
        for name in labels:
            facts[f'mode.{name}'] += 1
        facts['mode.labelled'] += bool(labels)


def _label_final_check(turn, followups, facts):
    checks = [pos for pos, _, _ in _of(turn, E['check'])]
    for pos, _, event in _of(turn, E['reminder']):
        facts['final_check.reminders'] += 1
        followup = followups.get(event.get(F['seq'])) if event.get(F['seq']) is not None else None
        if followup is None:
            continue
        rounds = _int(followup.get(F['tool_rounds']))
        if (followup.get(F['edited']) is True or followup.get(F['worktree_changed']) is True
                or followup.get(F['delivered']) is True
                or (rounds is not None and rounds >= 1 and any(check > pos for check in checks))):
            label = 'catch'
        elif followup.get(F['fixed_token']) is True or rounds == 0:
            label = 'false_alarm'
        else:
            label = 'unclear'
        facts[f'final_check.{label}'] += 1
        facts['final_check.labelled'] += 1


def _label_skills(turn, facts):
    suggested = _of(turn, E['skill_suggested'])
    reads = _of(turn, E['skill_read'])
    hashes = {event.get(F['name_hash']) for _, _, event in suggested}
    for pos, _, event in suggested:
        facts['skill.suggested'] += 1
        facts['skill.followed'] += any(p > pos and r.get(F['name_hash']) == event.get(F['name_hash'])
                                       for p, _, r in reads)
    facts['skill.unsuggested_read_turns'] += any(r.get(F['name_hash']) not in hashes for _, _, r in reads)


def _label_c4(turn, complete, facts):
    for _, rnd, event in _of(turn, E['decision']):
        label = event.get(F['label'])
        if event.get(F['lever']) != C4_LEVER or label not in C4_VERDICT_LABELS:
            continue
        facts['c4.verdicts'] += 1
        edited = any(name in EDIT_TOOLS for later in range(rnd + 1, rnd + C4_EDIT_ROUNDS + 1)
                     for name in turn['tools'].get(later, ()))
        if not edited and not (complete or turn['rounds'] >= rnd + C4_EDIT_ROUNDS):
            continue
        kind = 'note' if label in C4_NOTE_LABELS else 'ok'
        facts['c4.notes_observed' if kind == 'note' else 'c4.ok_observed'] += 1
        facts[f'c4.{kind}_then_edit' if edited else f'c4.{kind}_no_edit'] += 1
        facts['c4.labelled'] += 1


def _label_effort(turn, outcome, facts):
    choices = [pos for pos, _, event in _of(turn, E['decision'])
               if event.get(F['lever']) == EFFORT_LEVER and event.get(F['label']) in EFFORT_LOW_LABELS]
    choices += [pos for pos, _, event in _of(turn, E['policy']) if event.get(F['rule']) in CAP_RULES]
    raises = [pos for pos, _, _ in _of(turn, E['floor'])]
    escalated = outcome is not None and (_int(outcome.get(F['escalations'])) or 0) > 0
    for pos in choices:
        facts['effort.low_or_capped'] += 1
        facts['effort.escalated_after_low'] += escalated or any(raise_pos > pos for raise_pos in raises)


def rates(facts):
    return {name: facts[top] / facts[bottom] if facts[bottom] else None for name, (top, bottom) in RATES.items()}


def coverage(facts):
    out = {name: facts[top] / facts[bottom] if facts[bottom] else None for name, (top, bottom) in COVERAGE.items()}
    labelled = sum(facts[top] for top, _ in COVERAGE.values())
    total = sum(facts[bottom] for _, bottom in COVERAGE.values())
    return {**out, 'all': labelled / total if total else None, 'target': COVERAGE_TARGET,
            'meets_target': labelled / total >= COVERAGE_TARGET if total else None}


# ---------------------------------------------------------------- session report

def parse_expect(values):
    """'key=0.074,other=40%' (repeatable) -> {key: fraction}."""
    expected = {}
    for value in values or []:
        for part in filter(None, (piece.strip() for piece in value.split(','))):
            key, _, number = part.partition('=')
            number = number.strip()
            expected[key.strip()] = float(number[:-1]) / 100 if number.endswith('%') else float(number)
    return expected


def compare_expected(measured, expected):
    out = {}
    for key, value in expected.items():
        got = measured.get(key)
        difference = None if got is None else got - value
        out[key] = {'expected': value, 'measured': got, 'difference': difference,
                    'flagged': difference is None or abs(difference) > EXPECT_TOLERANCE}
    return out


def usage_counters(utility_outcomes, counters):
    """Fold usage.json jev:<lever> and policy:<mode> decision counters into counters (mergeable)."""
    for key, outcome in (utility_outcomes or {}).items():
        decisions = outcome.get(U['decisions']) if isinstance(outcome, dict) else None
        if not isinstance(decisions, dict):
            continue
        for prefix, group in ((U['lever_prefix'], 'levers'), (U['policy_prefix'], 'policy')):
            if key.startswith(prefix):
                bucket = counters[group].setdefault(safe_label(key[len(prefix):]), Counter())
                for label, count in decisions.items():
                    if _int(count) is not None:
                        bucket[safe_label(label)] += count
    return counters


def decisions_report(sessions, expect=None, excluded=()):
    """sessions: dicts with 'events_path', 'utility_outcomes' and 'kind' (root or subagent)."""
    stats, facts, included, kinds = {}, Counter(), 0, Counter()
    counters = {'levers': {}, 'policy': {}}
    without = 0
    for session in sessions:
        turns = read_turns(session['events_path'], excluded)
        if not has_decision_events(turns):
            without += 1
            continue
        included += 1
        kinds[session.get('kind', 'root')] += 1
        lever_stats(turns, stats)
        outcome_facts(turns, facts=facts)
        usage_counters(session.get('utility_outcomes'), counters)
    measured = rates(facts)
    report = {'sessions': {'with_decision_events': included, 'no_decision_events': without, **dict(kinds)},
              'levers': lever_report(stats),
              'usage_counters': {group: {key: _top(value) for key, value in sorted(items.items())}
                                 for group, items in counters.items()},
              'outcomes': dict(sorted(facts.items())), 'rates': measured, 'coverage': coverage(facts),
              'basis': 'events.jsonl of sessions with at least one jev_decision or jev_no_answer; sessions without '
                       'them are excluded, not counted as zero'}
    if expect:
        report['expect'] = compare_expected(measured, expect)
    return report
