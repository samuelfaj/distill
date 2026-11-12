"""Aggregate model usage across Distill session directories; prints aggregates only.

    python3 -B -m tools.task_cost_eval.session_usage [--sessions-root DIR] [--days N]
        [--exclude-days 2026-01-01:2026-01-03,2026-01-09] [--prices FILE] [--price FAMILY.FIELD=VALUE]
        [--report NAME ...] [--check-totals] [--simulate compaction ...]

Reads <root>/<project>/<session>/usage.json (and the optional events.jsonl,
execution_mode.json and subagents/*/meta.json next to it). Never writes, never
calls a model, and never prints a session ID, path, prompt or model ID: only
counts, token sums, cost-equivalents, roles and model families.

Dedupe: a root session's usage.json holds copies of its subagents' rows. Each
attempt_id is counted once, owned by the deepest session that holds it (parent
links from subagents/*/meta.json; without one, the file with the fewest rows).
Rows owned by a session with a parent, or also copied into another file, are
subagent threads; the rest are the root's own calls.

Cost-equivalents are in units of one uncached input token of the same model:
input 1, cache read 0.05 (Opus/Sonnet class), 0.025 (Fable class) or 0.1
(default), cache write 1.25 (5 minutes) or 2 (1 hour), output 5. prompt_tokens
is taken to include cache reads and writes. A cache-missing call (cached tokens
under half of the previous prompt on the same thread) is also priced on its own.

Reports (--report, repeatable): totals, cache-breaks, rounds, turn-start,
compaction. Two asked-for reports are not derivable from these files and are
omitted: `resume` (nothing records that a session was reloaded from disk) and
`mode-switch` (execution_mode.json keeps only the last choice and mode; no
per-call mode is stored). The mode-switch cause in cache-breaks is used only
when a row carries an `execution_mode` field. Cache-break causes come from the
`cacheBreaks` counters in usage.json when a session has them (cause -> count
for idleExpired, toolsChanged, settingsChanged, prefixChanged, lifetimeChanged
and unexplained, plus afterReminder and afterNoToolRound, which count what a
broken request followed and overlap the causes). The heuristic report beside
them uses: model or effort change, a cache lifetime change (the row's
`cache_lifetime_secs` when both rows carry it, else the first 1-hour write
from cache_creation_1h_prompt_tokens), gap over 5 minutes before a turn, turn
start, or unknown mid-turn.
"""
from __future__ import annotations

import argparse
import json
import math
import statistics
from collections import Counter, defaultdict, namedtuple
from datetime import date, datetime, timedelta, timezone
from pathlib import Path

DEFAULT_ROOT = Path('~/.distill/sessions')
MAIN_ROLES = ('main', 'main_retry')
POLL_TOOLS = frozenset({'get_command_or_subagent_output', 'monitor'})
MISS_SHARE = 0.5
GAP_SECONDS = 300
TOTALS_TOLERANCE = 0.10
PRICE_FIELDS = ('input', 'cache_read', 'cache_write_5m', 'cache_write_1h', 'output')
DEFAULT_PRICES = {
    'fable': {'match': ['fable'], 'input': 1.0, 'cache_read': 0.025, 'cache_write_5m': 1.25,
              'cache_write_1h': 2.0, 'output': 5.0},
    'opus-sonnet': {'match': ['opus', 'sonnet'], 'input': 1.0, 'cache_read': 0.05, 'cache_write_5m': 1.25,
                    'cache_write_1h': 2.0, 'output': 5.0},
    'default': {'match': [], 'input': 1.0, 'cache_read': 0.1, 'cache_write_5m': 1.25,
                'cache_write_1h': 2.0, 'output': 5.0},
}

Call = namedtuple('Call', 'attempt role family prompt cached write write_1h output duration_ms effort model '
                          'turn status label mode day lifetime', defaults=(None,))
# usage.json `cacheBreaks` keys that count what a broken request followed; every other key is a cause.
COUNTER_FOLLOWING = ('afterReminder', 'afterNoToolRound')


# ---------------------------------------------------------------- loading

def parse_time(value):
    try:
        moment = datetime.fromisoformat(value)
    except (TypeError, ValueError):
        return None
    return moment if moment.tzinfo else moment.replace(tzinfo=timezone.utc)


def parse_day_ranges(values):
    """'2026-01-01:2026-01-03,2026-01-09' -> [(start, end)] inclusive."""
    ranges = []
    for value in values or []:
        for part in filter(None, (piece.strip() for piece in value.split(','))):
            start, _, end = part.partition(':')
            ranges.append((date.fromisoformat(start), date.fromisoformat(end or start)))
    return ranges


def family_of(model, prices):
    lowered = (model or '').lower()
    for name, entry in prices.items():
        if name != 'default' and any(token in lowered for token in entry.get('match', [])):
            return name
    return 'default'


def read_events(path):
    """Stream events.jsonl: turn start times and round counts; phase lines are skipped unparsed."""
    starts, rounds, tool_rounds, poll_only = {}, 0, 0, 0
    tools = None

    def close():
        nonlocal tool_rounds, poll_only
        if tools:
            tool_rounds += 1
            poll_only += set(tools) <= POLL_TOOLS
    if not path.is_file():
        return None
    with path.open(errors='replace') as handle:
        for line in handle:
            if not any(tag in line for tag in ('turn_started', 'loop_started', 'tool_started', 'turn_ended')):
                continue
            try:
                event = json.loads(line)
            except ValueError:
                continue
            kind = event.get('type')
            if kind == 'turn_started':
                close()
                tools = None
                starts[event.get('turn_number')] = parse_time(event.get('ts'))
            elif kind == 'loop_started':
                close()
                rounds += 1
                tools = []
            elif kind == 'tool_started' and tools is not None:
                tools.append(event.get('tool_name'))
            elif kind == 'turn_ended':
                close()
                tools = None
    close()
    return {'turn_starts': starts, 'rounds': rounds, 'tool_rounds': tool_rounds, 'poll_only_rounds': poll_only}


def load_session(directory, prices, cutoff=None):
    usage = json.loads((directory / 'usage.json').read_text())
    updated = parse_time(usage.get('updatedAt'))
    if cutoff is not None and not (updated and updated >= cutoff):
        return None
    ledger = usage.get('session') or {}
    events = read_events(directory / 'events.jsonl')
    turn_end, turn_number, turn_day = {}, {}, {}
    for turn in usage.get('turns') or []:
        ended = parse_time(turn.get('endedAt'))
        for row in turn.get('attributions') or []:
            if row.get('turn_id'):
                turn_number[row['turn_id']] = turn.get('turnNumber')
                turn_day[row['turn_id']] = ended.date() if ended else None
        turn_end[turn.get('turnNumber')] = ended
    gaps = {}
    for turn_id, number in turn_number.items():
        start = (events or {}).get('turn_starts', {}).get(number)
        previous = turn_end.get(number - 1) if isinstance(number, int) else None
        gaps[turn_id] = (start - previous).total_seconds() if start and previous else None
    calls = []
    for row in ledger.get('attributions') or []:
        tokens = row.get('usage') or {}
        calls.append(Call(
            attempt=row.get('attempt_id'), role=row.get('role'), family=family_of(row.get('model_id'), prices),
            prompt=tokens.get('prompt_tokens') or 0, cached=tokens.get('cached_prompt_tokens') or 0,
            write=tokens.get('cache_creation_prompt_tokens') or 0,
            write_1h=tokens.get('cache_creation_1h_prompt_tokens') or 0,
            output=tokens.get('completion_tokens') or 0, duration_ms=row.get('api_duration_ms'),
            effort=row.get('applied_effort'), model=row.get('model_id'), turn=row.get('turn_id'),
            status=row.get('status'), label=row.get('source_kind'), mode=row.get('execution_mode'),
            day=turn_day.get(row.get('turn_id')) or (updated.date() if updated else None),
            lifetime=row.get('cache_lifetime_secs')))
    parents = {}
    for meta_path in sorted((directory / 'subagents').glob('*/meta.json')):
        try:
            meta = json.loads(meta_path.read_text())
        except (OSError, ValueError):
            continue
        if meta.get('child_session_id'):
            parents[meta['child_session_id']] = meta.get('parent_session_id') or usage.get('sessionId')
    primary = ledger.get('primaryModelId')
    return {'sid': usage.get('sessionId') or directory.name, 'updated': updated, 'calls': calls,
            'gaps': gaps, 'events': events, 'children': parents,
            'counters': ledger.get('cacheBreaks') if isinstance(ledger.get('cacheBreaks'), dict) else None,
            'utility_outcomes': ledger.get('utilityOutcomes') or {}, 'primary': primary,
            'primary_input': ((ledger.get('modelUsage') or {}).get(primary) or {}).get('inputTokens'),
            'input_total': ledger.get('inputTokens')}


def load(root, prices, *, days=None, now=None):
    root = Path(root).expanduser()
    cutoff = None if days is None else (now or datetime.now(timezone.utc)) - timedelta(days=days)
    paths = {path for pattern in ('*/usage.json', '*/*/usage.json') for path in root.glob(pattern)}
    if cutoff is not None:
        # A file last written a day before the cutoff cannot carry a later updatedAt.
        floor = (cutoff - timedelta(days=1)).timestamp()
        paths = {path for path in paths if path.stat().st_mtime >= floor}
    sessions = [session for session in (load_session(path.parent, prices, cutoff) for path in sorted(paths))
                if session is not None]
    parents = {child: parent for session in sessions for child, parent in session['children'].items()}
    return sessions, parents


# ---------------------------------------------------------------- dedupe

def depth(sid, parents):
    seen, level = set(), 0
    while sid in parents and sid not in seen:
        seen.add(sid)
        sid = parents[sid]
        level += 1
    return level


def dedupe(sessions, parents, excluded=()):
    """Assign each attempt once to its owning session; return threads of owned calls in file order."""
    holders = defaultdict(list)
    for session in sessions:
        for call in session['calls']:
            holders[call.attempt].append(session)
    owner = {attempt: min(held, key=lambda s: (-depth(s['sid'], parents), len(s['calls']), s['sid']))['sid']
             for attempt, held in holders.items()}
    threads, counts = {}, Counter()
    for session in sessions:
        seen = set()
        own = []
        for call in session['calls']:
            counts['rows_read'] += 1
            if owner[call.attempt] != session['sid'] or call.attempt in seen:
                counts['duplicate_rows_removed'] += 1
                continue
            seen.add(call.attempt)
            if any(start <= call.day <= end for start, end in excluded if call.day):
                counts['excluded_day_rows'] += 1
            else:
                own.append(call)
        copied = any(len({s['sid'] for s in holders[call.attempt]}) > 1 for call in own)
        kind = 'subagent' if session['sid'] in parents or copied else 'root'
        threads[session['sid']] = {'kind': kind, 'calls': own, 'session': session}
    return threads, dict(counts)


# ---------------------------------------------------------------- pricing

def cost(call, prices):
    price = prices.get(call.family) or prices['default']
    write_5m = max(0, call.write - call.write_1h)
    uncached = max(0, call.prompt - call.cached - call.write)
    return (uncached * price['input'] + call.cached * price['cache_read'] + write_5m * price['cache_write_5m']
            + call.write_1h * price['cache_write_1h'] + call.output * price['output'])


def misses(threads):
    """attempt ids of main calls whose cached tokens fell under half of the previous prompt."""
    missed = set()
    for thread in threads.values():
        previous = None
        for call in thread['calls']:
            if call.role not in MAIN_ROLES:
                continue
            if previous is not None and call.cached < MISS_SHARE * previous.prompt:
                missed.add(call.attempt)
            previous = call
    return missed


def bucket():
    return {'calls': 0, 'prompt_tokens': 0, 'cached_tokens': 0, 'cache_write_tokens': 0, 'output_tokens': 0,
            'cost_eq': 0.0}


def add(target, call, prices):
    target['calls'] += 1
    target['prompt_tokens'] += call.prompt
    target['cached_tokens'] += call.cached
    target['cache_write_tokens'] += call.write
    target['output_tokens'] += call.output
    target['cost_eq'] += cost(call, prices)


def quantiles(values):
    values = sorted(values)
    if not values:
        return {'n': 0}
    return {'n': len(values), 'mean': statistics.fmean(values), 'median': statistics.median(values),
            'p90': values[math.ceil(0.9 * len(values)) - 1]}


# ---------------------------------------------------------------- reports

def report_totals(threads, counts, prices):
    by_role, by_family, by_thread = defaultdict(bucket), defaultdict(bucket), defaultdict(bucket)
    miss_ids, missed, total = misses(threads), bucket(), bucket()
    for thread in threads.values():
        for call in thread['calls']:
            for target in (by_role[call.role], by_family[call.family], by_thread[thread['kind']], total):
                add(target, call, prices)
            if call.attempt in miss_ids:
                add(missed, call, prices)
    kinds = Counter(thread['kind'] for thread in threads.values())
    return {'sessions': len(threads), 'root_sessions': kinds['root'], 'subagent_sessions': kinds['subagent'],
            **{key: counts.get(key, 0) for key in ('rows_read', 'duplicate_rows_removed', 'excluded_day_rows')},
            'calls': total['calls'],
            'total': total, 'by_role': dict(by_role), 'by_model_family': dict(by_family),
            'by_thread': dict(by_thread), 'cache_miss_calls': missed}


def break_cause(call, previous, gap, first_of_turn):
    if call.model != previous.model or call.effort != previous.effort:
        return 'model_or_effort_change'
    if call.lifetime is not None and previous.lifetime is not None:
        if call.lifetime != previous.lifetime:
            return 'lifetime_change'
    elif call.write_1h > 0 and previous.write_1h == 0:
        return 'lifetime_5m_to_1h'
    if call.mode is not None and previous.mode is not None and call.mode != previous.mode:
        return 'mode_switch'
    if first_of_turn and gap is not None and gap > GAP_SECONDS:
        return 'gap_over_5m'
    return 'turn_start' if first_of_turn else 'mid_turn_unknown'


def report_cache_breaks(threads, prices):
    eligible, causes = 0, defaultdict(lambda: {'breaks': 0, 'uncached_tokens': 0, 'cost_eq': 0.0})
    for thread in threads.values():
        previous = None
        for call in thread['calls']:
            if call.role not in MAIN_ROLES:
                continue
            if previous is not None:
                eligible += 1
                if call.cached < MISS_SHARE * previous.prompt:
                    first = call.turn != previous.turn
                    entry = causes[break_cause(call, previous, thread['session']['gaps'].get(call.turn), first)]
                    entry['breaks'] += 1
                    entry['uncached_tokens'] += max(0, call.prompt - call.cached)
                    entry['cost_eq'] += cost(call, prices)
            previous = call
    breaks = sum(entry['breaks'] for entry in causes.values())
    report = {'heuristic': {'definition': f'main call with cached tokens under {MISS_SHARE:.0%} of the previous '
                                          'prompt on the same thread',
                            'eligible_calls': eligible, 'breaks': breaks,
                            'break_rate': breaks / eligible if eligible else None, 'by_cause': dict(causes)}}
    counted = [t['session']['counters'] for t in threads.values() if t['session']['counters']]
    if counted:
        by_cause, following = Counter(), Counter()
        for counters in counted:
            for key, value in counters.items():
                if isinstance(value, int):
                    (following if key in COUNTER_FOLLOWING else by_cause)[key] += value
        report['counters'] = {'sessions': len(counted), 'by_cause': dict(by_cause), 'following': dict(following)}
    return report


def report_rounds(threads):
    per_turn = defaultdict(list)
    for thread in threads.values():
        counts = Counter(call.turn for call in thread['calls'] if call.role in MAIN_ROLES)
        per_turn[thread['kind']].extend(counts.values())
    events = defaultdict(Counter)
    for thread in threads.values():
        if thread['session']['events']:
            for key in ('rounds', 'tool_rounds', 'poll_only_rounds'):
                events[thread['kind']][key] += thread['session']['events'][key]
    return {'main_calls_per_turn': {kind: quantiles(values) for kind, values in per_turn.items()},
            'event_rounds': {kind: {**counts, 'poll_only_share_of_rounds':
                                    counts['poll_only_rounds'] / counts['rounds'] if counts['rounds'] else None}
                             for kind, counts in events.items()},
            'poll_tools': sorted(POLL_TOOLS)}


def report_turn_start(threads):
    waits, with_jev = [], 0
    for thread in threads.values():
        turns = defaultdict(list)
        for call in thread['calls']:
            turns[call.turn].append(call)
        for calls in turns.values():
            if not any(call.role in MAIN_ROLES for call in calls):
                continue
            before = []
            for call in calls:
                if call.role in MAIN_ROLES:
                    break
                if call.role == 'jev':
                    before.append(call.duration_ms or 0)
            waits.append(sum(before))
            with_jev += bool(before)
    return {'turns': len(waits), 'turns_with_jev_before_first_main_call': with_jev,
            'serial_jev_ms_before_first_main_call': quantiles(waits),
            'basis': 'row order within a turn and api_duration_ms; Jev calls that overlap are summed'}


def report_compaction(threads, sessions, prices):
    labels = defaultdict(lambda: {**bucket(), 'statuses': Counter()})
    for thread in threads.values():
        for call in thread['calls']:
            if call.role == 'compaction':
                entry = labels[call.label or 'unlabelled']
                add(entry, call, prices)
                entry['statuses'][call.status] += 1
    for entry in labels.values():
        entry['cached_share'] = entry['cached_tokens'] / entry['prompt_tokens'] if entry['prompt_tokens'] else None
        entry['statuses'] = dict(entry['statuses'])
    decisions = defaultdict(Counter)
    for session in sessions:
        for kind, outcome in session['utility_outcomes'].items():
            if kind.startswith('compaction') and isinstance(outcome, dict):
                decisions[kind].update(outcome.get('decisions') or {})
    named = {label for label in labels if 'pass' in label or 'applied' in label}
    return {'calls_by_label': dict(labels), 'utility_decisions': {k: dict(v) for k, v in decisions.items()},
            'pass1_vs_applied': ({label: labels[label]['calls'] for label in sorted(named)} if named else
                                 'not recorded: compaction rows carry no pass-1/applied label')}


def check_totals(sessions, threads):
    """Per file: primary-model prompt tokens of its (in-file deduped) rows against session.modelUsage.
    Across files (informational): owner-deduped prompt tokens against root sessions' inputTokens."""
    within, outside, worst = 0, 0, 1.0
    for session in sessions:
        expected = session['primary_input']
        if not expected:
            continue
        seen, recomputed = set(), 0
        for call in session['calls']:
            if call.attempt not in seen and call.model == session['primary']:
                seen.add(call.attempt)
                recomputed += call.prompt
        ratio = recomputed / expected
        worst = max(worst, ratio, 1 / ratio if ratio else float('inf'))
        if abs(ratio - 1) <= TOTALS_TOLERANCE:
            within += 1
        else:
            outside += 1
    deduped = sum(call.prompt for thread in threads.values() for call in thread['calls'])
    root_totals = sum(t['session']['input_total'] or 0 for t in threads.values() if t['kind'] == 'root')
    return {'tolerance': TOTALS_TOLERANCE, 'sessions_checked': within + outside, 'within': within,
            'outside': outside, 'worst_ratio': worst if within + outside else None,
            # Informational: near 1 when every subagent's rows were folded into a loaded root.
            'deduped_prompt_tokens_over_root_session_totals': deduped / root_totals if root_totals else None,
            'ok': outside == 0}


def simulate_compaction(threads, prices, *, soft, hard, post, regrowth):
    """Replay main-call prompt sizes per thread through a compaction policy; no API calls.

    Soft threshold applies at turn boundaries, the hard cap at any call. After a
    simulated compaction the prompt is `post` plus `regrowth` times the recorded
    growth since then (never above the recorded size); cached and written
    tokens scale with the size, and the compacting call itself is a full cache
    miss written at the 5-minute rate plus a summarising call (input the
    pre-compaction size, output `post`).
    """
    base = {'prompt_tokens': 0, 'cost_eq': 0.0}
    sim = {'prompt_tokens': 0, 'cost_eq': 0.0, 'compactions': 0, 'compaction_calls_cost_eq': 0.0}
    calls_seen = 0
    for thread in threads.values():
        anchor = None
        previous_turn = object()
        for call in thread['calls']:
            if call.role not in MAIN_ROLES:
                continue
            calls_seen += 1
            base['prompt_tokens'] += call.prompt
            base['cost_eq'] += cost(call, prices)
            size = call.prompt if anchor is None else max(0, min(call.prompt, post + regrowth * (call.prompt - anchor)))
            turn_start = call.turn != previous_turn
            previous_turn = call.turn
            price = prices.get(call.family) or prices['default']
            if (turn_start and size > soft) or size > hard:
                summary = size * price['input'] + post * price['output']
                sim['compactions'] += 1
                sim['compaction_calls_cost_eq'] += summary
                anchor, size = call.prompt, min(post, size)
                simulated = call._replace(prompt=size, cached=0, write=size, write_1h=0)
                sim['cost_eq'] += summary
            else:
                scale = size / call.prompt if call.prompt else 1.0
                simulated = call._replace(prompt=size, cached=call.cached * scale, write=call.write * scale,
                                          write_1h=call.write_1h * scale)
            sim['prompt_tokens'] += size
            sim['cost_eq'] += cost(simulated, prices)
    return {'policy': {'soft_threshold': soft, 'hard_cap': hard, 'post_size': post, 'regrowth': regrowth},
            'threads': len(threads), 'main_calls': calls_seen, 'baseline': base, 'simulated': sim,
            'cost_ratio': sim['cost_eq'] / base['cost_eq'] if base['cost_eq'] else None}


# ---------------------------------------------------------------- command line

REPORTS = ('totals', 'cache-breaks', 'rounds', 'turn-start', 'compaction')


def prices_from(args):
    prices = json.loads(json.dumps(DEFAULT_PRICES))
    if args.prices:
        for name, entry in json.loads(Path(args.prices).read_text()).items():
            prices[name] = {**prices.get(name, prices['default']), **entry}
    for item in args.price or []:
        key, _, value = item.partition('=')
        name, _, field = key.partition('.')
        if field not in PRICE_FIELDS:
            raise SystemExit(f'--price expects FAMILY.FIELD=VALUE with FIELD in {", ".join(PRICE_FIELDS)}')
        prices.setdefault(name, dict(prices['default'], match=[name]))[field] = float(value)
    return prices


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--sessions-root', type=Path, default=DEFAULT_ROOT)
    parser.add_argument('--days', type=float, help='only sessions updated in the last N days')
    parser.add_argument('--exclude-days', action='append', help='date ranges to leave out: START[:END],...')
    parser.add_argument('--prices', help='JSON file: {family: {match: [...], input, cache_read, ...}}')
    parser.add_argument('--price', action='append', help='override one price, FAMILY.FIELD=VALUE')
    parser.add_argument('--report', action='append', choices=REPORTS)
    parser.add_argument('--check-totals', action='store_true',
                        help='exit 1 unless deduped primary-model prompt tokens are within 10%% of usage.json')
    parser.add_argument('--simulate', choices=('compaction',))
    parser.add_argument('--soft-threshold', type=int, default=150_000)
    parser.add_argument('--hard-cap', type=int, default=400_000)
    parser.add_argument('--post-size', type=int, default=40_000)
    parser.add_argument('--regrowth', type=float, default=1.0)
    args = parser.parse_args(argv)
    prices = prices_from(args)
    sessions, parents = load(args.sessions_root, prices, days=args.days)
    threads, counts = dedupe(sessions, parents, parse_day_ranges(args.exclude_days))
    output = {'sessions_root': 'default' if args.sessions_root == DEFAULT_ROOT else 'argument'}
    reports = args.report or ([] if args.check_totals or args.simulate else ['totals'])
    for name in reports:
        output[name] = {'totals': lambda: report_totals(threads, counts, prices),
                        'cache-breaks': lambda: report_cache_breaks(threads, prices),
                        'rounds': lambda: report_rounds(threads),
                        'turn-start': lambda: report_turn_start(threads),
                        'compaction': lambda: report_compaction(threads, sessions, prices)}[name]()
    if args.simulate:
        output['simulate-compaction'] = simulate_compaction(threads, prices, soft=args.soft_threshold,
                                                            hard=args.hard_cap, post=args.post_size,
                                                            regrowth=args.regrowth)
    if args.check_totals:
        output['check-totals'] = check_totals(sessions, threads)
    print(json.dumps(output, indent=2, default=str))
    return 1 if args.check_totals and not output['check-totals']['ok'] else 0


if __name__ == '__main__':
    raise SystemExit(main())
