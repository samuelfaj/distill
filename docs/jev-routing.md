# How Jev routes work

Jev is Distill's decision layer. It answers structured questions about a small
state assembled by the harness: which model should handle a call, how much
effort it needs, or which parts of a tool result are worth keeping.

Of the tiers set in [Choose your models](../README.md#choose-your-models), the
**main** model is required and owns every session: it plans the work, delegates
the implementation, and reviews what comes back. The optional **worker** model
runs the subagents the main model delegates to. Utility tasks receive a bounded
payload, such as a tool result, log excerpt, or candidate list, instead of the
full conversation.

UltraCode is a separate session mode. It permits bounded recursive task
delegation when useful, while model and effort selections keep their existing
precedence. See [UltraCode](ultracode.md) for activation, runtime limits and paired
off/flat/hierarchical comparisons. Turning it on does not guarantee delegation
or a cheaper result.

```text
   Main model: owns the session, plans, delegates, reviews
      |                     |                        |
  conversation      delegated chunk          Utility task
                  (worker subagent)         bounded payload
                  report + diff -> main reviews
```

Jev chooses among candidates supplied by code. It does not invent candidates
or decide permissions. Distill owns plan mode, auto approval, YOLO, and
permission policies; Jev cannot approve, veto, or hold a tool call for
confirmation. If a Jev decision fails, times out, or lacks enough confidence,
the harness keeps its normal execution path.

## Main and worker

The main model owns the conversation for the whole session. When a worker model
is configured, the main model's system prompt carries an `<orchestration>`
section that makes the worker the default executor, small changes included: the
main model gives it every assignment a precise spec fully determines (implementing a specified
change, writing tests for specified behavior, mechanical or repetitive edits,
running builds and tests, and finding or summarizing code) and keeps what needs
judgment no spec can carry (unclear requirements, design decisions, finding the
cause of an unexplained failure, security-sensitive choices). Each assignment is
written as a spec a weaker model can follow without guessing: goal, exact files
and functions, the change with signatures and an example, what must stay
unchanged, conventions, acceptance criteria, and the exact verifying command
with its expected result. The main model judges each result by its diff and
check output, sends a failure back with the concrete error, splits or takes over
an assignment that fails twice, and runs the final check itself. A request that
already states the change goes to the worker before the main model reads the
code, a lone assignment runs in the foreground (`background: false`) so its
result returns in the same call, and the main model edits files itself only to
fix a few lines found in review or to finish an assignment the worker failed
twice. Without a
worker model, or when the worker is the main model, the section is left out and
the main model does the work itself.

A fresh child on the worker model starts with a worker instruction from the
harness: do exactly what the assignment specifies and change nothing else; when
the code does not match the assignment or it leaves open a decision that
changes the result, stop and report instead of guessing; run the checks it
names and report each exit status and relevant output.

Model choice for subagents:

| Subagent | Model |
|---|---|
| A fresh subagent the main model delegates (`general-purpose`, `explore`, a user agent without a `model:`) | the worker model; the main model when none is set |
| `plan` and `code-reviewer` | the main model, unless Jev routes a simple task to the worker |
| A full-context fork, a resumed subagent, or one spawned with an explicit `model` | its own model (the parent's for a fork) |
| Harness roles (a goal's planner, verifiers, strategist, summarizer) | the main model; the summarizer tries the utility first, and the progress checkpoint skips its call when the harness sees new work (see `docs/token-saver.md`) |

An explicit `[subagents.models]` pin or an agent definition's `model:` wins over
the worker default. A worker model missing from the catalog falls back to the
main model.

With `[jev.ladder] e_cheap_agent = true` (off by default), a fresh `explore`
child the main model delegates with no model of its own runs its rounds on the
utility model when that model is a catalog entry (a raw OpenRouter chain does
not qualify). The child keeps the model the table gives it as its fallback: a
failed utility request (rejection, rate limit, timeout, server error), a utility
that is missing, lists no tool calling, or whose window the conversation outgrows,
or a conversation that carries a secret, moves the round and the rest of the child back to it; a round
the user cancelled or rewound is not resent. Workflow children and children with an output budget stay
on their model. After a utility child
fails, the parent's later children skip the utility. A child that answers that
it cannot do the task is not retried yet; its report reaches the main model as
it is.

A delegated worker cannot delegate further (the subagent depth limit is one). Its
final message is the report the main model receives: the outcome, then the
evidence (`path:line` references and each command with its result), then what it
could not verify.

Both tiers take an effort level or `auto`, and `auto` lets Jev pick the effort
for every call from that model's own menu:

- Main model: `/model <model> [effort|auto] [variant]` and `/effort <level|auto>` set this
  session. The Model tiers screen (`/tiers`, entered as `model effort`) also saves
  it for new sessions: `auto` as `[jev] effort_auto = true`, a level as
  `[jev] effort_auto = false` plus `[models].default_reasoning_effort`.
- `variant` is OpenRouter only: `floor` (default, cheapest provider), `nitro`, `exacto` or `none`; it is saved as `[models].main_variant` or `worker_variant`.
- Worker model: `/worker-model <model> [effort|auto] [variant]` or the Model tiers screen
  saves `[models].worker_effort`; unset means `auto`. It applies to every child
  that runs on the worker, independent of the main model's mode, unless the
  caller, a role or the agent definition set an effort. `/worker-model clear`
  removes the worker.

Explicit subagent model and effort policies remain pinned.

Jev re-picks effort on every main model call from that model's supported menu.
It takes the cheapest level that holds at least half the probability mass, with
no confidence floor. The pick follows an execution-mode policy: UltraSave leans
lower, Normal is balanced, UltraFast is fastest, and UltraCode leans higher. A
short provider note describes how that provider's levels behave. When Jev gives
no usable answer, the call uses a mode baseline on the model's menu: UltraSave
and UltraFast use low, Normal uses medium, and UltraCode uses high, moving to
the nearest offered level. It never raises a lower pinned effort, and it lowers a higher one such as max.
A redo can raise effort when the previous attempt lacked reasoning. A fixed
effort, selected in a picker or with `/effort <level>`, takes precedence over
automatic effort selection.

```toml
[models]
default = "chatgpt/gpt-6-sol"    # main model (required)
worker = "chatgpt/gpt-6-luna"    # worker model (optional)
worker_effort = "auto"           # worker effort: auto or a level

[jev]
effort_auto = true               # main model effort: auto
```

Older configs are migrated at startup. A `[models].reasoning` model becomes
`[models].default`, and the previous main model becomes `[models].worker`. The
previous main model's `default_reasoning_effort` never reaches the new main
model: when auto effort was off it becomes `[models].worker_effort`, and under
auto (where it was only the fallback) it is dropped. A `[jev.tiers] light` model
becomes `[models].worker`, and a fixed `light_effort` becomes its
`worker_effort`.

Jev's change review (C4) judges each executed edit. When it wants an independent
look, the main model is told to ask the read-only `code-reviewer`; a delegated
worker is told to name the edit and its risk in its report instead, so the main
model reviews it. The built-in `code-reviewer` inspects a substantive code
checkpoint with a fresh, read-only context on the main model, unless pinned
through `[subagents.models]`. Give it the diff, acceptance criteria and test
evidence. Goal completion still uses its existing verifier; a goal's planner and
round-end evaluation run on the main model, and the mid-turn progress
checkpoint skips its call when the harness sees new work, else runs on main.

```toml
[subagents.models]
code-reviewer = "reviewer-catalog-entry"
```

The value must name an existing model catalog entry.

The routing and subagent choices are hypotheses about quality and cost. To
claim a saving, compare accepted tasks with and without delegation, including
all main, worker, subagent, utility, and retry usage. Test outcomes and the
delivered behavior must be compared alongside cost; token counts alone do not
establish a financial saving.

## Optional model facts and cache

Jev enriches configured candidates with OpenRouter's public model catalog:
context, tool/reasoning support, pricing (including cache reads), and available
Artificial Analysis coding, agentic, and intelligence scores. The public catalog
works without OpenRouter login. With an existing OpenRouter key, the benchmarks
API supplies additional scores. No login is required or prompted for this feature.

OpenRouter candidates also receive provider endpoint facts: recent uptime,
latency, throughput, and implicit-cache support when published. These are hints
about available endpoints, not a guarantee of which provider will serve a request.
Prices and endpoint capabilities apply only to OpenRouter routes, never to
ChatGPT/Grok subscriptions or direct-provider billing. Exact model IDs (and the
catalog's canonical mapping) are used; missing matches remain unknown. Benchmark
scores do not imply a measured benefit from a particular reasoning effort.

`jev/model-facts-v1.json` under the active profile stores the cache. Catalog and
benchmark entries refresh after 24 hours; endpoint entries after 15 minutes.
Refreshes run in the background, with atomic writes and a nonblocking process lock.
Failures retain the previous data and back off for an hour (15 minutes for
endpoints). Catalog/benchmark facts expire after seven days; endpoint facts after
one hour. Routing reads the in-memory snapshot and never waits for these HTTP
requests. Offline, missing-key, and invalid-response cases keep normal routing.

Only public model IDs are queried; task content is not sent to metadata endpoints.
Jev considers total task cost, retries, and possible loss of prompt-cache reuse
when switching models. Benchmark and price hints do not override the configured
candidate set, context guard, explicit effort, or permission policy.
Every request that replays the main prefix routes on the main call's key: the
session ID, the parent's ID for a verbatim fork, or `{group}:{type}` for a fresh
subagent. That covers compaction pass 1, recap and `/btw`, which also send the
effort the last main round used. Their conversation ID stays the session's own,
so sibling subagents never share a Codex thread. Calls with a different prompt
(goal evaluation, compaction pass 2, memory capture, dream, classifiers, prompt
suggestion) route on a stable `{session}:{purpose}` key, so they do not evict
the main prefix and can reuse their own. Responses requests send the key as
`prompt_cache_key`. OpenRouter requests send it as the body `session_id`, as
`prompt_cache_key` and as the `x-session-id` header (unless you set that
header), and `anthropic/*` and `google/gemini*` models get `cache_control`
breakpoints there. On ChatGPT (Codex), the first response of a turn also issues
an `x-codex-turn-state`, which the turn's later rounds send back.

With auto effort, a turn keeps the effort its first round picked on backends
whose cache is keyed by effort (everything except Messages models with the
per-message effort marker); a new turn, a model switch, a compaction or a
prompt under 16K tokens frees it. On `api.anthropic.com` every request keeps
the five-minute cache lifetime by default: moving a warm five-minute prefix to
the hour re-writes everything after the last one-hour entry at twice the input
price, since a five-minute hit does not count when a later one-hour breakpoint
misses. `[cache] extended_ttl = "auto"` restores the per-round choice, where a
round that blocked on a task or subagent, or started background work, asks for
the one-hour lifetime (a refusal turns it off for the process once the resend
without it goes through). The free fourth
breakpoint anchors on a former tip that moves every 10 rounds. Rewrites of old
history (soft trims, the retained hard clear, old goal directives) wait for a
cold moment: idle past the last request's cache lifetime (5 minutes on
Messages, 1 hour when asked, 30 minutes on Codex, 10 on Grok and OpenRouter,
and with `extended_ttl = "auto"` never within the hour after a one-hour write,
which later requests still read through), a compaction, a model switch or a
resume. A verbatim fork inherits its parent's
soft trims. Each turn also writes `cache_state.json` in the session directory
(the wall-clock time of the last main request, its cache lifetime and the soft
trims sent), so a session reloaded inside that lifetime, or a subagent resumed
inside its source's, repeats the same trims and is not cold; past the lifetime,
with system prompt updates to fold into the head, or without a readable file,
the resume is cold as before.

When you type into a session of 100K tokens or more that sat idle past that
lifetime (a known one only; the hour fallback never counts), the request about
to go out writes the whole history at full price anyway, so the turn first asks:
Compact and continue, Keep full history, or Don't ask again, which writes
`[compaction] cold_return = "off"`. `cold_return = "auto"` compacts without
asking, and `"off"` turns the offer off. A history already at the
auto-compact threshold compacts as usual, and subagents, `--no-ask-user`, a
client without the question UI and no answer within two minutes keep the full
history.

The offer also covers a session reloaded past the lifetime `cache_state.json`
recorded, before it sends anything; a missing or unreadable file is not a cold
moment. A headless session cannot be asked, so under the default `"offer"` it
keeps its history and only `"auto"` compacts it. Leaving `cold_return` unset
behaves as `"offer"`. This default has not been benchmarked: there is no
pass-rate evidence yet that compacting more often keeps tasks passing, and each
compaction replaces a long history with a short summary.
`[compaction] cold_return_min_tokens` moves
the 100K floor, which assumes a cache read at 0.1x of input; at 0.05x (Opus
and Sonnet) or 0.025x (Fable and Mythos) the same payback needs about twice or
four times the history. `[compaction] max_prompt_tokens` is an absolute cap
beside `auto_compact_threshold_percent`: a history above it compacts before the
first request of a human turn, never mid-turn; unset or `0` is off. While the
effective mode is UltraSave, `[modes.ultrasave] cold_return` and
`[modes.ultrasave] max_prompt_tokens` replace those two values when set.
Subagents never compact here. `usage.json` counts each compaction these keys
decide under `compaction_timing` (`compact:cold-resume`,
`compact:cold-headless`, `compact:max-prompt`).

The main system prompt keeps its per-session values, the worker model id and
the memory roots, in an `<environment>` block at its end, so sessions of one
agent on other workspaces or workers share every byte before it; a resumed head
that names a stale worker has only that block and the `<orchestration>` section
replaced. Tool definitions are sorted by name, so registration and MCP connect
order never change the tools array that opens the prefix.

A system prompt that changes mid-session (another mode, `/memory`, a new worker)
no longer rewrites the opening prompt on `api.anthropic.com` models that take
system-role messages (Opus 4.8, 5 and 5.5, Sonnet 5.5, Haiku 5.5, Fable and Mythos 5 and
5.1; not Sonnet 5). The new prompt is appended to the history at a turn boundary
and sent as a system message after the next user turn, saying it replaces the
earlier instructions, so everything cached before it stays; reverting appends
the original again, and a rewind that cuts an update appends it again. A cold moment (above) folds the updates back into the
opening prompt, and a resume does so before the head is reconciled. A change in
the middle of a tool round, turning memory off while remembered notes are in the
prompt, another endpoint or model, and every request after the API refuses a
system message (which turns them off for the process once the resend without
them goes through) send the latest prompt as the opening one, as before.

On the same models and endpoint, an optional tool family (`p1_tool_family`) no
request has needed yet is declared in the tools array with `defer_loading`
from the first request, under the `mid-conversation-tool-changes-2026-07-01`
beta. When a later request needs it, a `tool_addition` system message after
that human turn offers its tools, so the tools array and everything cached
before the join stay. A family needed by the first request just joins the
array. The model never sees a deferred tool, and a call that names one anyway
gets an error result instead of running. Plan mode, another endpoint or model,
an addition with no user turn to follow, and every request after the API
refuses a deferred tool or the beta (which turns them off for the process once
the resend without them goes through) send the tools in effect, as before.

None of this guarantees a cache hit; compare `cachedReadTokens` in `usage.json`
before and after. `/usage` shows the hit rate (cache reads over the whole prompt,
per model and in total), the cache writes split by lifetime (`cacheCreation1hTokens`
in `usage.json`; a write whose lifetime the endpoint does not report counts as
five-minute), an estimated saving in dollars for OpenRouter calls whose catalog
prices are known, net of the cache-write premium (shown as a loss when the
writes cost more than the reads saved), and the last cache break of a main
Messages request: the first history item that changed, the tools or settings
when every item was intact, or "expired" when nothing changed and the gap
outlived the previous request's cache lifetime ("not expired" inside it: the
system prompt the sampler sent changed, or the entry was evicted). muse-spark on OpenRouter has a single provider (Meta), and its
hits and misses alternate inside that provider (about 40% of prompt tokens
cached) even though the session key reaches it, so no request field fixes it.

Ignored tests check this against the real APIs and spend real tokens, so they
run only when asked:
`cargo test -p distill-shell --lib real_api_cache -- --ignored --nocapture`.
A multi-step tool loop must read the whole previous request from cache on every
request after the first (less a 256-token tail), on Anthropic with your Claude
login, on OpenRouter with `OPENROUTER_API_KEY` (Claude Haiku 4.5 by default)
and on ChatGPT with your ChatGPT login. On Anthropic, the request after a tool
family joins through `tool_addition`, and the one after a system prompt update,
must read everything before them, and the model must follow the new prompt.
Each provider without a credential is skipped with a message; the stored logins
are only read, never refreshed, so an expired one skips too until Distill runs
again. `DISTILL_CACHE_TEST_ANTHROPIC_MODEL`, `DISTILL_CACHE_TEST_OPENROUTER_MODEL`
and `DISTILL_CACHE_TEST_CHATGPT_MODEL` pick other models. Prompts sit just past
each model's minimum cacheable length, so a run costs about a cent on OpenRouter.

## Utility work

Two utility tasks are allowed. `select_units` picks the IDs of labelled source
units (lines, JSON elements, skills, tool families) that answer a question, and
the harness copies those units verbatim; an ID outside the supplied set rejects
the answer. `display_text` writes text a person reads and the main model never
relies on: a title, a shell completion, a prompt suggestion, a recap, a goal's
closing summary. The laziness classifier sends its own request to the utility
model and keeps only a verdict that parses; opt-in `explore` children run whole rounds on it.
Every rejected, failed or late answer falls back to the normal path (the table
in [Where the token savings come from](token-saver.md#every-utility-use-at-a-glance)
names each fallback). The other tasks in `jev/tasks.rs` are registered but not
wired.

You can configure a single model entry or an ordered OpenRouter fallback chain:

```toml
[jev.local]
model = "inclusionai/ling-3.0-flash-vl:free,inclusionai/ling-3.0-flash-vl,qwen/qwen3.7-flash"
max_context_tokens = 262144
notes = "Extraction, summaries, and mechanical edits."
```

A comma-separated chain is tried in order. Model availability and charges come
from the provider. Keep API keys in the environment or use provider login;
do not paste them into this example.

An explicit `[jev.local] model` is the only candidate: if it cannot run, there
is no utility lane. When it is unset, the utility model is the first of these
that can run: the shipped OpenRouter chain (so an install that already had a
utility keeps it), then the `[models] session_summary` or `prompt_suggestion`
pin (a catalog id, often on a subscription) when that chain has no key. The
worker is never used, because it is priced like a main model. A candidate that
is the session's own model is skipped. So is the model a caller falls back to:
the summary model for the title, the suggest model for a prompt suggestion. The
session title and `ask_stored_output` use the same resolver. When nothing
resolves, utility work keeps its previous path and the log says so once. A
sampler-backed lane gives up after 20 s, the closed client's request timeout,
and the caller keeps today's bytes.

No route hands a whole main-session call to the utility model: a round on
another model replays the history uncached. `b2_local_model` is off and reads
nothing; only the `e_cheap_agent` explore children above run whole rounds on
it. The `e_retention` route breaks large outputs into blocks
and decides what to retain before discarding the original text, with special
handling for secrets. It is off by default and, when on, runs only on a result
utility selection (`e_cheap_compress`) left as it was, so a result pays for one
selection. The utility transport supports catalog models on the sampler stack as well as the closed client. Missing decisions keep the existing behavior.

## New routing and context levers

| Lever | Decision | Confidence floor |
|---|---|---:|
| `d4_compaction_timing` | Whether the next step is independent enough to compact now | 0.75 |
| `d5_memory_capture_gate` | Whether the turn produced durable knowledge to capture | 0.70 |
| `b7_subagent_model` | Whether the worker model can do a subagent task as well as the main model | 0.75 |

A missing or uncertain answer keeps today's behavior. D4 also reads the end of
the last assistant message and the todo statuses. P3 now sends previews to Jev
before compaction, only for an input that is cold anyway and will be sampled. C4 sends the change once for review. B1's intent reaches
B2 as `turn_intent`.

Retired levers are `e_cheap_task`, `e_lane_choice`, `e_breaker`, `a3_log_lines`,
`c2_failure_triage` and `c7_change_type`. Their configuration keys are ignored,
and config load warns once per key that it is retired. Failure triage (C2) and
change categorization (C7) now ride along as extra questions in the C5 and C4
requests, under those levers' switches; their answers only add advisory hints.

These levers are on by default:

| Lever | Decision | Effect |
|---|---|---|
| `b6_delegation_hint` | Whether the turn has independent work for a worker | A delegation hint, only when a worker model is configured |
| `b2_model_tier` | Whether a call can run on a cheaper tier | Downgrades only, and only in automatic effort mode |
| `c1_premature_stop` / `c3_completion_check` | Whether the main turn is ending with requested work or proof missing | One advisory reminder per user turn, then the turn ends normally |

## Auto mode

Under Auto one Jev question (`b8_execution_mode`) picks the effective mode
when a request starts, and a review after each completed tool round may move
it. Two `[jev.ladder]` keys shape both, and both are on by default:

- `b8_criteria_v2`: the question judges the whole request, not the next step.
  Each option is an object with the same three fields: what the mode is for,
  what it is not for, and two or three example requests. UltraSave covers
  mechanical, fully specified work that is cheap to verify; UltraFast a quick
  answer or a small fix where waiting is the cost; Normal ordinary multi-step
  work; UltraCode changes where a wrong result is expensive (hidden
  invariants, migrations, concurrency, public contracts). The state carries
  the request once, the mode in force and the switch cost as numbers; from a
  session's second request on it adds how the previous request ended (failed
  tool calls, and checks passed, failed and unknown), and a review adds its
  progress and latest step. UltraSave is left out of the options only when it
  could change nothing here: no cheaper model, no tier routing, and a runtime
  row that equals Normal's apart from tier routing (every shipped row differs,
  so in practice it stays). Below the request-start floor (0.0 until it is
  fitted) a session's first request falls to Normal and later requests keep
  their mode. With `[jev] shadow_packs = true` the first wording rides the
  same request and is recorded as `shadow:mode_v1:<choice>`, so the two can be
  compared from recorded decisions. `false` restores the first question and
  state exactly.
- `b8_up_only`: inside a request the mode only moves up (UltraSave or
  UltraFast to Normal, Normal to UltraCode): by a review whose probability for
  the higher mode exceeds the current mode's by at least the climb margin
  (0.25, `mode_review_up_margin` in the thresholds table), or by the
  escalation ratchet. A leaner or sideways answer is recorded as `keep` and
  waits for the next request, whose pick may choose it. At UltraCode the
  review asks nothing. In UltraFast on its v2 row (no turn-start waits) the
  review does not hold the next main request: it runs on its own task and its
  answer applies at the first review after it arrives; a new request drops
  it. A mid-turn switch appends the mode directive only when the model has
  something new to read: the tools array changed, or the live history after
  the last compaction does not already end on the new mode's directive.
  Normal and UltraSave turns under Auto take the UltraCode review's snapshot
  before their first tool call, so a climb to UltraCode ends the turn through
  that review and the review sees the edits made before the climb; a climb
  from UltraFast is reviewed from the first tool round after it.
  `b8_review_hygiene` still applies to the climbs that remain. `false`
  restores the review that moves either way.

Both keys are on by the owner's choice. Neither the new criteria nor the
up-only rule has been through the replay, shadow and paired-benchmark
sequence below; that work is still to be done.

## Escalation ratchet

UltraSave and UltraFast give up capacity on purpose (a cheaper tier, a lower
effort), so they need a way back up that does not depend on one Jev answer. A
trigger is a harness signal that a turn may need more capacity:

| Trigger | Fires when |
|---|---|
| `routed_error` | A request on a tier-routed round fails |
| `c4_break` | C4 answers breaks, mismatch or incomplete (above its own floor) on an edit that is not prose (`.md`, `.mdx`, `.txt`, `.rst`, or under `docs/`) |
| `check_failed_twice` | The same recorded check fails, the working tree changes, and it fails again |
| `identical_calls` | The harness nudges the model out of a run of identical tool calls |
| `stalled_rounds` | 8 tool rounds in a row bring no new working-tree change and no passing check |

An incomplete final check, two failed commands or a spent budget are not
triggers: they fire on many runs that pass, and a mode that saves money must
not buy the expensive model because a turn spent money.

Every trigger is always recorded in shadow, once per turn and only in root
sessions, as a content-free `escalation` decision row labelled
`shadow:<trigger>`. With no `[modes] escalation` key all five triggers also
act. This default has not been benchmarked: no trigger has been fitted
against its shadow rows yet. An explicit list names the triggers that act, and
an empty list turns every trigger off, leaving only the shadow rows:

```toml
[modes]
escalation = ["check_failed_twice", "c4_break"]   # only these two act
# escalation = []                                 # none acts; shadow rows only
```

Unknown names are ignored with one warning. A live trigger climbs a turn-scoped
ratchet, at most twice per turn and only upward, with or without Jev, and the
ratchet is cleared when the turn ends:

- UltraSave: first the executor's effort goes up one level for the rest of the
  turn; then one harness-started, read-only review on the session's model reads
  the turn's patch, the request and the recorded checks, and its gaps are
  appended once as a reminder. The main model takes over the rest of the turn
  only while the last main prompt is under 16,384 tokens; there is no cheap
  signal for a cold prefix, so the prompt size alone decides.
- UltraFast: first the effort floor goes up one level; then the rest of the
  turn follows Normal's effort policy.
- Auto: each climb moves the effective mode one step up (UltraSave or
  UltraFast to Normal, Normal to UltraCode) without asking Jev, and the mode
  does not come back down before the next request.

The tier floor keeps tier routing above a level for the rest of the turn: the
tiers below it are not offered, and when only the main model is left nothing is
asked (`tier:floor_main`). A live `c4_break` on an edit made by a routed tier,
or a live `check_failed_twice`, raises it to the tier above the one that ran
the round. A turn changes tier at most once, recorded as `tier:escalated`. With
any trigger live, tier routing also refuses to move a round to a lower tier in
the middle of a turn once the last main prompt has 16,384 tokens or more, since
the switch would re-read it uncached; the round keeps its model and the refusal
is recorded as `tier:refused_midturn`.

The other rows are `climb:effort`, `climb:review`, `climb:normal_effort`,
`climb:mode:normal` and `climb:mode:ultracode`, and the review's outcome:
`review:refuted`, `review:not_refuted`, `review:unavailable` or
`review:skip-prose`.

## Decision policy by mode

Each mode's row in the [mode policy table](mode-policy.md) also decides how
the harness asks Jev, not only what the prompt says. These rules are on by
default for UltraFast, UltraSave and UltraCode; `[modes] v2 = []` turns them
off (every mode back on its today row), and listing modes keeps them only for
those. Normal does not change. None of this has been benchmarked yet.

| Mode | What its v2 row enforces | Policy rows |
|---|---|---|
| UltraFast | No Jev question is awaited before a main request: the turn-start skill ranking runs detached and its line is appended as a reminder before the turn's second request if it is in, else dropped; one effort decision per turn, about the request, asked detached (rounds run at the silent level until it is in, and a later round takes it only where switching effort is free); effort capped at medium; C4 waits at most 800 ms; no utility-model selection of tool results (the deterministic reducers stay) | `jev_wait_skipped`, `effort_ceiling`, `c4_deadline`, `utility_selection_skipped` |
| UltraSave | Every decision awaited within the 4 s deadline; effort chosen per round at the 0.4 quantile, from a menu that stops at high | `effort_ceiling` |
| UltraCode | Effort read at the 0.75 quantile; no effort-lowering pass; high at least on the round after a failed tool or a failed recorded check; with Jev silent, the turn's last answered level, else medium | `effort_floor` |
| All three | A round whose only tool calls wait on background work, and the round that answers the final-check reminder, ask no effort question and keep the last level | `effort_kept` |

The ceiling filters the levels the effort question offers, and a level still
above it (an effort configured for the session, a redo floor) is lowered and
recorded as `effort_ceiling`. The escalation ratchet's level is applied after
the ceiling, so a climb is never capped; UltraFast's second climb hands the
rest of the turn to Normal's effort rules. Where the prompt cache is keyed by
effort, the ceiling and the floor act only on a round that sets the turn's
route, so they never force an uncached re-read in the middle of a turn. An
explicit effort turns every effort rule off (the turn outcome record shows
`effort_pinned`). Under Auto the request-start mode pick is still awaited, and
the rules of the mode it picks apply from then on; the turn-start skill line
follows the mode in force when the request arrives. Three waits are outside
these rules: tier routing, where it runs (a worker or utility model is
configured), still asks before the round that sets the turn's route; the skill
descriptors of a session's first prompt are still ranked before it is sent;
and the first-turn memory ranking still runs when memory is on.

`[modes.ultrafast] effort_per_round = true` is a benchmark control arm: it puts
UltraFast back on today's per-round effort decision and keeps the rest of its
v2 row.

## Changing a question or a threshold

A new wording, a new question or a moved floor goes through three steps, in
this order, and stops at the first it fails:

1. **Replay.** Ask Jev again about recorded states with
   `crates/codegen/distill-workspace/tests/jev_replay.rs` (ignored; it makes
   paid calls). Run `JEV_REPLAY_MODE=twice` first: the same body asked twice
   gives each lever's disagreement rate, the noise floor a change must beat.
   `solo` compares a batched request with its packs asked alone, `models`
   compares two Jev model ids, and `swap` compares the live wording with a
   swap (`JEV_REPLAY_SWAP=swap.json`, `{lever, question, instructions?,
   criteria?}`). The committed set holds hand-written states around the
   benchmark fixtures only.
2. **Shadow.** The candidate rides as a shadow pack in the request its lever
   already sends, on the same state, and is recorded as a `jev_decision`
   with a `shadow:` label. Nothing acts on it and it never sends a request
   of its own. Shadow packs are on by default and have not been
   benchmarked; `[jev] shadow_packs = false` turns them off. With them on, a
   session pinned to a mode also gets the mode question in its request-start
   skill ranking, and UltraCode's diff review carries two more notes (an
   unhandled edge case, an invariant at risk) plus what alert and
   second-opinion floors of 0.55 and 0.50 would have flagged. This is a
   separate key from `shadow`, which stops the levers that read it from
   acting at all.
3. **Paired benchmark** against the same binary, with no extra regression.

`tools/task_cost_eval/jev_calibrate.py` reads the decision events and
outcome labels: a reliability table per lever, a threshold only when the
rarer outcome has at least 30 events, and `--report scorecard`.

The floors of the mode review, the final check (C1, C3) and the diff review
(C4) are read per served Jev model id from
`distill_workspace::jev::catalog::thresholds`. Its calibration table is empty,
so every model id runs on the shipped defaults and is logged as
`uncalibrated` in the decision log's reason. A calibrated entry names its fit
window, its number of labelled decisions and the hash of the question it was
fitted on; a test fails when that question's wording changes.

To build a replay set from real use, set `[jev] capture_states = true`. Each
Jev request's state and questions are appended to
`<distill home>/jev/states/<date>-<pid>.jsonl`, readable by the owner only.
That file holds user content (requests, diffs, final messages), so the key
is off by default, the file never leaves the machine, and it is kept out of
the session folders that trace uploads read. Never commit a captured set.

## The `jev_decide` tool

The main model and its `general-purpose`, `explore`, `plan` and
`code-reviewer` subagents can call Jev directly with `jev_decide`. It takes
`questions` (a map of `noul`, `choice` or `score` questions in the API's shape),
an optional small `state`, and `sources`: paths of stored tool outputs (the
same files `ask_stored_output` accepts). The harness reads each source,
numbers its lines verbatim, and returns the typed answers with the served
model, usage, latency and per-source coverage (`sent_lines`, `complete`).
Sources over 32 KiB go out as one long-context request (up to about 700 KiB of
source text); a source that does not fit is reported as incomplete, never as
fully read. Secret-bearing sources are refused. Jev advises on the choices
the caller supplies; it does not generate text or approve anything.

Images are refused for now: on 2026-10-15 OpenRouter's Decisions endpoint
accepted `assets` but answered the same with and without the image, so a
visual question cannot be trusted there yet.

## Other decisions

| Area | What Jev decides |
|---|---|
| Content | Which files, lines, logs, search results, and instructions merit another look. |
| Planning | Intent, relevant tool families, and delegation hints. |
| Quality | Whether an edit or failed check needs another attempt, and which errors to address first. |
| Context | What to preserve during compression and compaction, when to compact, and what knowledge to capture. |

Individual switches live under `[jev.ladder]`. For example:

```toml
[jev]
provider = "openrouter_decisions"
base_url = "https://openrouter.ai/api"
model = "~typesafe/jev-latest"
api_key_env = "OPENROUTER_API_KEY"
timeout_ms = 20000
effort_auto = true

[jev.ladder]
b2_micro_effort = true
e_cheap_agent = true             # opt-in: utility explore children
```

Jev runs only through OpenRouter's Decisions endpoint
(`https://openrouter.ai/api/alpha/decisions`, model `~typesafe/jev-latest`,
key `OPENROUTER_API_KEY`). Any other `provider`, `base_url` or `api_key_env`
is rejected at startup and Jev stays off, rather than sending decisions
elsewhere; redirects are not followed. Without the key, the harness still
runs: routing decisions fall back to the session model, and unavailable
utility routes stay out of the way.

To inspect decisions:

```sh
GROK_LOG_JEV=1 distill
```

This compatibility-named variable enables `logs/jev.jsonl` inside the active
profile. Entries record the route, decision, confidence, latency, model, and
whether the requested choice was applied. See [the decision inventory](../list.md)
for implementation pointers. Utility outcomes per source kind are always in the
session's `usage.json` (`utilityOutcomes`, and `source_kind`/`final_decision` on
utility attempt rows), with no content; [Where the token savings come
from](token-saver.md#the-decision-layer-on-top) says how to read them.

Every recorded decision is also one `jev_decision` line in the session's
`events.jsonl`: lever, a hash of the question, the served model, each answer
as numbers and option ids, the composed label, confidence, whether it came
from the memo, latency and round; an ask that got no answer is one
`jev_no_answer` line with its cause (`error`, `timeout`, `no_credential`,
`lever_off`). No state, question text or reason is written, and an id that is
not a short identifier (a test path, for example) is replaced by its hash.
`usage.json` counts the same labels per lever under `utilityOutcomes`, key
`jev:<lever>`, with a failed ask counted as `no_answer:<cause>`.
So that each decision can be joined with what happened after it, the same
file also records outcome facts: `check_recorded` (outcome, command hash,
whether it ran on the current tree), `final_check_reminder` and, when that
request ends, `final_check_followup` (tool rounds after the reminder, edit,
commit/push/PR, the bare `NOTHING_OUTSTANDING` reply, working-tree change),
`mode_switched` (from, to, `request_start`, `review` or `user`),
`effort_floor_raised`, and `skill_suggested` / `skill_read`. They carry ids,
numbers, booleans and 12-hex hashes only, never a command, path, skill name,
model name or message text.
