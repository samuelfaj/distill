"""Reliability of Jev's probabilities and a per-lever scorecard, from decision events; prints aggregates only.

    python3 -B -m tools.task_cost_eval.jev_calibrate [--sessions-root DIR] [--labels FILE]
        [--report reliability|scorecard ...] [--lever NAME ...] [--model ID] [--question ID]
        [--bins N] [--exclude-days 2026-01-01:2026-01-03,2026-01-09]

Reads <root>/<project>/<session>/events.jsonl: `jev_decision` and `jev_no_answer`
events, and `turn_started` to count turns. Never calls a model and never prints a
session id, path or text; Jev model ids and question hashes are printed because a
calibration entry needs them.

Outcome labels come from a JSON file (--labels):
    {"decisions": {"<session folder name>": {"<seq>": true}},
     "sessions": {"<session folder name>": false}}
A label is true when what the decision predicts or acts on was real (the work was
unfinished, the edit did break something, the picked mode was right). A decision's
own label wins over its session's.

reliability (default): per lever, probability bins against the observed rate of
true labels. The probability is the answer to --question when given (noul: its
value; choice: the probability of the chosen option, else its confidence), else
the event's `confidence`, else its only answer. A threshold (p >= t predicts true,
t chosen by balanced accuracy) is proposed only when the rarer label has at least
30 events; otherwise the lever is REFUSED with the counts. A proposal is printed
with what a calibration entry needs (fit window, n, question hashes, model ids);
it changes nothing by itself.

scorecard: per lever, asked (decision and no-answer events), answered (decisions
with answers), applied, labelled precision of applied decisions with a 95% Wilson
interval, milliseconds awaited per turn (latency of non-memo asks over all turns
read), and DORMANT when under 1% of at least 500 answers were applied. Applied is
the event's `applied` field when it has one; otherwise every label except the
deferral labels in NOT_APPLIED. Decisions labelled `shadow:...` are reported as
`<lever> (shadow)` and never count as applied.
"""
from __future__ import annotations

import argparse
import json
import math
from collections import defaultdict
from datetime import date, datetime, timezone
from pathlib import Path

DEFAULT_ROOT = Path('~/.distill/sessions')
MIN_RARER_CLASS = 30
DORMANT_MIN_ANSWERS = 500
DORMANT_APPLIED_SHARE = 0.01
SHADOW_PREFIX = 'shadow:'
# Labels that keep the caller's own path; anything else counts as applied when an
# event has no `applied` field.
NOT_APPLIED = frozenset({
    'defer', 'keep', 'complete', 'review:ok', 'review:defer', 'review:skip-prose',
    'review:defer-evidence', 'skip', 'none', 'no_answer',
})
TAGS = ('jev_decision', 'jev_no_answer', 'turn_started')


def parse_day_ranges(values):
    """'2026-01-01:2026-01-03,2026-01-09' -> [(start, end)] inclusive."""
    ranges = []
    for value in values or []:
        for part in filter(None, (piece.strip() for piece in value.split(','))):
            start, _, end = part.partition(':')
            ranges.append((date.fromisoformat(start), date.fromisoformat(end or start)))
    return ranges


def event_day(event):
    try:
        moment = datetime.fromisoformat(event.get('ts') or '')
    except ValueError:
        return None
    return (moment if moment.tzinfo else moment.replace(tzinfo=timezone.utc)).date()


def load_events(root, excluded=()):
    """[(session, event)] for the Jev events of every session, and the number of turns."""
    events, turns = [], 0
    root = Path(root).expanduser()
    for path in sorted(root.glob('*/*/events.jsonl')):
        session = path.parent.name
        with path.open(errors='replace') as handle:
            for line in handle:
                if not any(tag in line for tag in TAGS):
                    continue
                try:
                    event = json.loads(line)
                except ValueError:
                    continue
                day = event_day(event)
                if day and any(start <= day <= end for start, end in excluded):
                    continue
                if event.get('type') == 'turn_started':
                    turns += 1
                elif event.get('type') in TAGS:
                    events.append((session, event))
    return events, turns


def load_labels(path):
    if not path:
        return {'decisions': {}, 'sessions': {}}
    data = json.loads(Path(path).read_text())
    return {'decisions': data.get('decisions', {}), 'sessions': data.get('sessions', {})}


def label_of(labels, session, event):
    value = labels['decisions'].get(session, {}).get(str(event.get('seq')))
    if value is None:
        value = labels['sessions'].get(session)
    return None if value is None else bool(value)


def lever_key(event):
    lever = event.get('lever', '?')
    return f'{lever} (shadow)' if str(event.get('label', '')).startswith(SHADOW_PREFIX) else lever


def answer_probability(answer):
    if not isinstance(answer, dict):
        return None
    if answer.get('type') == 'noul':
        return answer.get('noul')
    if answer.get('type') == 'choice':
        chosen = (answer.get('probabilities') or {}).get(answer.get('choice'))
        return chosen if chosen is not None else answer.get('confidence')
    return answer.get('confidence')


def probability(event, question=None):
    answers = event.get('answers') or {}
    if question:
        return answer_probability(answers.get(question))
    if event.get('confidence') is not None:
        return event['confidence']
    if len(answers) == 1:
        return answer_probability(next(iter(answers.values())))
    return None


def applied(event):
    value = event.get('applied')
    if value is not None:
        return value is True or value == 'applied'
    label = str(event.get('label', ''))
    return not label.startswith(SHADOW_PREFIX) and label not in NOT_APPLIED


def wilson(successes, n, z=1.96):
    if n == 0:
        return None
    share = successes / n
    centre = (share + z * z / (2 * n)) / (1 + z * z / n)
    spread = z * math.sqrt(share * (1 - share) / n + z * z / (4 * n * n)) / (1 + z * z / n)
    return max(0.0, centre - spread), min(1.0, centre + spread)


def reliability(points, bins=10):
    """points: [(p, label)] -> [(low, high, n, mean_p, observed_rate)] for non-empty bins."""
    rows = []
    for index in range(bins):
        low, high = index / bins, (index + 1) / bins
        inside = [(p, y) for p, y in points if low <= p < high or (index == bins - 1 and p == 1.0)]
        if inside:
            rows.append((low, high, len(inside), sum(p for p, _ in inside) / len(inside),
                         sum(1 for _, y in inside if y) / len(inside)))
    return rows


def propose_threshold(points):
    """The cut t (p >= t predicts true) with the best balanced accuracy, or a refusal."""
    positives = sum(1 for _, y in points if y)
    negatives = len(points) - positives
    if min(positives, negatives) < MIN_RARER_CLASS:
        return {'refused': True, 'positives': positives, 'negatives': negatives}
    best = None
    for step in range(1, 20):
        cut = step / 20
        tp = sum(1 for p, y in points if p >= cut and y)
        tn = sum(1 for p, y in points if p < cut and not y)
        balanced = (tp / positives + tn / negatives) / 2
        if best is None or balanced > best['balanced_accuracy']:
            flagged = sum(1 for p, _ in points if p >= cut)
            best = {'refused': False, 'threshold': cut, 'balanced_accuracy': balanced,
                    'precision': tp / flagged if flagged else None, 'recall': tp / positives,
                    'positives': positives, 'negatives': negatives}
    return best


def report_reliability(events, labels, question=None, bins=10):
    by_lever = defaultdict(list)
    for session, event in events:
        if event.get('type') != 'jev_decision':
            continue
        label = label_of(labels, session, event)
        p = probability(event, question)
        if label is None or p is None:
            continue
        by_lever[lever_key(event)].append((float(p), label, event))
    lines = []
    for lever in sorted(by_lever):
        rows = by_lever[lever]
        points = [(p, y) for p, y, _ in rows]
        lines.append(f'== {lever}: {len(points)} labelled decisions')
        lines.append(f'{"bin":<11} {"n":>6} {"mean p":>8} {"observed":>9}')
        for low, high, n, mean_p, rate in reliability(points, bins):
            lines.append(f'{low:.2f}-{high:.2f}  {n:>6} {mean_p:>8.3f} {rate:>9.3f}')
        proposal = propose_threshold(points)
        if proposal['refused']:
            lines.append(f'REFUSED: rarer class under {MIN_RARER_CLASS} '
                         f'(true={proposal["positives"]}, false={proposal["negatives"]})')
            continue
        days = sorted(filter(None, (event_day(event) for _, _, event in rows)))
        hashes = sorted({event.get('question_hash') for _, _, event in rows if event.get('question_hash')})
        models = sorted({event.get('model_id') for _, _, event in rows if event.get('model_id')})
        precision = proposal['precision']
        lines.append(
            f'PROPOSED: threshold {proposal["threshold"]:.2f} '
            f'(balanced accuracy {proposal["balanced_accuracy"]:.3f}, '
            f'precision {"n/a" if precision is None else f"{precision:.3f}"}, recall {proposal["recall"]:.3f}; '
            f'true={proposal["positives"]}, false={proposal["negatives"]})')
        lines.append(f'  fit window {days[0] if days else "?"}..{days[-1] if days else "?"}, n={len(points)}, '
                     f'question hashes {", ".join(hashes) or "?"}, models {", ".join(models) or "?"}')
    return '\n'.join(lines) if lines else 'no labelled decisions with a probability'


def scorecard(events, labels, turns):
    rows = defaultdict(lambda: {'asked': 0, 'answered': 0, 'applied': 0, 'labelled': 0, 'correct': 0,
                                'awaited_ms': 0})
    for session, event in events:
        answered = event.get('type') == 'jev_decision' and bool(event.get('answers'))
        # A decision recorded without answers follows a no-answer event or no ask at all.
        if not answered and event.get('type') != 'jev_no_answer':
            continue
        row = rows[lever_key(event)]
        row['asked'] += 1
        if event.get('memo') is not True:
            row['awaited_ms'] += event.get('latency_ms') or 0
        if not answered:
            continue
        row['answered'] += 1
        if applied(event):
            row['applied'] += 1
            label = label_of(labels, session, event)
            if label is not None:
                row['labelled'] += 1
                row['correct'] += label
    for row in rows.values():
        row['precision'] = row['correct'] / row['labelled'] if row['labelled'] else None
        row['interval'] = wilson(row['correct'], row['labelled'])
        row['ms_per_turn'] = row['awaited_ms'] / turns if turns else None
        row['dormant'] = (row['answered'] >= DORMANT_MIN_ANSWERS
                          and row['applied'] < DORMANT_APPLIED_SHARE * row['answered'])
    return dict(rows)


def format_scorecard(rows, turns):
    lines = [f'turns read: {turns}',
             f'{"lever":<34} {"asked":>7} {"answered":>9} {"applied":>8} {"precision":>18} {"ms/turn":>8}']
    for lever in sorted(rows):
        row = rows[lever]
        if row['precision'] is None:
            precision = 'unlabelled'
        else:
            low, high = row['interval']
            precision = f'{row["precision"]:.2f} [{low:.2f},{high:.2f}]'
        per_turn = '-' if row['ms_per_turn'] is None else f'{row["ms_per_turn"]:.0f}'
        lines.append(f'{lever:<34} {row["asked"]:>7} {row["answered"]:>9} {row["applied"]:>8} '
                     f'{precision:>18} {per_turn:>8}{"  DORMANT" if row["dormant"] else ""}')
    return '\n'.join(lines)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--sessions-root', type=Path, default=DEFAULT_ROOT)
    parser.add_argument('--labels', help='JSON file of outcome labels per decision seq or per session')
    parser.add_argument('--report', action='append', choices=('reliability', 'scorecard'))
    parser.add_argument('--lever', action='append', help='only these levers')
    parser.add_argument('--model', help='only decisions answered by this Jev model id')
    parser.add_argument('--question', help='question id whose answer is the probability')
    parser.add_argument('--bins', type=int, default=10)
    parser.add_argument('--exclude-days', action='append', help='date ranges to leave out: START[:END],...')
    args = parser.parse_args(argv)
    events, turns = load_events(args.sessions_root, parse_day_ranges(args.exclude_days))
    if args.lever:
        events = [(s, e) for s, e in events if e.get('lever') in args.lever]
    if args.model:
        events = [(s, e) for s, e in events if e.get('type') != 'jev_decision' or e.get('model_id') == args.model]
    labels = load_labels(args.labels)
    for name in args.report or ['reliability']:
        print(f'### {name}')
        if name == 'reliability':
            print(report_reliability(events, labels, args.question, args.bins))
        else:
            print(format_scorecard(scorecard(events, labels, turns), turns))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
