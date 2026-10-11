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
prompt under 16K tokens frees it. On `api.anthropic.com`, a round that blocked
on a task or subagent, or started background work, asks for the one-hour cache
lifetime (a refusal turns it off for the process once the resend without it
goes through), and the free fourth
breakpoint anchors on a former tip that moves every 10 rounds. Rewrites of old
history (soft trims, the retained hard clear, old goal directives) wait for a
cold moment: idle past the last request's cache lifetime (5 minutes on
Messages, 1 hour when asked, 30 minutes on Codex, 10 on Grok and OpenRouter,
and never within the hour after a one-hour write, which later requests still
read through), a compaction, a model switch or a resume. A verbatim fork inherits its parent's
soft trims.

When you type into a session of 100K tokens or more that sat idle past that
lifetime (a known one only; the hour fallback never counts), the request about
to go out writes the whole history at full price anyway, so the turn first asks:
Compact and continue, Keep full history, or Don't ask again, which writes
`[compaction] cold_return = "off"`. `cold_return = "auto"` compacts without
asking. A history already at the auto-compact threshold compacts as usual, and
headless sessions, subagents, `--no-ask-user`, a client without the question UI
and no answer within two minutes keep the full history. A resumed session has
no idle time in the process and is never asked.

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
and config load warns once per key that it is retired.

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
for implementation pointers. Utility outcomes per source kind are always in the
session's `usage.json` (`utilityOutcomes`, and `source_kind`/`final_decision` on
utility attempt rows), with no content; [Where the token savings come
from](token-saver.md#the-decision-layer-on-top) says how to read them.
