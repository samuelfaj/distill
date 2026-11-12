# UltraSave

UltraSave targets lower token use while preserving task requirements and
verification. A session has exactly one execution mode: Normal, UltraFast,
UltraCode, or UltraSave. Selecting `/ultrasave on` replaces the previous mode;
headless prompts use `--ultrasave`. Model and effort pins, permissions, and
required checks remain authoritative. A cheaper model or an output-token cap
is not the token-saving policy evaluated here.

## Execution policy

The mode directive favors direct execution, narrow searches and reads, reuse of
unchanged evidence, and concise reports. Required checks and error evidence
remain part of completion.

By default UltraSave is on its `v2` policy row, and its directive states only
what the harness enforces on that row (automatic effort chosen per round, at
high or lower) or what the model must do itself: reuse evidence already in
context, batch independent reads and edits, keep reports to paths and check
results, and test every stated requirement before finishing. It no longer
suggests delegating to a subagent. `[modes] v2 = []` restores the previous
directive described above. No campaign has measured the new directive yet;
see [Mode policy table](mode-policy.md#directives).

UltraSave advertises only direct-work tools: file inspection and editing,
search, shell and background-task output, and MCP lookup. Planning, todo,
delegation, workflow, feedback and scheduling schemas are not sent, so their
schemas are not repeated on every model call and cannot spend a whole round.
This replaces the earlier utility-model tool-family selection for the mode. It
reduces the default selected
skill-descriptor limit from eight to four while preserving explicitly requested
and active skills and access to the recoverable full catalog. It reuses an
automatic effort decision within the turn, invalidates that reuse after a tool
failure, and skips the second model-tier effort decision. Explicit effort pins
and the existing retry effort floor remain authoritative. These mechanisms
target token economy; the benchmark below determines their observed effect.

## Model and provider path (opt-in)

These settings apply only to a session pinned to UltraSave (`/ultrasave on`,
`--ultrasave`, `/mode ultrasave`). Under Auto they never apply, whatever mode
Auto picks.

```toml
[modes.ultrasave]
model = "<model id>"        # optional; "worker" means the [models] worker model
provider_tier = "economy"   # optional; unset sends no serving-path field
```

- `model`: when the session is pinned, this model becomes the session's model
  as a session-only explicit pick (the account Auto routing setting on disk is
  not changed). `model = "worker"` uses the configured `[models] worker`. A
  worker model alone, without this key, changes nothing; the utility model is
  never used, and models are not ranked automatically.
  Leaving UltraSave restores the previous model, unless you picked another
  model with `/model` in the meantime. Compaction and other auxiliary requests
  run on the session's model, as before.
- With `model` unset (or `"worker"` with no worker configured), UltraSave has
  no cheaper model and keeps the session's model; the session log records this
  once.
- `provider_tier = "economy"`: decided once when the session is pinned and kept
  until the session leaves the mode, because changing it mid-session
  invalidates the provider's prompt cache. It applies to the session's model
  only (a round routed to another model goes without it):
  - Responses API endpoints: `service_tier: "flex"`.
  - Anthropic Messages endpoints: nothing is sent.
  - OpenRouter: the model variant stays `floor` when `[models] main_variant`
    is unset, which is already the default.

Whether an endpoint accepts `service_tier` depends on the endpoint, the model
and your plan; a flex tier may also answer more slowly. If requests start
failing after you set `provider_tier`, remove the key. No cost or speed claim is
made for these settings until a provider probe and a paired benchmark have been
run.

## Predeclared paired evaluation

The initial UltraSave comparison is fixed before live provider calls:

- Compare **Normal versus UltraSave using the same new frozen binary**, with
  explicit mode acknowledgements through ACP. Do not reuse the older UltraFast
  binary or its Normal runs as the baseline for this comparison.
- Use all four cases in `cohort-ultracode-heldout-v1.json`, once per arm: eight
  runs. This existing cohort covers three substantial tasks and one control,
  including divisible and sequential work. It was already used for UltraFast
  and is not fresh unseen confirmation for UltraSave. Keep prompts, fixtures,
  references, and external graders unchanged; do not tune runtime policy to
  these particular tasks.
- Main and task workers use `chatgpt/gpt-6.1-sol`; the existing utility and title
  tier uses `chatgpt/gpt-6-luna`. Use automatic effort in both arms, capacity
  eight, no depth override, and the same 900-second deadline.
- Keep pairs adjacent, shuffle cases with seed `20261020`, and alternate arm
  order across cases. Run only one comparison at a time. Each run gets a fresh
  fixture repository and isolated profile using the same OAuth account.
- Freeze hashes for the binary, evaluator, fixtures, prompts, graders, and
  grader helpers. The existing strict accounting audit must confirm every
  main, worker, utility, title, and retry call; missing usage is unknown and
  makes the comparison `INCOMPLETE`, never zero.

The target is **at least 20% lower median paired total tokens, with zero
additional baseline-passing pairs failing in UltraSave**. A passing task must
satisfy the unchanged external grader and ACP completion checks. This is a
bounded observed quality comparison, not statistical equivalence or proof of
general superiority. Wall time and modeled credits are reported as tradeoffs;
there is no speed target for this token-economy comparison.

Total tokens are the sum of **provider-reported input tokens plus output
tokens over every physical call**. Input already includes cached input;
output already includes reasoning tokens. Neither subset is added again.
Uncached input is `input - cached_input`. The evaluator reconciles each field
against the all-call ledger before computing ratios. It does not use the
runtime's `total_tokens` field, which can represent normalized context-window
usage rather than provider billing usage.

Every pair reports input, cached input, uncached input, output, reasoning,
total tokens, call counts, acceptance, credits, and time for both arms. The
aggregate includes all runs, including failures. A pair receives token-savings
credit only when both arms pass; otherwise its gate ratio is at least 1.0,
while its raw ratio and actual usage remain visible. Any baseline-pass to
UltraSave-fail regression fails the verdict regardless of token reduction.
Do not omit failed runs, replace them with retries, select favorable cases, or
change the target after observing results.

The following command prints the exact run plan without provider calls. After
the PR contains the implementation and this protocol, append `--execute` to
run it. Use an immutable absolute binary path and a new empty work root:

```sh
python3 -B -m tools.task_cost_eval.compare_parallel \
  --baseline-binary /absolute/path/to/new-frozen-distill \
  --candidate-binary /absolute/path/to/new-frozen-distill \
  --baseline-mode normal --candidate-mode ultrasave \
  --transport acp --verdict ultrasave --max-concurrent 8 \
  --model chatgpt/gpt-6.1-sol --worker-model chatgpt/gpt-6.1-sol \
  --cohort tools/task_cost_eval/cohort-ultracode-heldout-v1.json \
  --order-seed 20261020 --repetitions 1 --timeout 900 \
  --distill-profile ~/.distill \
  --work-root /absolute/path/to/normal-vs-ultrasave-runs \
  --output /absolute/path/to/normal-vs-ultrasave.json
```

Timing includes model startup, inference, tools, children, and cleanup, with
external grading time reported separately. Build, fixture staging, study
orchestration, and manual review are excluded. Credits use the repository's
modeled rates, not invoice charges. Preserve the original UltraFast results
and raw artifacts independently of this new comparison.

Local regression checks use fake ACP peers and no model calls:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -B -m unittest \
  tools.task_cost_eval.test_acp_driver \
  tools.task_cost_eval.test_compare_parallel \
  tools.task_cost_eval.test_ultrafast_verdict \
  tools.task_cost_eval.test_ultrasave_verdict
```

No measured UltraSave token-saving or quality result is claimed by this
protocol. One repetition does not establish provider variability; any
follow-up must repeat the full cohort and retain the initial stage.

## Measured result

The [initial live evaluation](ultrasave-results.md) passed all eight executions but failed the 20% token-reduction target: median paired total token use increased 2.11%. Aggregate token use decreased 1.14%; modeled credits decreased 17.47%, alongside higher cache use. These are separate metrics.
