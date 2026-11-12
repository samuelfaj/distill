# UltraFast

A session has one execution mode: Normal, UltraFast, UltraCode, or UltraSave.
Selecting a mode replaces the previous mode.
Use `/ultrafast on`, `/ultrafast off`, or `/ultrafast` in the interactive UI;
headless prompts accept `--ultrafast`. The CLI rejects simultaneous
execution-mode flags.

UltraFast prioritizes direct work. It advertises only direct-work tools: file
inspection and editing, search, shell and background-task output, and MCP
lookup. Planning, todo, delegation, workflow, feedback and scheduling schemas
are not sent, so they cannot spend a model round. It retains the requested model, explicit
reasoning-effort choices, permissions, task requirements, and verification
obligations. More agents alone do not establish a speed or quality improvement.

By default UltraFast is on its `v2` policy row, and its directive states only
what the harness enforces on that row (automatic effort set once per turn, at
medium or lower) or what the model must do itself: batch independent tool
calls, run focused checks and the full check once at the end, change the
approach after a failure, and confirm each stated requirement before
finishing. It gives no delegation advice. `[modes] v2 = []` restores the
previous directive. No campaign has measured the new directive yet; see
[Mode policy table](mode-policy.md#directives).

By default UltraFast also waits for nothing but the model and the checks: no
Jev question is awaited before a main request, the effort is decided once per
turn without waiting and capped at medium, the after-edit review waits at most
800 ms, and the utility model does not select tool results. It also runs no
subagent: a lone child costs its start-up and the hand-back, so the session
does the work itself. And it keeps the session's own model instead of
routing a round to a cheaper tier: a cheaper model is not reliably a faster
one (in a benchmark pilot the worker model answered more slowly than the
session model), and the result keeps the model you chose. Name a faster model
with `[modes.ultrafast] model` if you want one. A session pinned to UltraFast is not offered the spawn
tool; under Auto the tool list stays the same across modes and a spawn is
refused. `[modes] v2 = []` turns all of this off; it has not been benchmarked.
See [Decision policy by mode](jev-routing.md#decision-policy-by-mode).

## Model and provider path

These keys apply only to a session pinned to UltraFast (`/ultrafast on`,
`--ultrafast`, `/mode ultrafast`). Under Auto they never apply, whatever mode
Auto picks.

```toml
[modes.ultrafast]
model = "<model id>"      # optional; unset keeps the session's model
provider_tier = "fast"    # the default; "off" sends no serving-path field
```

- `model`: when the session is pinned, this model becomes the session's model
  as a session-only explicit pick (the account Auto routing setting on disk is
  not changed). Leaving UltraFast restores the previous model, unless you picked
  another model with `/model` in the meantime. The default is the session's own
  model: UltraFast is meant as the same model served faster, not a smaller one.
- `provider_tier`: `"fast"` by default; `"off"` turns it off. It is decided once
  when the session is pinned and kept until the session leaves the mode,
  because changing it mid-session invalidates the provider's prompt cache. It
  asks for the endpoint's fast serving path on the session's model only (a
  round routed to another model goes without it):
  - Responses API endpoints: `service_tier: "priority"`.
  - Anthropic Messages endpoints: `speed: "fast"`, except on a Claude
    subscription, which refuses fast mode without usage credits; nothing is
    sent there.
  - OpenRouter: the model variant defaults to `nitro` when `[models]
    main_variant` is unset; an explicit `main_variant` wins. No other field is
    sent to OpenRouter.

No provider has been probed for these fields, and this default has not been
benchmarked. Whether an endpoint accepts them depends on the endpoint, the
model and your plan, and a fast path usually costs more per token. When an
endpoint refuses a request that carried the field with HTTP 400, 404 or 422,
the session drops the field for the rest of the session, resends that request
once without it, records a `provider_tier_rejected` policy row and sends the
client one `x.ai/session/provider_tier/rejected` notice. No speed or cost claim
is made for these keys until a provider probe and a paired benchmark have been
run.

## Reproducible paired evaluation

The existing comparison runner accepts explicit `--baseline-mode` and
`--candidate-mode` values: `normal`, `ultrafast`, `ultracode`, or `ultrasave`. The historical
`--baseline-ultracode` and `--candidate-ultracode` flags remain available for
comparisons with older binaries; they cannot be combined with an explicit mode
for the same arm.

For ACP, explicit modes use `_x.ai/session/execution_mode/set` with
`{ "sessionId": "...", "mode": "ultrafast" }`. The runner requires the returned
`mode` to match before sending the task. The same acknowledgement is required
for explicit Normal and UltraCode arms. A passing fixture cannot compensate for
a missing or incorrect activation acknowledgement.

The initial evaluation is fixed before provider execution:

- Use all four cases in `cohort-ultracode-heldout-v1.json`: three substantial
  tasks and one small control, covering divisible and sequential work.
- Compare the same frozen candidate binary in Normal versus UltraFast, then
  UltraCode versus UltraFast. Run each case once in each arm: 16 model runs in
  total. Do not tune runtime behavior or graders on these results.
- Use `chatgpt/gpt-6.1-sol` for both main and task workers, the same existing
  utility tier, automatic effort in both arms, capacity eight, and a 900-second
  deadline. Effort is the same requested policy; the saved per-call ledger shows
  the actual effort applied. Leave depth overrides unset so each mode uses its
  own runtime policy.
- Keep pairs adjacent, shuffle cases with seed `20261020`, and alternate arm
  order across cases. Execute one comparison at a time to avoid competing study
  runs. Each run starts with a fresh fixture repository and isolated profile.
- Freeze and hash the binary, fixture, prompt, grader, grader helpers, and
  evaluator source. Existing graders are unchanged and run outside the model's
  workspace after its process tree has stopped.

The predeclared target is **at least 30% lower median paired completion time,
with no additional failed pairs**, separately against both Normal and
UltraCode. A quality regression means a baseline-passing pair fails in
UltraFast. Passing means satisfying the unchanged external grader and the
transport/completion checks; this bounded test cannot establish general quality
equivalence.

Completion time includes model startup, inference, tools, children, cleanup,
and external grading. A failed run is charged at least its full 900-second
deadline so an early failure cannot manufacture a speed gain. The report also
retains raw wall time for every run, including failures, median and slowest raw
time, all-call usage, actual model/effort counts, and observed active subagent
counts. Observed live children include waiting time and are not proof of
simultaneous provider inference. Main, worker, utility, title and retry usage
must pass the existing strict ledger audit; missing usage is unknown, not zero.
Modeled credits use the repository's rates and are not invoice charges.

Build and copy the candidate binary to an immutable absolute path, open the PR,
then run the commands below. The default prints the plan without provider calls;
append `--execute` for measured execution. Each comparison needs a new empty
work root. Use an existing signed-in ChatGPT OAuth profile; auth copies are
removed after every run.

```sh
python3 -B -m tools.task_cost_eval.compare_parallel \
  --baseline-binary /absolute/path/to/frozen-distill \
  --candidate-binary /absolute/path/to/frozen-distill \
  --baseline-mode normal --candidate-mode ultrafast \
  --transport acp --verdict ultrafast --max-concurrent 8 \
  --model chatgpt/gpt-6.1-sol --worker-model chatgpt/gpt-6.1-sol \
  --cohort tools/task_cost_eval/cohort-ultracode-heldout-v1.json \
  --order-seed 20261020 --repetitions 1 --timeout 900 \
  --distill-profile ~/.distill \
  --work-root /absolute/path/to/normal-vs-ultrafast-runs \
  --output /absolute/path/to/normal-vs-ultrafast.json

python3 -B -m tools.task_cost_eval.compare_parallel \
  --baseline-binary /absolute/path/to/frozen-distill \
  --candidate-binary /absolute/path/to/frozen-distill \
  --baseline-mode ultracode --candidate-mode ultrafast \
  --transport acp --verdict ultrafast --max-concurrent 8 \
  --model chatgpt/gpt-6.1-sol --worker-model chatgpt/gpt-6.1-sol \
  --cohort tools/task_cost_eval/cohort-ultracode-heldout-v1.json \
  --order-seed 20261020 --repetitions 1 --timeout 900 \
  --distill-profile ~/.distill \
  --work-root /absolute/path/to/ultracode-vs-ultrafast-runs \
  --output /absolute/path/to/ultracode-vs-ultrafast.json
```

Report both comparisons even if one misses its target. Do not omit failed runs,
replace them with retries, change the threshold after observing results, or
select only favorable cases. Any follow-up repetitions must repeat the full
cohort with the same settings and retain the initial results as a distinct
stage. One repetition is a bounded observation, not a reliable estimate of
provider variability or statistical evidence of equivalence. Binary build,
fixture staging, study orchestration, and manual review time are excluded from
the task timing and model-usage totals.

Runner regression checks use local fake peers and no model calls:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -B -m unittest \
  tools.task_cost_eval.test_acp_driver \
  tools.task_cost_eval.test_compare_parallel \
  tools.task_cost_eval.test_ultrafast_verdict
```

The [initial measured evaluation](ultrafast-results.md) passed all 16 task runs but
failed the 30% speed target against both Normal and UltraCode.
