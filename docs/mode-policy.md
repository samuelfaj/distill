# Mode policy table

The execution modes (Normal, UltraFast, UltraCode, UltraSave) differ in what
the harness does, not in the system prompt. The policy table is the one place
that says what each mode makes the harness do. It lives beside the mode enum
in `crates/common/distill-tool-types/src/execution_mode.rs` as `ModePolicy`.

## Rows

A row is one mode's settings, as typed fields. Each mode has two rows:

- `today`: what the code does now. Where the value lives in a constant, the
  row and the lever read the same constant (the Jev answer deadline and the
  quantile at which a Jev choice is read).
- `v2`: the target row. Normal's `v2` row equals its `today` row.

The `v2` rows are declared but most are not enforced yet: the levers still
read their own branches, and later changes wire them to the table one at a
time. The one `v2` value enforced today is UltraCode's turn-end verification,
the independent review described in [UltraCode](ultracode.md).

## Two views

- The runtime view follows the effective mode, so it also applies under Auto:
  effort cadence, quantile, ceiling and floor, the effort-lowering pass, the
  effort used when Jev is silent, tier routing, Jev waits and deadlines,
  utility selection of tool results, cold-return and prompt-cap behaviour,
  read narrowing, turn-end verification, the escalation ladder, subagent depth
  and the skill descriptor cap.
- The prefix view holds what changes the cached prompt prefix: the lean tool
  list, the directive, the per-mode model key and the provider-tier key. It
  applies only to a pinned mode; under Auto it is absent, because a per-mode
  prefix would re-read the prompt uncached at every mode switch.

## Selecting v2 rows

```toml
[modes]
v2 = ["ultrasave", "ultrafast", "ultracode"]   # the default when the key is absent
```

`[modes] v2` lists the modes that use their `v2` row. With no key, UltraSave,
UltraFast and UltraCode all use it. A list is authoritative: a mode left out
goes back to its `today` row, and `v2 = []` puts every mode on `today`.
Unknown names are ignored with a warning. UltraCode on its `v2` row runs the
UltraCode turn review; with UltraCode left out, `[modes.ultracode] contract =
"v2"` still turns the review on by itself. No `v2` row has been through a
benchmark campaign yet.

## What makes the modes distinct

The modes are meant to differ in what the harness does, and the difference is
tested. `ModePolicy::differing_fields` names the harness-enforced fields in
which two rows differ; `effort.lean` and `prefix.directive` are left out
because they are text sent to Jev or the model, not enforcement. A unit test
requires every pair of `v2` rows to differ in at least five fields, and in at
least five runtime fields, which also apply under Auto. The `v2` rows differ
in:

| Pair | Fields | Differing fields |
|---|---|---|
| Normal, UltraFast | 15 (12 runtime) | `effort.cadence`, `effort.ceiling`, `effort.lowering_pass`, `effort.when_silent`, `tier_routing`, `jev.await_turn_start`, `jev.diff_risk_deadline_ms`, `utility_selection`, `context.cold_return`, `escalation`, `subagent_depth`, `skill_descriptor_cap`, `prefix.lean_tools`, `prefix.model_key`, `prefix.provider_tier_key` |
| Normal, UltraCode | 8 (8 runtime) | `effort.quantile`, `effort.floor_after_failure`, `effort.lowering_pass`, `effort.when_silent`, `context.cold_return`, `context.narrow_reads`, `turn_end`, `subagent_depth` |
| Normal, UltraSave | 15 (12 runtime) | `effort.cadence`, `effort.quantile`, `effort.ceiling`, `effort.lowering_pass`, `effort.when_silent`, `tier_routing`, `context.mode_keys`, `context.cold_return`, `context.max_prompt_tokens`, `escalation`, `subagent_depth`, `skill_descriptor_cap`, `prefix.lean_tools`, `prefix.model_key`, `prefix.provider_tier_key` |
| UltraFast, UltraCode | 18 (15 runtime) | `effort.cadence`, `effort.quantile`, `effort.ceiling`, `effort.floor_after_failure`, `effort.when_silent`, `tier_routing`, `jev.await_turn_start`, `jev.diff_risk_deadline_ms`, `utility_selection`, `context.cold_return`, `context.narrow_reads`, `turn_end`, `escalation`, `subagent_depth`, `skill_descriptor_cap`, `prefix.lean_tools`, `prefix.model_key`, `prefix.provider_tier_key` |
| UltraFast, UltraSave | 11 (11 runtime) | `effort.cadence`, `effort.quantile`, `effort.ceiling`, `tier_routing`, `jev.await_turn_start`, `jev.diff_risk_deadline_ms`, `utility_selection`, `context.mode_keys`, `context.cold_return`, `context.max_prompt_tokens`, `escalation` |
| UltraCode, UltraSave | 17 (14 runtime) | `effort.cadence`, `effort.quantile`, `effort.ceiling`, `effort.floor_after_failure`, `effort.when_silent`, `tier_routing`, `context.mode_keys`, `context.cold_return`, `context.max_prompt_tokens`, `context.narrow_reads`, `turn_end`, `escalation`, `subagent_depth`, `skill_descriptor_cap`, `prefix.lean_tools`, `prefix.model_key`, `prefix.provider_tier_key` |

A field that differs in the table is a field the row declares; whether the
lever reading it is wired is a separate question (see Rows above).

### Directives

A mode on its `v2` row states a short directive: its objective in one
sentence, then only what that row enforces or what the model must do itself
because the harness cannot, such as testing the stated requirements before
finishing. None of them gives delegation advice, and none repeats a sentence
of another. UltraCode's says that the harness reviews the turn's code changes
independently at the end and appends the verification status to the final
message; a subagent, whose turns are not reviewed, gets it without that
sentence. These directives are the default, because every mode is on its
`v2` row unless `[modes] v2` says otherwise; `v2 = []` brings back the
previous directives byte for byte, including the UltraCode variant without
delegation sentences for a session that cannot spawn.

### Mechanical signature, campaign gates and merge rule

A benchmark run of a pinned mode is checked for the mode's signature from its
own records (`mode_signature.py`): UltraSave rounds stay at or below high
unless the run escalated and no one-hour cache write happens; UltraFast turns
carry `ultrafast:jev_wait_skipped` and no utility selection runs on a tool
result; UltraCode's reviewed turns carry a `review_verdict` row and the
effort-lowering pass is never asked. Each check reads `PASS`, `FAIL` or
`NO DATA` with its counts.

In a matrix campaign (`modes_verdict.py`), each mode's own metric must beat
Normal by more than twice the A/A deviation: credits for UltraSave, charged
time for UltraFast, accepted runs on substantial cases for UltraCode. Each
mode's declared loss stays under its ceiling: UltraSave time at most 1.5x
Normal; UltraFast credits at most 1.15x Normal, or 2.5x on a fast provider
path; UltraCode credits at most 3.0x and time at most 2.5x on substantial
cases. The merge rule reports, and never applies, `MERGE: <mode> is
dominated by <mode>` when one lean mode beats the other on credits and on
time beyond the A/A deviation, and `MERGE: <mode> duplicates <mode>` when two
modes stay within the A/A deviation on credits, time and accepted runs; a
merged mode's claim reads `no claim yet`. The details are in
[task-cost evaluation](task-cost-evaluation.md#matrix-campaigns).

No campaign has run with these gates, the signature or the new directives
yet; nothing here is a measured result.

## Policy rows

Each time a policy rule acts, the harness records a policy row: a count under
`utilityOutcomes["policy:<mode>"]` in the session's `usage.json` and a
`mode_policy` event in `events.jsonl` with `seq`, `mode`, `rule` and `round`.
The rule is a short identifier; anything else is replaced by its hash. The
rules recorded today: `review_verdict:reviewed` or
`review_verdict:unverified` when the UltraCode turn review ends;
`compaction:cold_return` or `compaction:max_prompt` when a mode's row or
`[modes.<mode>]` key decided a compaction that the `[compaction]` keys alone
would not have; `cold_return_skipped` when the row suppressed a question or a
compaction those keys would have made; `read_not_narrowed` (at most once per
round) when the row kept a file read whole; and `provider_tier_rejected` when
an endpoint refused a pinned session's serving-path field and the session
dropped it.

## Context policy

The cold-return compaction, the absolute prompt cap and file-read narrowing
read the effective mode's row, so they apply under Auto too. On by default
(every mode is on its v2 row); `[modes] v2 = []` turns them off.

| Row | Cold return | Prompt cap | File reads |
|---|---|---|---|
| Normal (both rows), every `today` row | `[compaction]` values | `[compaction]` value | may be narrowed |
| UltraSave v2 | `auto`, from the price-table floor | 250,000 tokens | may be narrowed |
| UltraFast v2 | `off` | none | may be narrowed |
| UltraCode v2 | `offer` | none | never narrowed, in children too |

On the `today` rows `[modes.ultrasave]` still replaces the `[compaction]` keys
in UltraSave. Precedence on the v2 rows: an explicit `[modes.<mode>]`
`cold_return` or `max_prompt_tokens` wins, then the row's value, then the
`[compaction]` key; `0` turns a cap off. The percent threshold applies in
every mode, and the cap fires only before a human turn's first request. The
250,000-token cap and UltraSave's automatic compaction have not been
benchmarked: compacting more often can lower the pass rate on long tasks.
Details and counters are in `docs/token-saver.md` ("Context by mode").

## Turn outcome record

When a root prompt turn ends, the harness writes one turn outcome record. It
goes to `events.jsonl` as a `turn_outcome` event, to the turn's row in
`usage.json` as `outcome`, and to the client with the turn-end usage as
`turnOutcome`. Older `usage.json` files without it still load. Subagent turns
write none.

| Field | Meaning |
|---|---|
| `mode_choice` | `auto` or the pinned mode |
| `mode_rounds` | main-loop rounds of the turn by effective mode |
| `verification` | `reviewed` (the UltraCode review passed), `checked` (the latest recorded check passed on the current working tree) or `unverified` |
| `verification_reason` | for `unverified`: `no_check`, `check_failed`, `check_unknown`, `check_stale` or `review_unverified` |
| `checks` | recorded checks of the turn by outcome: `passed`, `failed`, `unknown` (an identical rerun with the same outcome counts once) |
| `escalations` | escalations the turn made (0 until an escalation ladder runs) |
| `policy` | policy rows of the turn by `<mode>:<rule>` |
| `child_calls`, `child_api_ms`, `child_cost_usd_ticks` | the subagents' model calls, API time and cost; cost is absent when a call did not report one |
| `blocked` | what kept the mode from acting: `no_cheaper_model` (UltraSave pinned without a cheaper model), `jev_silent` (Jev off or without a credential), `effort_pinned` (an explicit effort), `auto_runtime_only` (under Auto only the runtime view applies), `tier_routing_off` (tier routing is not running) |

Plain headless output prints the record as one line on stderr after the
answer, and the interactive UI adds it to the turn's distribution block.

## Changing mode mid-session

An explicit `/mode` change applies at once. The first time it happens in a
session whose last main prompt had 16,384 tokens or more, the client gets one
`x.ai/session/execution_mode/cache_cost` notice with the size the next request
may read uncached.
