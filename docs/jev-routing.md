# How Jev routes work

Jev is Distill's decision layer. It answers structured questions about a small
state assembled by the harness: which model should handle a call, how much
effort it needs, or which parts of a tool result are worth keeping.

Of the tiers set in [Choose your models](../README.md#choose-your-models), the
**main** model is required and runs every session and every step. The optional
**reasoning** model never takes over the session: the main model consults it to
plan or review a step it cannot do alone. Utility tasks receive a bounded
payload, such as a tool result, log excerpt, or candidate list, instead of the
full conversation.

```text
   Main model: owns the session, runs every step
      |                  |                     |
  conversation   reasoning advice       Utility task
                 (plan or review)      bounded payload
```

Jev chooses among candidates supplied by code. It does not invent candidates
or decide permissions. Distill owns plan mode, auto approval, YOLO, and
permission policies; Jev cannot approve, veto, or hold a tool call for
confirmation. If a Jev decision fails, times out, or lacks enough confidence,
the harness keeps its normal execution path.

## Main and reasoning

Before each step, Jev decides whether the main model can do it correctly and
completely on its own. When it can, the main model continues alone. When it
cannot (a new non-trivial request, architecture or design, ambiguous
requirements, interpreting evidence, recovering from a failure, or reviewing a
change), the reasoning model receives a bounded, tool-free view of the work and
returns a plan or review. The advice joins the conversation as
`<reasoning_advice>`, so the main model keeps following it on later rounds of
the same request.

A confident answer decides. An unsure answer depends on the request: a request
with no advice yet still gets a plan, because a wrong first step costs more
than one consult; once advice was given, the main model keeps following it and
only a confident answer (usually after a failure) consults again. When the
change review asks for an independent review, the reasoning model reviews the
edit without a second question. The same decision picks the reasoning model's
effort from its own menu. Without a reasoning model, or when Jev is unavailable,
the main model works alone.

The built-in `plan` and `code-reviewer` subagents run on the reasoning model;
every other subagent runs on the main model. An explicit subagent model pin
still wins. Without a reasoning model, `plan` and `code-reviewer` use the main
model.

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

Effort selection uses a confidence floor of 0.40, and the reasoning consult a
floor of 0.55. A fixed effort, selected in a picker or with `/effort <level>`,
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
