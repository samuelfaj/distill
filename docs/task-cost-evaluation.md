# Paired task-cost evaluation

Status at E3: this is a recording evaluator, a frozen smoke cohort, and a
one-cell runner. Two authorized real-provider smoke cells are recorded below;
they are not a matched benchmark and T02 remains incomplete.

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

## E3 recorded smoke checkpoint

The only two authorized provider runs used artifact root
`/tmp/jev-cost-smoke.bfjnuH`; there were no retries or fallback models. The
runtime pins and raw artifacts are retained there for controller readback;
the metadata below is sanitized:

| Cell | Frozen runtime | Result and accounting |
| --- | --- | --- |
| `tax-bug-en--distill-jev-off--1` | Distill 2.0.6, build `2c87833e5bb4`, executable SHA-256 `b4a7ec2f7aef7f3524655688eef28b66d8d80e84954dee2ecc537548eb8aa10e`, config SHA-256 `d49d5172b59252709fc1c6d5361c013434a8d01461c8017d45e442937755605a`, `qwen/qwen3.7-flash`, effort `none`, max turns `12` | Process exit 0 but post-dispatch verification was inconclusive; no grader result. Six model calls and 202,091 total tokens were retained, but cost is unknown/incomplete. Session `1e1e9b3e-954e-482c-aa01-0393c5e42d3c`. |
| `tax-bug-en--pi-smoke--1` | Pi 0.73.1, build `pi-0.73.1`, executable SHA-256 `e959f463b06ddd15ed882783ac02f39ecaeef950ef967da86380f39dda6595fa`, config SHA-256 `2b288184c28b1e9ea0d9e3f2d3f1f992059d0eaf142659871eedb5c0059ae0f5`, `qwen/qwen3.7-flash`, thinking `off` | Grader exit 0 and accepted. Four calls, 7,625 total tokens, and 1,642,020 ticks (`$0.000164202`) from Pi tariff estimates. This is an estimate, not provider billing. The four returned generation IDs remain in the raw JSONL; each official generation-metadata lookup returned HTTP 404, so actual billing remains unknown. Session `01a0c6ea-4b60-7634-af61-ac86386a4606`. |

The Distill attempt remains inconclusive because the experiment preparation
mutated its protocol, not because the historical binary failed. The separate
receipt snapshots are:

```text
Distill receipt line 153: script_ref=tools/task_cost_eval/graders/grade_tax_bug.py
  inferred protocol root: empty; staged fixture: worktree/fixtures/tax_bug
Pi receipt lines 185/193: script_ref=graders/grade_tax_bug.py
  corrected protocol root: tools/task_cost_eval; staged fixture: worktree/tools/task_cost_eval/fixtures/tax_bug
Evidence source: /tmp/distill-cost-orchestration-20260921-b02qc6vm/E3.smoke.events.jsonl
  lines 153, 185, and 193; corrected Pi cohort: /tmp/jev-cost-smoke.bfjnuH/cohort.json
```

The same temporary `cohort.json` was changed between those receipts. The
original Distill row stays inconclusive and the corrected Pi row stays as
recorded; the old rows do not carry the new cohort digest, so neither is
retrofit or paired. The offline frozen grader against the original Distill
worktree exited 1 because its canonical fixture path was absent; it created no
generation and did not change the manifest. Do not reuse the 201,744 versus
7,226 token totals as a clean overhead comparison.

The runner now stages all three cases under the fixed canonical
`tools/task_cost_eval` root, copies each source cohort to the cell, records its
SHA-256 in `task.cohort_sha256`, and checks the source before and after
dispatch. It also uses `--always-approve` only for authorized isolated fixture
cells, validates positive pinned `--max-turns`, and records both effective
settings for future reproducibility.

The controller's TOML comparison found only the bootstrap key
`marketplace.default_skills_installs_purged` changed between the original and
applied Distill config. The config-integrity gate remains strict; a future
frozen preflight must include a legitimate initial marker before hashing and
must not mutate these historical pins. These two smoke cells establish no
savings, price, or other financial conclusion.

## Identity and frozen protocol

Task identity and executable identity are separate fields in every manifest
record:

```json
{
  "task": {
    "revision": "task-worktree-commit",
    "prompt_sha256": "...",
    "fixture_sha256": "...",
    "grader_sha256": "...",
    "cohort_sha256": "..."
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

`cohort_sha256` is the SHA-256 of the exact cohort JSON bytes evaluated for the
run. A pair is eligible only when both rows carry that digest and it matches
the currently evaluated cohort. Missing or historical digests and different
digests remain visible in the run and variant cost totals, but are explicitly
unpaired and cannot support a savings claim.

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

An executed cell also preserves the exact source cohort bytes as
`cohort.json`; historical rows without `task.cohort_sha256` remain historical
evidence and are not backfilled.

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

The runner pins the effective model/provider/effort CLI overrides, approval
setting, and optional turn limit in the manifest and does not accept
`--api-key` or any credential as an argument.

## Paid-execution proposal for controller authorization

No further paid command is authorized in this slice. The smoke cells above are
not the matched T02 baseline; a future controller authorization should still
use one cell only, an explicitly pinned model/effort, and a finite ceiling:

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

## T05 first matched-pair readiness (2026-09-22)

The next exploratory protocol is prepared outside the historical cohort under
`/tmp/distill-cost-orchestration-20260921-b02qc6vm/pilot-prep/`. It plans one
`tax-bug-en` pair (`product-baseline-d5` versus `candidate-bec1-r1`) to prove
billing and isolation, not an overall product-gain result. The copied
active-records and inventory-report fixtures remain the expansion pool; the
historical cohort and smoke receipts are unchanged.

CP31's functional Mocha receipt (`739a2fb0`) is retained as functional proof
only; it does not close T01–T21, late accounting, or financial acceptance.

The config pins the primary and auxiliary catalog routes to the same declared
OpenRouter Qwen model and environment-key name, and requires the initial title
attempt to be accounted for even though title refresh is disabled. Those routes
are the generic Qwen Chat Completions decision adapter over OpenRouter; they are
not an actual Jev service endpoint, and using the same backend is valid here for
exploratory accounting/isolation only, not empirical calibration of Jev
primitives. The current candidate runtime row is intentionally pending E2/E3 R1
fixes and a fresh artifact. The baseline hash is an identity receipt only and
also remains pending a late-accounting rebuild and proof; neither row is ready
for execution. Public price math is only a conservative planning envelope.
Provider-reported complete USD cost for every physical attempt remains
mandatory; missing, partial, late, or unknown billing blocks the cell and is
never zero.

The exact protocol, redacted config, cohort, and public price snapshot are in
`pilot-prep/protocol-t05-first-pair.md`, `pilot-prep/distill-config.toml`,
`pilot-prep/cohort-t05-first-pair.json`, and
`pilot-prep/price-snapshot-qwen3.7-flash.json`. The existing one-cell runner
does not enforce an aggregate provider-call ceiling, so the protocol records
that finite eligibility/stop condition as a controller dependency rather than
presenting wall-clock timeout as a hard spend guarantee. This readiness artifact
does not establish a savings claim.

## Paired campaign yardstick

`compare_parallel.py` runs paired campaigns (`--baseline-*` against
`--candidate-*`). Without `--execute` it only prints the run plan. The gate
rules below apply to the `ultrafast`, `ultrasave` and `auto` verdicts:

- Gates decide on the ratio of totals: candidate total over baseline total,
  pooled over every case and repetition. Credits charge a failed candidate
  attempt at least its baseline's credits; completion time charges a failure
  the timeout. The median of paired ratios is still reported, but it reads
  about 1.1 for two identical arms and is never the gate.
- Each ratio carries a 90% bootstrap interval that resamples whole cases, so
  repetitions of one case are not counted as independent tasks.
- A gate verdict needs 3 repetitions. With fewer, a finished report becomes
  `SMOKE`, keeps its would-be result in `smoke_verdict`, and exits 1.
- Auto accepts at least the compared arm's count minus 1 in 60 runs; it no
  longer needs every run to pass.
- `--aa-arm` adds a third arm that repeats the baseline binary and settings.
  Arm order rotates by case and repetition. When the A/A ratio of totals
  leaves 0.90-1.10 on credits or on charged time the campaign is `INVALID`;
  when it cannot be measured the campaign is `INCOMPLETE`. The A/A rows stay
  in `runs`, but the verdict only pairs baseline and candidate.
- With `--enable-jev` and `--execute`, one Jev decision request is sent with
  the staged key before the first run (same endpoint, bearer header and typed
  envelope as the Rust client). A failure aborts the campaign with exit code 2
  and the HTTP status; the result records `jev_preflight`. A run whose Jev
  calls failed is reported as `Jev unavailable (N of M calls failed)` instead
  of the generic incomplete-accounting reason.

The result JSON also records:

- `binaries.<arm>.source_commit` and `commit_source`: taken from
  `<binary> --version` when it prints `(commit)`, else from
  `--baseline-commit` / `--candidate-commit`, else `unknown`.
- Per call, `api_duration_ms` from the usage ledger. Per run and per
  aggregate, `model_time_ms`, `jev_time_ms`, `root_rounds` (root main and
  retry calls), `child_calls` (worker calls other than Jev) and
  `jev_failed_calls`.
- Per ACP run, `effective_mode_timeline`, `effective_mode_mix` and
  `mode_switches`, read from the `x.ai/session/execution_mode/changed`
  notifications that Auto sends after each pick. The mix counts entries into a
  mode, not model calls; it sits next to the existing effort mix.
- `failed_run_dirs`. Run folders are never deleted by the runner; only the
  staged credential copies are. `--keep-failed-runs DIR` copies every failing
  run folder to `DIR` without credential files, so the evidence survives a
  temporary work root.

`--verdict normal --compare-versions` runs Normal old against new: it passes
when the credit and time ratios of totals stay at or under 1.10 and the new
build accepts at least the old count minus 1 in 60 runs.

## Matrix campaigns

`--arms` replaces the fixed pair with N arms on one `--binary`. Listing
`normal` twice adds the A/A arm (`normal_aa`); every other mode appears at most
once. Arms of one case run next to each other and the leading arm rotates one
step per case, so over a campaign each arm leads equally often. A dry run
needs no binary, work root or output:

```sh
python3 -B -m tools.task_cost_eval.compare_parallel \
  --arms normal,normal,ultrasave,ultrafast,ultracode,auto \
  --cohort tools/task_cost_eval/cohort-ultrafast-v2.json --repetitions 3
```

An executed campaign adds `--binary`, `--transport acp`, `--work-root` and
`--output`. The source commit of the binary can be given with
`--candidate-commit`. Two model-set profiles are run as two campaigns and
reported separately:

- `single` (default): no worker model; the main model does all the work.
- `tiered`: `--ultrasave-model <model>` writes `[modes.ultrasave] model` into
  every run config.

The result records `arms`, `model_profile` and `ultrasave_model`, and is
judged by `modes_verdict.py` (`python3 -m tools.task_cost_eval.modes_verdict
single.json tiered.json` prints both profiles):

- `INCOMPLETE` without a Normal and an A/A arm, or with missing evidence;
  `INVALID` when the A/A ratio of totals leaves 0.90-1.10 on credits or time.
- Every other arm is compared with Normal on ratios of totals with a case
  bootstrap. A saving counts only when it also clears twice the A/A
  deviation.
- UltraSave (tiered profile only): credits per accepted run at most 0.45x
  Normal and 0.75x UltraFast. The single profile makes no UltraSave money
  claim.
- UltraFast: charged time at most 0.90x Normal and credits at most 1.15x.
- Both: accepted at least Normal minus 1 in 60 runs and silent failures at
  most 1 in 60. A silent failure is a failed grader on a run with no
  escalation row and no failed-check fact, read from the optional run fields
  `escalations` and `failed_check_facts`. A failing run without those fields
  is counted as unknown, and an unknown blocks the claim.
- A mode that misses its gate shows `no claim yet`; the separation of
  UltraSave and UltraFast is reported, not gated.
- Auto (`auto_verdict.matrix_verdict`): accepted at least the best pinned arm
  minus 1 in 60. It reports regret, Auto's total over the best single pinned
  mode's total on credits and on charged time (among pinned arms that meet
  the same acceptance bar), and a per-case oracle chosen on odd repetitions
  and scored on even ones.
- UltraCode (`ultracode_verdict.py`, also runnable on its own): needs the case
  split, given with `--substantial` and `--control`. GO needs at least +15
  points of accepted runs on substantial cases with a one-sided 90% bootstrap
  lower bound above 0, no extra control failures, credits at most 3.0x and
  time at most 2.5x on substantial cases, review overhead at most 240 s p90
  per code-edit turn on controls (`review_overhead_s`), and at least half of
  the extra passes refuted before the fix (`refutation_before_fix`). A
  missing optional field gives `INCONCLUSIVE`.
