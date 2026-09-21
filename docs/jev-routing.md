# How Jev routes work

Jev is Distill's decision layer. It answers structured questions about a small
state assembled by the harness: which model should handle a call, how much
effort it needs, or which parts of a tool result are worth keeping.

Of the three tiers set in [Choose your models](../README.md#choose-your-models),
the Reasoning model handles calls unless a routing decision selects another
path. The Worker shares the conversation. Utility tasks receive a bounded
payload, such as a tool result, log excerpt, or candidate list, instead of the
full conversation.

```text
                         Jev decision
                              |
                +-------------+-------------+
                |                           |
          Reasoning model              Worker model
                |                  same conversation
                |
           Utility tasks
     bounded payloads and checked results
```

Jev chooses among candidates supplied by code. It does not invent candidates
or decide permissions. Distill owns plan mode, auto approval, YOLO, and
permission policies; Jev cannot approve, veto, or hold a tool call for
confirmation. If a Jev decision fails, times out, or lacks enough confidence,
the harness keeps its normal execution path.

## Reasoning and Worker

Jev chooses between the configured Reasoning and Worker models for each call.
`/effort auto` lets it choose the Reasoning model's effort; a fixed effort pins
that model's intensity without disabling Worker routing. `/worker-model <model>
[effort]` independently sets the Worker's effort, defaulting to `auto`. Explicit
subagent model and effort policies remain pinned.

Model selection and the candidates' separate effort questions share one request.
Each effort is validated against the selected model's own menu, including model-ID
variants. The Utility model also keeps its explicit effort, or uses its own auto
selection. An uncertain answer preserves the selected model's configured default.
The decision considers the next action, recent results, and the previous dispatch:
Worker execution should be bounded and already decided; investigative tool use
can still need the Reasoning model.

Worker routing requires a confidence of at least 0.55. Effort selection uses a
floor of 0.40. A fixed effort, selected in a picker or with `/effort <level>`,
takes precedence over automatic effort selection.

The configuration keys keep their internal names, so the Worker is `light` and
the Utility model is `local`:

```toml
[jev]
effort_auto = true

[jev.tiers]
light = "codex-luna"

[jev.ladder]
b2_light_model = true
```

`light` names a configured model entry. Leave it unset to run without a Worker,
or set `b2_light_model = false` to turn off Worker routing.

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
b2_light_model = true
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
