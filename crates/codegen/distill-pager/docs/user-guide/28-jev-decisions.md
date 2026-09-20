<!-- Modified for Distill by Samuel Fajreldines, 2026. -->
# Jev decisions (TypeSafe System One)

Distill can route a small class of **structured decisions** to [Jev](https://docs.typesafe.ai), TypeSafe's System One model, instead of asking an LLM. Jev answers typed questions (`choice`, `score`, `noul`) over a bounded state and returns typed answers with probabilities and confidence. It does not generate text, it is much cheaper per token, and this build treats it as strictly **additional** to the existing paths — never a replacement.

Jev is enabled by default for routing, content selection, and context optimization when a credential is available. Permissions belong exclusively to Distill: Jev cannot approve, deny, or hold a tool call for user confirmation. This applies to the main agent and subagents in every permission mode.

---

## What it is used for today

The decision catalogue is documented item by item — question battery, threshold, code locator and covering test — in [`todo.md`](../../../../../todo.md) at the repository root. Grouped by area:

| Area | Items | What changes |
|---|---|---|
| A — content selection | file to edit, read window, log/test lines, web results, memory entries, test to run | the model re-reads less; nothing outside the candidates the code already produced is ever introduced |
| B — effort routing | turn intent, tool families, model/effort tier, subagent type, skill suggestion, delegation hint | fewer tools per turn, a cheaper setting on a routine turn (off until its gate passes), an unknown subagent type resolved to an allowed definition |
| C — verification | premature stop, failure triage, completion check, diff risk, error priority, injection screen, change type | hints on a failure, an unfinished request is not called complete, risky diffs carry a warning |
| D — context and cost | compaction recorte, big-output retention, post-compaction retrieval | a smaller summary and context |

Distill enforces plan-mode restrictions, explicit permission policies, hooks, and the selected approval mode. Auto mode uses the harness classifier. YOLO bypasses that classifier; Jev adds no separate veto.

---

## Configuration

Nothing is required: with `JEV_API_KEY` in the environment the harness defaults apply — master switch **on**, optimization levers enabled, shadow **off** (active). This block spells them out and shows the kill switch:

```toml
[jev]
enabled = true               # default; false (or GROK_JEV=0) disables everything
shadow  = false              # default: Jev decides; true = record only

[jev.ladder]
p1_tool_family        = true # B4: prune tool families for the turn
p2_read_shortlist     = true # A2: pick the read window instead of the whole file
p3_compaction_recorte = true # D1: which segments the summarizer must see
p6_skill_suggestion   = true # B5: name the announced skill the request needs
a1_file_to_edit       = true # A1: rank the candidate files
a3_log_lines          = true # A3: keep the lines that explain a failure
a4_web_results        = true # A4: rank search results before reading them
a5_memory_rank        = true # A5: rank memory entries before injecting them
a6_test_to_run        = true # A6: pick which test to run
b1_intent_routing     = true # B1: classify the turn's intent and complexity
b3_subagent_type      = true # B3: resolve an unknown subagent type
b6_delegation_hint    = true # B6: one advisory line on the delegation tool
c1_premature_stop     = true # C1: requested work still open
c2_failure_triage     = true # C2: classify a failure
c3_completion_check   = true # C3: something the user asked for is missing
c4_diff_risk          = true # C4: flag a risky diff
c5_error_priority     = true # C5: order errors by importance
c7_change_type        = true # C7: label the change type
d2_big_output_retention = true # D2: drop a large inert output
d3_post_compaction    = true # D3: re-inject only still-relevant memory

# Off until their own gate passes (the plan's standing rule):
b2_model_tier         = false # money lever: only ever downgrades a routine turn
c6_injection_screen   = false # cost per tool output not measured yet

# Optional, with the defaults shown
# base_url      = "https://api.typesafe.ai"
# model         = "jev-latest"      # pin a version once thresholds are calibrated
# timeout_ms    = 10000             # per call, covering reading the body
# api_key_env   = "JEV_API_KEY"
# max_state_bytes = 32768
```

### Shadow mode first

With `shadow = true`, Jev decisions are recorded without applying them. This setting does not affect permission decisions, which remain in the harness.

The credential is read from the environment **at call time** by the name in `api_key_env`:

```sh
export JEV_API_KEY="…"
```

Keep it away from agent-run commands by excluding it from the shell environment they inherit:

```toml
[shell_environment_policy]
exclude = ["JEV_API_KEY"]
```

`[jev]` is read from user/system/managed configuration only — general settings never come from a repository, so a checked-out project cannot repoint the endpoint. The key is never written to configuration, logs, or error messages (a server that echoes it back is redacted before anything is logged).

---

## Kill switch and how to revert

* **Turn it off:** set `GROK_JEV=0` or `[jev] enabled = false`, then start a fresh process because the resolved config/status is cached per process. With the master switch off, no client is constructed and no connection is made — that is a tested property, not a promise.
* **Remove it entirely:** delete the `[jev]` section (and the `JEV_API_KEY` export). Nothing else in Grok depends on it.
* **Revoke access:** delete the API key in the TypeSafe dashboard. A missing or empty credential degrades to the existing LLM path.

---

## Auto effort — one decision per model call

`/effort auto` hands the reasoning effort to the decision layer: instead of one
fixed level for the whole session, **every model call of the turn** gets the
effort that call needs. The palette (`/effort`) shows it as **Auto Effort**, above
the model's own levels, marked `(active)` while it is on.

What the decision sees for each call:

| Field | Content |
|---|---|
| `model` / `model_id` | the model's display name and the wire id that will run the call — how much thinking a call needs depends on how strong the model is |
| `offered_efforts` | exactly the levels that model offers, cheapest first, with the same descriptions the palette shows |
| `phase` | `start_of_turn` (the user's request) or `mid_turn_after_tools` (continuing after tool results) |
| `recent_steps` | the last few tool calls and results, as short labels (no bodies) |
| `turn_items` / `request` | how far the turn has progressed, and the user's request (bounded) |

Rules that hold in auto mode:

* the pick is always **one of the levels that model offers** — an answer naming
  anything else is ignored;
* the pick must clear **0.45** confidence. The floor is calibrated on live
  answers (2026-09-18, `deepseek-v4.1-flash`): a trivial request answers `none`
  at 0.67/0.64, while a hard one splits medium 0.40 / high 0.31 — so a clear
  cheap win is applied and a hard call keeps the session's own level instead of
  quietly dropping quality. Below the floor, on a timeout, on an error, or with
  the lever off, the call keeps the session's level (the fallback);
* Jev may answer `keep_session_effort` explicitly, which means the same;
* every decision is recorded with what it *wanted*, so the log shows a deferral
  (`wanted low at 0.41 below the floor`) as clearly as an application
  (`effort:none · applied to this call`);
* turning it off is one command: `/effort <level>` (an explicit level always wins),
  or `[jev.ladder] b2_micro_effort = false` to keep the mode but disable the decision; the master switch (`GROK_JEV=0`) turns it off with everything else.

The footer shows `(<model>) (auto)` while the mode is on, and the turn-status row
still shows the `jev …` chip per turn.

## Local model first (oMLX, Ollama, any OpenAI-compatible server)

The local model is free, so it takes a call whenever it can fully do it. Configure the endpoint like any custom
model and point `[jev.local]` at it:

```toml
[model.qwen38-local]
model = "Qwen3.8-27B-4bit"
base_url = "http://127.0.0.1:8000/v1"
name = "Qwen3.8 27B (local oMLX)"
api_key = "none"
api_backend = "chat_completions"
context_window = 32768
max_completion_tokens = 4096
stream_tool_calls = false

[jev.local]
model = "qwen38-local"
notes = "tool calling OK, no reasoning effort; weak at proofs and counting."
context_reserve_tokens = 8192
# min_capability = 0.70
```

Per model call:

1. **Code guard** — the conversation estimate plus the reserve must fit the local window; if not, the call stays
   on the session model and the record says why.
2. **Decision** — three `noul` questions: *can the local model fully do this call?*, *does it need more context
   than its window?*, *does it need frontier-level reasoning?*. Local wins only with `capable ≥ min_capability`
   (0.70 by default) and no red flag at or above 0.40. Any missing answer, error or timeout means cloud.
3. **Apply** — the round runs with the local endpoint, credential, backend and window; session attribution is
   kept, and the turn row marks `jev ×N ·local`.

Watch the window: this harness's base prompt (system prompt plus the turn's history) is already **~29k tokens**,
and the tool definitions ride on top. A 32k local window therefore rarely fits — raise it on the server (oMLX:
*Settings → model → max context window*, then restart the server so it re-reads its settings) and keep
`[model.<local>] context_window` at the same number, or point `[jev.local]` at a larger local model. Measured
once the window was 128k: a trivial turn ran **entirely** on the local model (126.4k tokens, 3m25s) instead of the
cloud (about 9s) — free tokens, slower work. Disable it with `[jev.ladder] b2_local_model = false`.

## Reviewing a change

Every tool result that changed a file goes through one review battery, and only a real finding travels back to the
model:

| Question | Meaning | Floor |
|---|---|---|
| `matches_step` | Does the change include what the step asked for? A wider change (whole-file rewrite) still counts when it contains the asked-for work | 0.60 to stay silent; below it the model is told to re-read and fix or revert |
| `may_break` | Could it break something that relies on the old behaviour (signatures, callers, data shapes)? | 0.50 → "check the callers" |
| `looks_incomplete` | Is the change unfinished in itself — stub body, truncated code, a renamed caller left behind? | 0.50 → "finish it" |
| `step_complete` | Is the step done as it stands, nothing left to redo? | 0.60; below it the step is redone |
| `needs_more_thinking` | If it has to be redone, does the redo need a higher reasoning effort (not just another try)? | 0.50 |

The reviewer reads the step the model said it was on (not the whole request: a correct edit of a two-part request
must not read as half-done) plus the call and its result, both bounded. Anything unusable defers: no note, and no
claim that the change was reviewed. The verdict lands in `~/.grok/logs/jev.jsonl` as
`review:ok | review:mismatch | review:breaks | review:incomplete`, with the step it judged.

**When the step has to be redone**, the review acts instead of only advising:

* needs more thinking → the harness raises the **turn's effort floor** to the next level the model offers and tells the
  model which level the redo will run at (`redo:floor` in the log; the floor wins over the auto-effort choice for the
  rest of the turn);
* already at the highest setting (compared by the value the level maps onto, so a level sharing the top value does not
  count) → the note says so and the model has to **find the actual error and redo** the step;
* another try at the same setting is enough → the note asks for the redo plainly.

## Local work through a subagent (short context)

Delegation is the fast path to a local model: a subagent opens its own conversation, so the session's context never
travels with the call.

```markdown
<!-- ~/.grok/agents/local-worker.md -->
---
name: local-worker
description: Small self-contained step, done on the free local model.
model: qwen38-local
---
You are the local worker: a small model on this machine, used for one bounded step at a time. …
```

```toml
[subagents.models]
local-worker = "qwen38-local"

[jev.local]
max_context_tokens = 32768   # speed policy: above this, the call stays on the session model
context_reserve_tokens = 2048
```

Measured on one turn (oMLX `Qwen3.8-27B-4bit`): the subagent's calls reached the local server with
**450 → 9,935 → 10,010** prompt tokens, while main-loop rounds asked for **42–45k**. The turn report shows both
routes, and a call above the cap records why it went back to the session model.

## Observability

**In the TUI:** two places show where the Jev path stands.

The **turn status row** (the line above the prompt, next to the running tool) shows what the decision layer did *for this turn*, and only when it did something:

| Chip | Meaning |
|---|---|
| **`jev…`** (green) | A decision is being asked for right now — the call is in flight |
| **`jev 0.4s`** (green) | One decision answered, with its latency |
| **`jev ×3`** (green) | Several decisions so far this turn |
| **`jev·fallback`** (red) | A Jev optimization or quality decision was declined; this is not a permission veto |
| **`jev ×8 ·none`** | …plus the routing of the call that is running: the effort level Jev chose for this micro-action (`none`, `low`, `medium`, `high`, `xhigh`, `max`) |
| **`jev ×3 ·local low`** | The call was routed to the configured local model, at that level |
| *(nothing)* | Jev was not consulted in this turn — the row never claims otherwise |

**At the end of every turn**, a block reports how the task was distributed:

```
Qwen3.8 27B (local oMLX) - 12.4k tokens
DeepSeek V4.1 Flash high - 210.4k tokens
Jev - 41x
```

One line per engine that ran in that turn (model plus the effort it ran at, with that turn's tokens), then the
number of decisions the layer took during it. It comes from the harness's own per-turn ledger, rides the same
turn-terminal payload as the bill, and is only printed when there is something to say — a turn the layer never
touched adds no block.

The **prompt footer** always shows where the path stands, next to the mode flags:

| Badge | Meaning |
|---|---|
| **`jev`** (green, bold) | Jev is available, independently of the permission mode |
| **`jev·shadow`** (green) | Same, but decisions are recorded and not applied |
| **`jev:off`** (dim) | The path is disabled (`GROK_JEV=0`, `[jev] enabled = false`) or no credential is resolvable |

The badge is always visible. `jev:off` means Jev is disabled or lacks a credential; it says nothing about permission to execute tools. The status is resolved once per process.

Both surfaces read the same decision record, so they cannot disagree: the footer says whether the path *can* be used, the turn row says whether it *was*, and with which outcome.

**In logs:** every decision is recorded with the lever, the question ids, the verdict, the confidence, the model version that answered, latency and `usage` tokens. Deferrals are recorded too, so a seam that silently adds nothing is visible. Set `GROK_LOG_JEV=1` to write them to `~/.grok/logs/jev.jsonl` (one JSON object per line, size-capped); without it the records only reach the tracing subscriber.

---

## Limits worth knowing

* Requests are capped at 64k tokens total (32k for state + the longest question); this build stays far below that with `max_state_bytes`.
* One attempt per call on the hot path: a timeout or error defers; nothing is retried behind your back.
* Jev is unreliable for arithmetic, counting, date ordering and multi-hop reasoning, and it cannot generate text — decisions of that shape stay in code or with the LLM.

The design, thresholds and evidence behind this page live in `plan/plan.md` (§1.3.1, §1.6, §12.5) and `plan/docs-review.md`.

---

## Testing it

Four layers, cheapest first. Layers 1 and 2 need nothing from you beyond the API key; layer 3 needs a real session; layer 4 is the deterministic end-to-end harness.

### 1. Unit and live tests (no session needed)

```sh
# Foundation, policy, ladder, seam — all hermetic (local HTTP stub, no network):
cargo test -p distill-workspace --lib jev::

# Config row, credential list:
cargo test -p distill-config-types -p distill-env

# The log sink that layer 3 reads:
cargo test -p distill-telemetry --lib jev_log

# The gate against the real API (opt-in, hits the network):
JEV_API_KEY=… cargo test -p distill-workspace --test jev_live -- --ignored --nocapture
```

The opt-in suite includes historical permission-classifier experiments; those classifiers are not wired into Distill sessions.

### 2. A real session in shadow mode (recommended first)

```sh
export JEV_API_KEY="…"
export GROK_JEV=1
export GROK_LOG_JEV=1          # decisions land in ~/.grok/logs/jev.jsonl
cargo run -p distill-pager-bin
```

With `shadow = true` in `[jev]`, Jev does not apply optimization choices, and it does not participate in permission outcomes. Every optimization decision it *would* make is written as one JSON line:

```sh
tail -f ~/.grok/logs/jev.jsonl | jq '{lever, decision, escalated, confidence, model, latency_ms, input_tokens}'
```

Run a session with an optimization-eligible turn (`cargo check`, `rg`, or an edit), then inspect the optimization record: its lever, selected or declined optimization, confidence, fallback reason, model, latency, and token usage. Do not interpret a Jev record as an allow/block/escalate result. Permission outcomes belong to the independent Distill harness check below.

### 3. Optimization fallback and kill-switch drills (a real session, five minutes)

| Drill | How | What must happen |
|---|---|---|
| Kill switch | set `GROK_JEV=0` or `[jev] enabled = false`, then start a fresh process | the footer shows `jev:off`, no Jev client/request is made, and the normal optimization path continues; permission behavior is unchanged |
| Dead endpoint or timeout | set `base_url = "http://127.0.0.1:9"` or use the existing timeout seam | the optimization attempt records a transport fallback and the existing model/effort/context path runs; no permission result is attributed to Jev |
| Missing credential | `unset JEV_API_KEY` | the Jev seam is not wired, the normal optimization path runs, and harness permission behavior is unchanged |
| Bad model | `model = "does-not-exist"` | `invalid`/`unavailable` is recorded, then the normal optimization path runs |
| Lever disabled or shadow mode | disable the selected ladder lever, or set `shadow = true` | no optimization choice is applied; the session keeps its ordinary model/effort/context and permission paths |

### 4. Independent harness permission check

Permission proof is separate from Jev proof. Use the existing Distill permission unit/PTY harness with Jev enabled and disabled, and assert the harness-owned result for an ordinary command and a risky command: the configured auto classifier/policy allows, blocks, or requests confirmation as appropriate, while YOLO follows its own harness policy. Repeat with `GROK_JEV=0` and confirm the permission result is identical and no Jev record is treated as an authorization decision. Do not use Jev logs, a Jev `allow`/`block`/`escalate` label, or absence of a model request as permission evidence.

### 5. Deterministic optimization end-to-end (the PTY harness)

The repo ships a scripted PTY harness that drives the real TUI against a mock model server: `crates/codegen/distill-pager-pty-harness` with YAML scenarios under `tests/scenarios/` and a `ScriptedScenario` runner. An optimization scenario may assert the recorded Jev decision, the applied model/effort or context choice, and the fallback path when Jev is unavailable. It must not assert that Jev approved a permission request or that a routine permission request disappeared. (The deterministic optimization layer is not implemented yet — the existing unit/integration tests prove the current seams.)

### What each layer proves

| Layer | Proves |
|---|---|
| 1 | Contract, error taxonomy, thresholds, optimization fallback, flag-off inertness, and the gate against the live API |
| 2 | The optimization wiring inside a real session, with your config |
| 3 | That optimization failures degrade to the existing path instead of changing authorization |
| 4 | That harness permission behavior is independent of Jev |
| 5 | The same optimization behavior, repeatably, in CI |
