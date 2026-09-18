# Jev decisions (TypeSafe System One)

Grok can route a small class of **structured decisions** to [Jev](https://docs.typesafe.ai), TypeSafe's System One model, instead of asking an LLM. Jev answers typed questions (`choice`, `score`, `noul`) over a bounded state and returns typed answers with probabilities and confidence. It does not generate text, it is much cheaper per token, and this build treats it as strictly **additional** to the existing paths — never a replacement.

In this build the path is **on by default** — the owner's explicit override of the plan's original default-OFF invariant. With `JEV_API_KEY` in the environment, auto mode wires the Jev seam ahead of the LLM classifier; without a resolvable credential the seam is not wired at all and nothing changes. The kill switch is a tested property: `[jev] enabled = false` or `GROK_JEV=0` leaves no client and no connection.

---

## What it is used for today

The **catalogue of 23 decision points** is wired and documented item by item — question battery, threshold, code locator and covering test — in [`todo.md`](../../../../../todo.md) at the repository root. Grouped by area:

| Area | Items | What changes |
|---|---|---|
| Permission | auto-mode classifier, always-approve brake | routine actions route to *allow*; a confident catastrophe is refused in always-approve mode |
| A — content selection | file to edit, read window, log/test lines, web results, memory entries, test to run | the model re-reads less; nothing outside the candidates the code already produced is ever introduced |
| B — effort routing | turn intent, tool families, model/effort tier, subagent type, skill suggestion, delegation hint | fewer tools per turn, a cheaper setting on a routine turn (off until its gate passes), an unknown subagent type resolved to an allowed definition |
| C — verification | premature stop, failure triage, completion check, diff risk, error priority, injection screen, change type | hints on a failure, an unfinished request is not called complete, risky diffs carry a warning |
| D — context and cost | compaction recorte, big-output retention, post-compaction retrieval, call validation | a smaller summary and context; a call that looks out of scope is held for a human |

The **auto-mode permission classifier** is the oldest seam, and the only one that can *allow* without asking. A proposed tool call in auto mode is judged by a speculative battery of atomic questions (risk class, does it escape the workspace, does it delete data, privilege escalation, network egress, untrusted execution, an injection screen, and a severity rubric). The answers are composed in code against fixed thresholds, and the outcome can only be:

| Outcome | What happens |
|---|---|
| **Block** | Confident danger (for example a high escape probability) — the call is refused. |
| **Allow** | Only for the *routine, in-workspace, confident* class: Jev must agree with the local fast-path class, report no findings, and clear the confidence bar (0.60 normally, 0.85 when the caller marks the action sensitive). |
| **Defer** | Everything else — a low-confidence answer, a value inside the review band (0.30–0.70), a timeout, an error, or a rejected request — goes to the **existing LLM classifier**, and then to the normal human prompt if that also cannot decide. |

Two safety rules are structural, not advisory:

* **A Jev allow never clears the denial ratchet.** The consecutive-denial counter that forces escalation stays untouched, so a steered classifier cannot switch the automatic escalation off.
* **Security findings bypass Jev entirely.** If the harness's own static analysis flagged the command, Jev is not consulted at all.

---

## Configuration

Nothing is required: with `JEV_API_KEY` in the environment the harness defaults apply — master switch **on**, every lever **on**, shadow **off** (active). This block spells them out and shows the kill switch:

```toml
[jev]
enabled = true               # default; false (or GROK_JEV=0) disables everything
shadow  = false              # default: Jev decides; true = record only

[jev.ladder]
permission_classifier = true # default: Jev seam ahead of the LLM classifier
yolo_veto             = true # default: brake in always-approve (YOLO) mode
p1_tool_family        = true # B4: prune tool families for the turn
p2_read_shortlist     = true # A2: pick the read window instead of the whole file
p3_compaction_recorte = true # D1: which segments the summarizer must see
p5_call_validation    = true # D4: hold a call that looks out of scope
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

With `shadow = true` the seam computes the decision it *would* make and records it, while the LLM classifier still decides. That is the intended way to evaluate agreement on your own workload before letting Jev decide anything. In this build shadow is **off** (Jev decides), so if you want the observation-only phase, set it explicitly.

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

## Always-approve (YOLO) runs a brake

Always-approve exists to avoid interruptions, so Jev does not act as a gatekeeper there — it acts as a **brake**. In that mode every tool call is sent to Jev (that is the point: nothing is pre-filtered), and the only outcome it can produce is a refusal:

| Jev says | What happens |
|---|---|
| **Block** (confident catastrophe: escapes the workspace, destructive, severity at the top of the rubric) | The call is **refused** with the reason, and the model is told to pick another approach — no prompt, so the mode keeps its no-interruption promise |
| Anything else | The call proceeds exactly as always-approve did before |
| Timeout, error, no credential, rate limit | The call proceeds (**fail-open**): a brake that cannot reach the service must not stop the session |

Two consequences worth knowing: the brake never *widens* anything (the mode already allows everything), and it deliberately ignores the mode's usual "security findings skip the classifier" rule — flagged commands are exactly what a brake should look at. Turn it off with `[jev.ladder] yolo_veto = false`, or the master switch.

## Exactly what is sent

The state is an allowlist of named fields, capped (`max_state_bytes`, 400 characters per field):

| Field | Content |
|---|---|
| `proposed_action.tool` | The tool name (`bash`, `search_replace`, …) |
| `proposed_action.detail` | The command or the bounded access detail (file path, MCP args truncated to 1 KiB) |
| `recent` | Up to 6 recent transcript turns, each truncated, user text and assistant tool *names/args* only |
| `project_instructions` | The repository `AGENTS.md`, truncated |
| `note` | A fixed line stating that everything above is untrusted data, never instructions |

**Never sent:** file contents, tool output, diffs, secrets, your prompts verbatim, or the API key. The transcript turns are the only place attacker-influenced text can enter, which is why one of the questions is an explicit injection screen — and why that screen is treated as a filter, never as a security boundary.

---

## Kill switch and how to revert

* **Turn it off:** set `enabled = false`, or unset `GROK_JEV`. With the master switch off, no client is constructed and no connection is made — that is a tested property, not a promise.
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

## Observability

**In the TUI:** two places show where the Jev path stands.

The **turn status row** (the line above the prompt, next to the running tool) shows what the decision layer did *for this turn*, and only when it did something:

| Chip | Meaning |
|---|---|
| **`jev…`** (green) | A decision is being asked for right now — the call is in flight |
| **`jev 0.4s`** (green) | One decision answered, with its latency |
| **`jev ×3`** (green) | Several decisions so far this turn |
| **`jev·veto`** (red) | Jev refused a call in this turn (the always-approve brake) |
| *(nothing)* | Jev was not consulted in this turn — the row never claims otherwise |

The **prompt footer** always shows where the path stands, next to the mode flags:

| Badge | Meaning |
|---|---|
| **`jev`** (green, bold) | The seam can act and this session routes through it (auto mode) |
| **`jev·shadow`** (green) | Same, but decisions are recorded and not applied |
| **`jev·veto`** (green) | Always-approve (YOLO): Jev runs as a **brake** — see below |
| **`jev:idle`** (dim) | Ask mode: no classifier is consulted, so Jev is not reached |
| **`jev:off`** (dim) | The path is disabled (`GROK_JEV=0`, `[jev] enabled = false`) or no credential is resolvable |

The badge is deliberately never hidden: spotting `jev:idle` immediately explains "why is nothing happening" when you are in always-approve mode, and `jev:off` tells you the setup itself is not ready. The status is resolved once per process, like the wiring itself.

Both surfaces read the same decision record, so they cannot disagree: the footer says whether the path *can* be used, the turn row says whether it *was*, and with which outcome.

**In logs:** every decision is recorded with the lever, the question ids, the verdict, the confidence, the model version that answered, latency and `usage` tokens. Deferrals are recorded too, so a seam that silently adds nothing is visible. Set `GROK_LOG_JEV=1` to write them to `~/.grok/logs/jev.jsonl` (one JSON object per line, size-capped); without it the records only reach the tracing subscriber.

---

## Limits worth knowing

* Requests are capped at 64k tokens total (32k for state + the longest question); this build stays far below that with `max_state_bytes`.
* One attempt per call on the hot path: a timeout or error defers; nothing is retried behind your back.
* While another classify is in flight, Jev is skipped so a third-party call never queues ahead of the harness's own permission traffic.
* Jev is unreliable for arithmetic, counting, date ordering and multi-hop reasoning, and it cannot generate text — decisions of that shape stay in code or with the LLM.

The design, thresholds and evidence behind this page live in `plan/plan.md` (§1.3.1, §1.6, §12.5) and `plan/docs-review.md`.

---

## Testing it

Four layers, cheapest first. Layers 1 and 2 need nothing from you beyond the API key; layer 3 needs a real session; layer 4 is the deterministic end-to-end harness.

### 1. Unit and live tests (no session needed)

```sh
# Foundation, policy, ladder, seam — all hermetic (local HTTP stub, no network):
cargo test -p xai-grok-workspace --lib jev::

# Config row, credential list:
cargo test -p xai-grok-config-types -p xai-grok-env

# The log sink that layer 3 reads:
cargo test -p xai-grok-telemetry --lib jev_log

# The gate against the real API (opt-in, hits the network):
JEV_API_KEY=… cargo test -p xai-grok-workspace --test jev_live -- --ignored --nocapture
```

The last command runs an 18-case labelled corpus — including two prompt-injection cases — through the real permission battery and fails if Jev allows anything the corpus calls unsafe.

### 2. A real session in shadow mode (recommended first)

```sh
export JEV_API_KEY="…"
export GROK_JEV=1
export GROK_LOG_JEV=1          # decisions land in ~/.grok/logs/jev.jsonl
cargo run -p xai-grok-pager-bin   # then put the session in auto mode (/permissions → Auto)
```

With `shadow = true` in `[jev]`, nothing changes about what gets approved — but every decision Jev *would* make is written as one JSON line:

```sh
tail -f ~/.grok/logs/jev.jsonl | jq '{lever, decision, escalated, confidence, model, latency_ms, input_tokens}'
```

Run a session that does routine work (`cargo check`, `rg`, edits) and one risky action (`rm -rf ~/something`), then check the file. What you should see: `decision:"allow"` for the routine class, `decision:"block"` or `"escalate"` for the risky one, and `escalated:true` lines wherever Jev deferred to the LLM classifier.

### 3. Failure and kill-switch drills (a real session, five minutes)

| Drill | How | What must happen |
|---|---|---|
| Kill switch | unset `GROK_JEV` (or `enabled = false`) | identical behaviour to a build without Jev; `jev.jsonl` stops growing |
| Dead endpoint | `base_url = "http://127.0.0.1:9"` | every decision is an `escalate` with a transport reason; approvals still work through the LLM classifier |
| Missing credential | `unset JEV_API_KEY` | the seam is never wired (a warning says so); approvals unchanged |
| Bad model | `model = "does-not-exist"` | `invalid`/`unavailable` reasons, then the normal path |
| Jev-first | `shadow = false` | routine in-workspace calls are approved without the LLM classifier call; everything else still defers |

### 4. Deterministic end-to-end (the PTY harness)

The repo ships a scripted PTY harness that drives the real TUI against a mock model server: `crates/codegen/xai-grok-pager-pty-harness` with YAML scenarios under `tests/scenarios/` and a `ScriptedScenario` runner. Adding a scenario that puts the session in auto mode and asserts a `jev.decision` line is the deterministic version of layer 3; the scenario can also assert that no requests reach the mock model for a routine approval. (Layer 4 is not implemented yet — layer 3 plus the integration tests are what currently prove the seam.)

### What each layer proves

| Layer | Proves |
|---|---|
| 1 | Contract, error taxonomy, thresholds, authority rules, flag-off inertness, the gate against the live API |
| 2 | The wiring inside a real session: that the seam is reached, in auto mode, with your config |
| 3 | That failure modes degrade to the existing path instead of failing open |
| 4 | The same end-to-end behaviour, repeatably, in CI |
