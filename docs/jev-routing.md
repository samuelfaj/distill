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
| Harness roles (a goal's planner, verifiers, strategist, summarizer) | the main model |

An explicit `[subagents.models]` pin or an agent definition's `model:` wins over
the worker default. A worker model missing from the catalog falls back to the
main model.

A delegated worker cannot delegate further (the subagent depth limit is one). Its
final message is the report the main model receives: the outcome, then the
evidence (`path:line` references and each command with its result), then what it
could not verify.

Both tiers take an effort level or `auto`, and `auto` lets Jev pick the effort
for every call from that model's own menu:

- Main model: `/model <model> [effort|auto]` and `/effort <level|auto>` set this
  session. The Model tiers screen (`/tiers`, entered as `model effort`) also saves
  it for new sessions: `auto` as `[jev] effort_auto = true`, a level as
  `[jev] effort_auto = false` plus `[models].default_reasoning_effort`.
- Worker model: `/worker-model <model> [effort|auto]` or the Model tiers screen
  saves `[models].worker_effort`; unset means `auto`. It applies to every child
  that runs on the worker, independent of the main model's mode, unless the
  caller, a role or the agent definition set an effort. `/worker-model clear`
  removes the worker.

Explicit subagent model and effort policies remain pinned.

Jev chooses effort from each model's supported menu, with a confidence floor of
0.40. An uncertain answer keeps that model's configured default. A redo can
raise effort when the previous attempt lacked reasoning; the next independent
step can return to auto. A fixed effort, selected in a picker or with
`/effort <level>`, takes precedence over automatic effort selection.

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
progress evaluator run on the main model.

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
b2_micro_effort = true
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
