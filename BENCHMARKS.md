# Distill vs. Codex: ChatGPT Sol + Luna

Status: baseline and optimization in progress. **Distill has not yet met the
speed and cost targets.** Results below are measurements, including failures.

Both agents use the user's **ChatGPT subscription**, with the same account:

| Agent | Main model | Worker | Effort |
|---|---|---|---|
| Distill | `chatgpt/gpt-6.1-sol` | `chatgpt/gpt-6-luna` | Main auto; Worker auto |
| Codex `--yolo` | `gpt-6.1-sol` | Native subagents remain enabled | high |

Inference uses the native `https://chatgpt.com/backend-api/codex` OAuth route.
No model API keys are inherited by benchmark processes. The optional third-party
Jev decision service has no credential in the isolated profile: auto remains
selected and uses Distill's existing per-model fallback (observed: Sol low,
Luna medium). This measures subscription-only operation, including its current
fallback behavior, rather than claiming the external Jev service ran.

## Acceptance criteria

- At least **50% less total modeled credit cost** and **30% less wall time** over
  the paired task set, retaining all repetitions and failed attempts.
- Both agents must pass the same external behavior graders. Review the delivered
  changes as well. Results describe the measured tasks; they do not establish
  universal model equivalence.
- Count **every model call**: main, worker, subagents, title, compression, utility,
  review, and retries. Missing usage blocks a total-cost claim. Deduplicate folded
  child attempts instead of adding parent and child totals together.

## Accounting and subscription pricing

Rates retrieved **2026-09-30** from [ChatGPT/Codex subscription pricing](https://learn.chatgpt.com/docs/pricing#token-rates):

| Model | Input credits / 1M | Cached input credits / 1M | Output credits / 1M |
|---|---:|---:|---:|
| GPT-6.1 Sol | 50 | 2.5 | 250 |
| GPT-6 Luna | 2.5 | 0.25 | 12.5 |

Modeled credits = `(input - cached) * input_rate + cached * cache_rate + output * output_rate`,
with rates divided by one million. Reasoning is already included in output.
Credit billing has no separate cache-write charge. These Standard credit rates
apply to credit-based usage; they do **not** directly predict consumption of the
included subscription allowance. The benchmark does not reconcile an invoice,
change a fixed monthly subscription price, or convert API token prices into
subscription charges. Lower modeled credit cost matters when usage draws on
purchased credits; monetary conversion depends on the plan's credit price.

## Reproduce

`tools/task_cost_eval/compare_codex.py` runs one fresh Git repository per cell,
using the existing fixtures in `tools/task_cost_eval`. Prompts, starting code,
and external graders are identical for the two agents. Graders remain outside
the writable task repository. Cells run sequentially; alternate agent order
across repetitions. Record executable, config, prompt, fixture, and grader hashes.
Wall time includes startup and native shutdown.

Distill uses an isolated profile and `--no-leader`, with a temporary owner-only
copy of its existing ChatGPT login. That copy is removed on exit. Native parent
`usage.json` includes auxiliary calls and folded children. The evaluator verifies
that all child attempt identities appear in the parent exactly once. Unknown,
failed, or cancelled calls with missing usage keep accounting incomplete.

Codex uses `--ignore-user-config --ephemeral --yolo`, with
`forced_login_method="chatgpt"`. A local OTLP collector records native
`response.completed` token metadata for all conversations, including subagents
and warmup requests. It receives telemetry only; it does not proxy inference.
The parent CLI turn total is also retained for comparison.

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m tools.task_cost_eval.compare_codex \
  --variant distill --codex /path/to/current/codex \
  --case tax-bug-en --output /tmp/distill-bench/tax-distill-1
PYTHONDONTWRITEBYTECODE=1 python3 -m tools.task_cost_eval.compare_codex \
  --variant codex --codex /path/to/current/codex \
  --case tax-bug-en --output /tmp/distill-bench/tax-codex-1
```

A current Codex CLI is required: the installed 0.147.0 client could not discover
Sol and the subscription backend rejected its request. The already installed
**0.159.1** client succeeded. Distill discovers that version via the same PATH.

## Checkpoint 1: subscription pilot

Date: 2026-09-30. Distill source: `9bdd5e8e7fd57a2e4bc49f2820ce2894b3e4356c`.
One exploratory repetition, **not** a statistical result. Task: integer tax bug.

| Agent | Grade | Wall time | Calls | Input | Cached input | Output | Modeled credits |
|---|---|---:|---:|---:|---:|---:|---:|
| Codex 0.159.1, Sol high | PASS | 50.06 s | 6 | 135,251 | 109,056 | 662 | 1.74789 |
| Distill, Sol auto + Luna auto | PASS | 62.85 s | 11 | 79,675 known | 13,952 | 1,222 known | **incomplete** |

The delivered tax fixes pass the same external grader. Distill is **25.6% slower**
in this pilot and cannot support a total-cost claim. Its parent correctly folds
all child identities, but one Worker title call failed without usage and a
compression call was cancelled without usage. Their cost is unknown, not zero.

Codex's native CLI parent turn total reports 122,754 input and 662 output tokens.
Native completed-response telemetry also contains a 12,497-input, zero-output
warmup. The table includes that request in total observed usage. Since the
published rates do not separately explain warmup billing, the credit total is a
rate-based estimate, not proof that warmup consumed purchased credits. Later
comparisons will also report a conservative result excluding warmup credit cost.

The isolated Distill profile unexpectedly imported global MCP servers. This
adds tools and startup work to a task that requires only local files. The next
measurement will explicitly freeze external-tool discovery for both agents.

Raw non-secret pilot summaries and call metadata are in
[`tools/task_cost_eval/results/sol-luna-subscription-pilot.json`](tools/task_cost_eval/results/sol-luna-subscription-pilot.json).

## Checkpoint 2: auxiliary calls and complete child accounting

The control diagnostic with Cursor/Claude MCP imports disabled still took
**55.85 s**, passed the tax grader, and retained both incomplete auxiliary calls.
Managed MCP discovery is now explicitly disabled as well for subsequent cells.

Two product corrections address the observed execution paths:

- Preserve the ChatGPT OAuth resolver when resolving a subagent model. The
  previous assignment erased it after catalog resolution: direct child title
  calls received HTTP 401 while normal child calls reconstructed OAuth.
- A noninteractive attachment does not start a dashboard turn-summary model
  call immediately before shutdown. Interactive dashboard summaries retain
  their existing path.

Focused verification: **12 tests passed** (four model-resolution tests, the
new headless regression, and seven existing turn-summary/config/roster tests).
The release build completed from `a6ad779d193d38f6cea17ac59f2a814134700090`;
binary SHA256: `da213b731fcd14e9642bf7ca821df2bc6fcea1b46c9d20b62c486c2a2bf61f9c`.

A separate native Codex probe successfully launched two children. Completed
response telemetry recorded **13 calls across three conversations**, with
**242,962 input / 178,560 cached input / 515 output tokens**. The parent CLI
reported only **127,805 input / 379 output tokens**. This demonstrates why parent
CLI totals alone are insufficient. All observed requests cost an estimated
**3.79525 Standard credits** at the published rates; this probe is excluded from
task speed comparisons. Its non-secret call records are in
[`subscription-accounting-probe.json`](tools/task_cost_eval/results/subscription-accounting-probe.json).

## Checkpoint 3: complete subscription accounting

The first pair after the auxiliary fixes has complete call usage, matching
ChatGPT account checks, unchanged frozen inputs, and passing external graders.
Manual diff review confirms both make the required two-line tax correction.
This is another diagnostic pair, not the final repeated cohort.

| Agent | Grade | Wall time | Calls | Input | Cached input | Output | Modeled credits |
|---|---|---:|---:|---:|---:|---:|---:|
| Codex Sol high | PASS | 43.28 s | 5 | 95,709 | 73,088 | 603 | 1.46452 |
| Distill Sol auto + Luna auto | PASS | 51.68 s | 10 | 72,996 | 16,384 | 1,126 | 2.1724185 |

Distill is **19.4% slower** and uses **48.3% more modeled credits** in this pair.
Excluding Codex's zero-output warmup gives a conservative baseline of **0.96247**
credits, against which Distill costs **125.7% more**. All three main Sol requests
reported zero cached input; the Worker did receive cached input. Both title
calls now have complete usage, and the cancelled headless dashboard call is gone.

The accounting runner checks request/response counts per native conversation,
deduplicates repeated telemetry exports, and rejects missing child usage or
unfolded Distill child attempts. **18 Python tests passed**, covering these
accounting gates and the existing runner/evaluator. Native OAuth auth mode is
recorded for every completed Codex response. Executable provenance includes the
native payload as well as its JavaScript launcher; the payload hash in this
diagnostic was corrected after identifying the installed package layout.

Raw call metadata: [`subscription-aux-fixes.json`](tools/task_cost_eval/results/subscription-aux-fixes.json).

## Checkpoint 4: native session header parity

The pinned [Codex 0.159.1 HTTP client](https://github.com/openai/codex/blob/rust-v0.159.1/codex-rs/codex-api/src/endpoint/responses.rs)
sends native session/thread headers alongside turn affinity. Distill already
forwards a stable prompt cache key and echoes the turn-state response header,
but omitted `session-id`, `thread-id`, and `x-client-request-id`.
Its streaming and auxiliary Responses requests now send those identifiers only
to the canonical ChatGPT subscription endpoint. Other providers are unchanged.

**Five focused sampler tests passed**, including real local HTTP capture of the
headers and existing turn-state scoping. A fresh build and measurement will
determine whether this correction improves cache or latency; header parity alone
is not evidence of a performance gain.

## Checkpoint 5: cache result and bounded local execution

The native-header diagnostic passed the grader with complete usage: **53.27 s**,
10 calls, 73,310 input / 49,024 cached input / 1,206 output tokens, and **0.9254505
modeled credits**. The later main requests received 12,544 and 12,928 cached
tokens. Cost is 36.8% below the previous Codex diagnostic including its warmup,
but only 3.8% below the conservative baseline excluding it. Speed still misses
the target. One Worker shell attempt used an unavailable `python` executable;
its retry and all associated model usage are included.

The next candidate asks Workers to batch independent named-file reads with
instruction discovery, confirm uncertain runtimes in that inspection, and
report a compact diff and check evidence. Main planning and independent final
verification remain required. The local-file benchmark uses the existing
`--tools` operator allowlist for shell commands, delegation, and child lifecycle
management. File reading, editing, search, and checks use the shell, which keeps
those capabilities available to both parent and Worker. Scheduling, feedback, workflow, and external
integration schemas are not needed by these tasks. This is an explicit runtime
profile for this cohort, not a claim about the default full toolset. A reserved
Codex compatibility config cell was also set here; later source inspection
showed its MCP surface is not implemented, so it did not affect discovery.

The user's installed configuration was aligned with subscription-only execution:
main/planning/review use ChatGPT Sol, Worker/local-worker/auxiliary models use
ChatGPT Luna, and Worker/utility effort is auto. The external Jev decision
service points to an unset credential variable so it defers to existing native
fallbacks. Codex now has `forced_login_method="chatgpt"`. Selected routes were
read back without exposing credentials; benchmark profiles remain isolated.

Raw header diagnostic: [`subscription-native-headers.json`](tools/task_cost_eval/results/subscription-native-headers.json).

Verification for the bounded candidate: the Worker discipline regression and
the primary/child orchestration renderer test passed; the 18 accounting and
runner/evaluator tests passed again. Candidate task timing remains pending.

## Checkpoint 6: bounded profile counterexample

The first bounded inventory diagnostic passed, but took **72.13 s** with 11 calls,
59,351 input / 28,928 cached input / 1,982 output tokens, costing **1.202059**
modeled credits. Its preceding native Codex diagnostic passed in **61.24 s** with
six calls and **1.6567** modeled credits (**1.15465** excluding zero-output warmup).
The bounded profile therefore did not meet either target in this diagnostic.
Manual diff review confirms both deliver the same minimal aggregation and
formatting correction.

The Worker still performed three separate inspections and used unavailable
`python` before retrying with `python3`. Main then split diff inspection and
behavioral verification, and one of its later requests missed the cache.
All of these calls remain in the totals. Raw candidate data:
[`subscription-bounded-local.json`](tools/task_cost_eval/results/subscription-bounded-local.json).

The next revision makes the existing fresh-Worker discipline a system instruction,
retaining its original fresh-child/model scoping. It asks for relative file names
and a compact report, and gives main an explicit instruction to combine its own
independent diff/check operations. Both agents now receive the same fact that
`python3` is available. Original fixture prompts and external graders are
unchanged; final-run prompt hashes will include that shared runtime fact.

Direct inspection then found that the bounded diagnostic's executable still
loaded the old generated primary prompt: its Markdown source had changed, but
the XOR-obfuscated compiled copy had not been regenerated. The existing template
freshness test failed, and the saved runtime prompt lacked the new batching
instruction. The generated copy is now synchronized; the orchestration test also
asserts that the compiled prompt contains the new main batching instruction.
This explains why that diagnostic cannot establish the effect of the main prompt
change. The Worker discipline was present in its saved conversation.

Verification after synchronization: the fresh-Worker scoping/authority test,
compiled orchestration renderer, and existing encrypted-template freshness
test all passed. The 18 Python accounting/runner/evaluator tests passed again.

## Checkpoint 7: compiled prompt readback and global import control

The synchronized tax diagnostic passed with complete usage in **48.83 s**:
nine calls, 52,351 input / 23,040 cached input / 1,097 output, **1.0826995** modeled
credits. Its saved main prompt contains the new instruction, and main combined
its diff and check in one terminal call. A later main request still missed cache.
Worker imported the global bug-fix skill, producing additional discovery work
and context. Raw data: [`subscription-synced-prompts.json`](tools/task_cost_eval/results/subscription-synced-prompts.json).

The benchmark profile now also disables the six installed Claude plugins and
vendor rule/skill/agent/hook imports, and excludes the user's `.agents` and
`.codex` skill roots through the existing skill-ignore setting. This prevents
unrequested global frameworks and plugin MCPs from entering these self-contained
local tasks. The unsupported Codex MCP compatibility entry was removed.
User-owned global files remain available outside the isolated benchmark profile.
Codex continues to use `--ignore-user-config`. This comparison concerns these
explicit runtime profiles; it does not claim identical built-in prompts or tools.

## Checkpoint 8: global imports removed from the measured profile

The next tax pair used the same prompt and frozen input hashes. Both passed the
external grader with complete subscription accounting. Runtime prompt readback
found no Claude-plugin or Stripe references, and Worker used two terminal calls.

| Agent | Wall seconds | Calls | Input / cached / output tokens | Modeled credits |
|---|---:|---:|---|---:|
| Distill | 41.017758 | 8 | 27,771 / 10,880 / 851 | 0.78627 |
| Codex high | 42.258616 | 5 | 98,094 / 74,880 / 568 | 1.48990 |

This diagnostic is **2.9% faster** and **47.2% lower in modeled credits**. Removing
Codex's zero-output request from its cost gives 0.98785 credits and a **20.4%**
reduction. It still misses both acceptance targets. Main independently inspected
the actual diff and executed behavioral assertions; its final request missed
cache again. This pair is exploratory, not the repeated final cohort.

Raw data: [`subscription-frozen-imports.json`](tools/task_cost_eval/results/subscription-frozen-imports.json).
Distill binary source: `8401878eec6e789c6f2b45dd6f783cf1dbbb34ef`; harness source:
`e6d67bdf9b4df984f6a136e7d863610984f7fa46`.

## Checkpoint 9: proportionate commands and compact tool arguments

The primary and Worker instructions now omit optional default/null tool
arguments, shorten a fully specified localized assignment, and prefer the
shortest behavioral check that proves the requirement. A localized check starts
with a representative case and relevant boundary, expanding for uncovered risk.
Main still reviews the actual diff and runs its final check independently.
The evaluator's frozen external graders are unchanged.

Verification: the compiled orchestration renderer, encrypted-template freshness,
and fresh-Worker scoping tests pass, as do the 18 Python evaluator tests. The
renderer assertion was updated for the shorter instruction after its original
wording assertion failed. A rebuilt release is required for the next measurement.

## Setup failures retained

- Before subscription-only scope was clarified, a direct API pilot failed with
  no credits. It is excluded from the subscription cohort and makes no cost or
  performance claim. The final runner has no API inference mode.
- Codex 0.147.0: unsupported Sol model and incompatible catalog metadata; no
  accepted task.
- Distill with 0.147.0 discovery: unknown Sol catalog ID; no accepted task.
- Distill's headless CLI rejects `--reasoning-effort auto`; auto is configured
  through `[jev].effort_auto = true` and `[models].worker_effort = "auto"`.
