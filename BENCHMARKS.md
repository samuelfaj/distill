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

## Setup failures retained

- Before subscription-only scope was clarified, a direct API pilot failed with
  no credits. It is excluded from the subscription cohort and makes no cost or
  performance claim. The final runner has no API inference mode.
- Codex 0.147.0: unsupported Sol model and incompatible catalog metadata; no
  accepted task.
- Distill with 0.147.0 discovery: unknown Sol catalog ID; no accepted task.
- Distill's headless CLI rejects `--reasoning-effort auto`; auto is configured
  through `[jev].effort_auto = true` and `[models].worker_effort = "auto"`.
