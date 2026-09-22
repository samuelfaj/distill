# Paired task-cost evaluation

Status at CP1: this is a recording evaluator, a frozen smoke cohort, and a
one-cell runner. It has not launched Distill, Pi, a provider, or a paid
benchmark. T02 remains incomplete until real matched executions exist.

The evaluator is intentionally small and uses the existing artifacts:

- Distill `usage.json` supplies exact `session.costUsdTicks`; missing, partial,
  and incomplete billing data is never treated as zero.
- Distill `summary.json` supplies session identity and, when present,
  `headCommit`, which is the task workspace revision—not the executable build.
- Distill `updates.jsonl` supplies durable terminal evidence. A failed terminal
  turn is still an attempted execution.
- Pi JSONL session logs are normalized by the one-cell runner into the same
  `usage.json`/`summary.json`/`updates.jsonl` artifact shape. Pi's
  `usage.cost` is a tariff estimate, not a provider-billed statement; the
  normalized usage preserves that basis and mixed estimated/actual bases block
  savings claims. Missing, partial, or incomplete cost still blocks a claim.

No ambient log directory is read by the evaluator, and prompts, responses,
tokens, and credentials are never printed into the report.

## Identity and frozen protocol

Task identity and executable identity are separate fields in every manifest
record:

```json
{
  "task": {
    "revision": "task-worktree-commit",
    "prompt_sha256": "...",
    "fixture_sha256": "...",
    "grader_sha256": "..."
  },
  "runtime": {
    "executable": "distill",
    "version": "2.0.6",
    "build_id": "2c87833e5bb4",
    "executable_sha256": "...",
    "client_version": "2.0.6",
    "provider": "pinned-provider",
    "model": "pinned-model",
    "endpoint": "pinned-endpoint",
    "effort": "pinned-effort",
    "config_sha256": "..."
  }
}
```

The frozen `task_base_sha` is matched for the Distill pair. Runtime pins are
per-variant: executable version/SHA and config may legitimately differ between
the product baseline and the candidate build. The pair only requires the
runtime fields listed by its `match_runtime_fields` (model, endpoint, provider,
and effort for the controlled Distill comparison).

The current environment facts are recorded without pretending they are a
complete pin:

- Distill: installed `2.0.6 (2c87833e5bb4)`; the installed executable SHA-256
  is recorded in the cohort. The CLI reports default model `grok-4.7`, but the
  effective endpoint, effort, and sanitized config digest still require a
  controller pin.
- Pi: installed `0.73.1` on Node `22.23.2`/npm `10.9.8`; the CLI executable
  SHA-256 is recorded. Provider/model/config are still controller-pinned.
- `distill-jev-off` is blocked until the candidate executable is built and its
  immutable identity is recorded. No current-vs-ablation result is eligible
  merely because both worktrees share `task_base_sha`.

`cohort-v1.json` contains three concrete smoke cases, each with a non-empty
prompt, fixture tree, behavior grader, and SHA-256 digest for all three. The
grader commands reject unchanged or wrong behavior. The planned shape is
3 cases × 3 variants × 3 repetitions = 27 cells. Pi is explicitly blocked
until its CLI/provider/model command is pinned.

The loader verifies all declared prompt, fixture, grader, and pinned-runtime
digests. It also requires the runtime metadata in every run, checks the
summary model/effort against the recorded runtime, and rejects a run whose
behavioral acceptance disagrees with its grader exit code. A completed accepted
run must have `grader_exit_code: 0`; an `agent_failed` run cannot have
`grader_exit_code: 0`.

## Evaluator command

From the repository root:

```sh
python3 -m tools.task_cost_eval report \
  --cohort tools/task_cost_eval/cohort-v1.json \
  --runs /path/to/recorded-runs.jsonl \
  --output /path/to/task-cost-report.json
```

The command exits `0` when it can produce a report, including an intentionally
blocked report. It exits `2` for malformed input, duplicate cells, unplanned
cells, or a broken frozen protocol. A blocked report is expected for incomplete
recordings; rows must not be dropped to make it eligible.

## Cost and acceptance policy

The primary economic metric includes every attempted execution:

- A variant's complete total is the sum of complete costs from all attempts,
  including rejected and `agent_failed` attempts.
- Headline cost per accepted task is that all-attempt total divided by accepted
  count. It is unavailable if any planned/recorded attempt has missing,
  partial, or incomplete cost, if a planned cell is missing, or if no task was
  accepted.
- A comparison sums all matched baseline/candidate attempts for its headline
  totals and per-accepted metrics. Missing or partial cost in a rejected or
  failed attempt blocks the claim and is never zero.
- `secondary_matched_accepted_*` fields are diagnostic only; they are never the
  savings headline.
- Savings is allowed only when every planned pair is present, execution and
  behavioral acceptance are known, every attempt has complete cost, task
  revision is frozen, required runtime fields match, quality is preserved, and
  the candidate's all-attempt cost per accepted task is strictly lower.

For example, accepted 100 plus failed 10 is 110, while accepted 50 plus failed
1000 is 1050. The accepted-only 100 versus 50 comparison cannot recommend a
saving.

## One-cell runner

`runner.py` is deliberately one-cell only. It copies the frozen prompt and
fixture into the canonical temporary worktree layout, runs the frozen grader
from its original absolute path, uses `--grok-home` as a parent for the
per-cell Distill `GROK_HOME` (`<parent>/<cell_id>`) and per-cell Pi
session/config directories, and invokes the existing CLI in its own process
group. `--config-file` is copied as `GROK_HOME/config.toml` for Distill or
`PI_CODING_AGENT_DIR/models.json` for Pi; the applied bytes are hash-checked.
The supplied `--grok-home` must already be an existing parent directory; no host
Distill state is copied into the cell.
`--timeout-seconds` is finite, and post-dispatch failures still append an
inconclusive manifest row with any raw artifacts found. Without `--execute` it
prints a command plan without creating the run cell; with an unpinned variant
it refuses to run.

Example after the controller has frozen the runtime pin:

```sh
python3 -m tools.task_cost_eval.runner run-one \
  --cohort tools/task_cost_eval/cohort-v1.json \
  --case tax-bug-en \
  --variant distill-current \
  --repetition 1 \
  --work-root "$RUN_ROOT" \
  --grok-home "$ISOLATED_GROK_HOME" \
  --config-file "$CONFIG_FILE" \
  --timeout-seconds 120 \
  --execute
```

The runner pins the effective model/provider/effort CLI overrides in the
manifest and does not accept `--api-key` or any credential as an argument.

## Paid-execution proposal for controller authorization

No paid command has been run at this checkpoint. The exact first pilot should
be one cell only, using the discovered Distill model and an explicitly pinned
effort:

```sh
GROK_JEV=1 GROK_HOME="$ISOLATED_GROK_HOME" distill --no-leader \
  --no-subagents --disable-web-search --cwd "$WORKTREE" \
  --session-id "$SESSION_ID" --prompt-file "$PROMPT_FILE" \
  --output-format streaming-messages-json \
  --model grok-4.7 --reasoning-effort max \
  >"$RUN_DIR/stdout.ndjson" 2>"$RUN_DIR/stderr.log"
python3 tools/task_cost_eval/graders/grade_tax_bug.py "$WORKTREE"
```

The ablation command is identical with `GROK_JEV=0` and the candidate binary
after it is built and pinned. The command above is a proposal, not execution;
the cohort remains blocked until endpoint/effort/config and candidate identity
are frozen.

Initial authorization gate:

```text
pilot scope: 1 Distill product-baseline cell (tax-bug-en, repetition 1)
maximum paid task executions: 1
hard maximum spend: USD 2.00
stop before any second cell or retry
```

The CLI does not expose a provider price snapshot, so an honest token-based
spend estimate cannot be derived from fixtures. USD 2.00 is a proposed hard
ceiling, not a measured cost estimate. After the controller reviews the real
pilot artifact and price, a separate authorization is required for the
18-cell Distill pair (3 cases × 3 repetitions × 2 variants); Pi remains blocked.

Required credential presence is checked as boolean only and never printed or
stored in the manifest: authenticated Distill provider/config in the isolated
`GROK_HOME`, `JEV_API_KEY` for the enabled Jev process, and later the pinned
Pi provider credential (the installed CLI supports the inherited OpenRouter or
OpenAI environment names). No credential value is passed in argv.

## Verification boundary

Focused checks:

```sh
python3 -m unittest tools.task_cost_eval.test_evaluate
python3 -m unittest tools.task_cost_eval.test_runner
python3 -m unittest \
  tools.task_cost_eval.test_evaluate.TaskCostEvaluationTest.test_expensive_failed_attempt_cannot_create_savings
python3 -m tools.task_cost_eval --help
python3 -m tools.task_cost_eval.runner --help
```

These tests validate the evaluator and protocol only. Synthetic records are
not real measurements, and no report from them can complete T02, T20, or T21.
