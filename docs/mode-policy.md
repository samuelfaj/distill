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
v2 = ["ultrasave", "ultrafast", "ultracode"]
```

`[modes] v2` lists the modes that use their `v2` row; the default is the empty
list, which keeps every mode on `today`. Unknown names are ignored with a
warning. Listing `ultracode` also turns on the UltraCode turn review, as
`[modes.ultracode] contract = "v2"` already does.

## Policy rows

Each time a policy rule acts, the harness records a policy row: a count under
`utilityOutcomes["policy:<mode>"]` in the session's `usage.json` and a
`mode_policy` event in `events.jsonl` with `seq`, `mode`, `rule` and `round`.
The rule is a short identifier; anything else is replaced by its hash. Two
rules are recorded today: `review_verdict:reviewed` or
`review_verdict:unverified` when the UltraCode turn review ends, and
`compaction:cold_return` or `compaction:max_prompt` when a
`[modes.ultrasave]` key decided a compaction that the `[compaction]` keys
alone would not have.

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
