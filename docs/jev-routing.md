# How Jev routes work

Jev is Distill's decision layer. It answers structured questions about a small
state assembled by the harness: which model should handle a call, how much
effort it needs, or which parts of a tool result are worth keeping.

Of the tiers set in [Choose your models](../README.md#choose-your-models), the
**main** model is required and owns every session. The optional
**reasoning** model advises on planning, recovery, and review when the task
warrants an independent pass. After a diagnosed stall persists, one bounded
`reasoning-executor` child may work on the blocker while the main model waits.
Utility tasks receive a bounded
payload, such as a tool result, log excerpt, or candidate list, instead of the
full conversation.

```text
   Main model: owns the session and normal execution
      |                  |                     |
  conversation   reasoning advice       Utility task
                 (plan or review)      bounded payload
  persistent stall -> one executor -> main verifies
```

Jev chooses among candidates supplied by code. It does not invent candidates
or decide permissions. Distill owns plan mode, auto approval, YOLO, and
permission policies; Jev cannot approve, veto, or hold a tool call for
confirmation. If a Jev decision fails, times out, or lacks enough confidence,
the harness keeps its normal execution path.

## Main and reasoning

The main model executes the task and owns the conversation. Jev decides every consult of the reasoning
model, and the advice joins the conversation as `<reasoning_advice>`, so the
main model keeps following it on later rounds:

| Decision | When Jev is asked | What it weighs |
|---|---|---|
| Plan | The first round of a request | Whether the reasoning model plans it: not at all, now (a request that can be planned from its text), or after inspecting the workspace. For the last case, a fresh read-only `plan` subagent inspects files before the main model's first action when available; otherwise the main model inspects first and the inline reasoning consult follows. Jev also judges complexity for later decisions. |
| Step | Every later round in which the main model did something the reasoning model has not weighed | Whether the main model is stuck and needs a diagnosis before its next round. The facts since the last advice are in the question: tool calls and failures, the call that failed most, the call repeated most (waiting on background work aside), failures in a row, edits undone, rounds without advice and the latest calls. |
| Review | Before delivery, while there is work no review has attempted | Whether the reasoning model reviews the work: recorded edits, the current Git diff (including shell edits), recent checks, failures, the request's complexity, earlier consults and verdicts, and the start of the final message. A fresh read-only `code-reviewer` subagent can inspect changed files; the inline reasoning consult remains the fallback. `VERDICT: revise` keeps the main model working with the review; `VERDICT: approve` lets it deliver. |

An edit the change review (C4) flags for another model's opinion is itself a
Jev decision and is reviewed as it comes. Normal consults have no fixed budget
or cooldown. A persistent stall has a deterministic guard: after six Worker
rounds with the same failed call repeated or an edit undone, or eight rounds
with only the same non-polling call repeated and no edits, the reasoning model
diagnoses it even if Jev did not request advice. If the same signal recurs over
three more rounds, one foreground `reasoning-executor` child gets at most five
turns to make a focused correction and report its check. The Worker then inspects
the actual change and verifies the behavior. A failed diagnosis or child stops
further escalation for that request and tells the Worker to report the blocker.
The same stop instruction applies when the configured Reasoning route is unavailable.
Repeated background polling and elapsed rounds alone do not trigger this path.
An unsure or missing plan
decision leaves the main model to proceed. An unsure or missing delivery decision
still requests review when changes are visible or the final check failed. A
failed or unclear review is reported to the main model rather than counted as
approval. With
`/effort auto`, the round's question rides in the same decision request as the
main model's effort, so a round costs one Jev call. Without a reasoning model,
the main model works alone. If Jev is unavailable, planning and recovery fall
back to the main model; visible changes can still receive a delivery review.

Inline consults of one request are one conversation with the reasoning model. The
instructions are the same for every consult, and each consult resends the
earlier messages and advice unchanged, then adds one message: the work the main
model did since the last reply, the consult's own material (the struggle, the
flagged change, or the diffs, recent checks and final message of a review) and the
task last. Work already sent is never sent again, and every consult of the
request shares one `prompt_cache_key`, so the provider can serve the repeated
prefix from its prompt cache.

Jev decides each consult in one question battery: for every piece of work the
reasoning model has not seen, whether it needs it in full or only as a one-line
summary; and, with `/effort auto` (`b2_micro_effort`), the effort this consult
thinks with, from the reasoning model's own menu. Nothing carries over between
consults: without an answer every item goes in full and the model keeps its
configured effort. When a consult still does not fit the reasoning model's
window, the largest items are cut to verified quotes from the utility model
(`cite_spans`, each quote checked against the item), then to their summaries;
a consult that does not fit even then is skipped and logged.

While an inline reasoning consult runs, the status row names it (for example
`reasoning review gpt-6-sol high`). The turn report lists inline consult
tokens and a `Reasoning - Nx (plan, review)` count. Completed child usage is
folded into the parent session's usage accounting; an incomplete fold blocks a
cost claim, and child usage must not be added twice. With `GROK_LOG_JEV=1`,
each decision and a per-request summary (rounds, failures, changes,
complexity, consults, review verdicts) are logged under `b2_reasoning_model`,
which is what the questions should be tuned from.

The built-in `plan`, `code-reviewer`, and harness-only `reasoning-executor`
subagents default to the reasoning model; other subagents run on the main model.
An explicit subagent model pin
still wins. Without a reasoning model, `plan` and `code-reviewer` use the main
model. Jev starts these roles with fresh context, so a full-context fork cannot
pin them to the Worker's model. They receive the user request and relevant
evidence and follow project instructions. The planner and reviewer are read-only;
the executor can edit under the normal child permissions. The final review
may include pre-existing workspace changes; its prompt identifies that limit.

`/effort auto` chooses the main model's effort per call; a fixed effort pins
that model's intensity. `/model <model> [effort]` selects the main model;
`/reasoning-model <model>` sets the reasoning model and
`/reasoning-model clear` removes it. Explicit subagent model and effort
policies remain pinned.

Jev chooses effort from each model's supported menu. An uncertain answer keeps
that model's configured default. A redo can raise effort when the previous
attempt lacked reasoning; the next independent step can return to auto.
Delegation is useful for a coherent task with clear acceptance criteria and
small relevant context. A trivial step stays with the main agent; a fresh
subagent returns its result and verification rather than its full transcript.

Effort selection uses a confidence floor of 0.40, and the plan decision a floor
of 0.55. A fixed effort, selected in a picker or with `/effort <level>`,
takes precedence over automatic effort selection.

```toml
[models]
default = "chatgpt/gpt-6-luna"    # main model (required)
reasoning = "chatgpt/gpt-6-sol"   # reasoning model (optional)

[jev]
effort_auto = true
```

`b2_reasoning_model` (formerly `b2_light_model`, still accepted) turns the
reasoning consult on or off. Older configs with `[jev.tiers] light` are migrated
at startup: the worker becomes `[models].default` and the old default becomes
`[models].reasoning`.

The built-in `code-reviewer` subagent inspects a substantive code checkpoint
using a fresh, read-only context on the reasoning model, unless pinned through
`[subagents.models]`. Give it the diff, acceptance criteria and test evidence.
Routine or unchanged checkpoints do not need a separate review. Jev's
change-risk decision can request a second opinion, while the agent chooses a
reviewer at meaningful checkpoints and before final code handoff. Goal
completion still uses its existing verifier.

```toml
[subagents.models]
code-reviewer = "reviewer-catalog-entry"
```

The value must name an existing model catalog entry.

The routing and subagent choices are hypotheses about quality and cost. To
claim a saving, compare accepted tasks with and without the role handoffs,
including all main, reasoning, subagent, utility, and retry usage. Test outcomes
and the delivered behavior must be compared alongside cost; token counts alone
do not establish a financial saving.

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
For direct OpenRouter requests, the sampler sends the existing session ID as
`x-session-id` for provider affinity unless the user configured that header.
Responses requests carry the session ID as `prompt_cache_key`. On ChatGPT
(Codex), the first response of a turn also issues an `x-codex-turn-state`, which
the session's later rounds of that turn send back so the backend keeps the turn
on the replica that holds its cached prefix; a new turn starts without one.
This does not guarantee a cache hit; compare cache-read tokens and total cost
before changing cache TTL or context-pruning behavior.

## Utility work

Utility tasks extract literal values, digest logs, compress text, answer
questions about a supplied payload, pick candidate IDs, or classify content.
Each task checks the result before the harness uses it. Depending on the task,
checks preserve literal paths and error messages, require quoted spans, or
reject labels and IDs outside the supplied set. A rejected result falls back
to the normal path.

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

The `b2_local_model` route can also hand an entire call to the Utility model
when the capacity checks and context limit allow it. This is separate from the
bounded utility tasks. The `e_retention` route breaks large outputs into blocks
and decides what to retain before discarding the original text, with special
handling for secrets. Each route has a per-turn failure limit, so a failing
endpoint does not get retried at every step.

## Other decisions

| Area | What Jev decides |
|---|---|
| Content | Which files, lines, logs, search results, and instructions merit another look. |
| Planning | Intent, relevant tool families, and delegation hints. |
| Quality | Whether an edit or failed check needs another attempt, and which errors to address first. |
| Context | What to preserve during compression and compaction. |

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
b2_reasoning_model = true
b2_local_model = true
e_retention = true
```

Jev needs credentials for its configured decision endpoint. Without them, the
harness still runs: routing decisions fall back to the session model, and
unavailable utility routes stay out of the way.

To inspect decisions:

```sh
GROK_LOG_JEV=1 distill
```

This compatibility-named variable enables `logs/jev.jsonl` inside the active
profile. Entries record the route, decision, confidence, latency, model, and
whether the requested choice was applied. See [the decision inventory](../list.md)
for implementation pointers.
