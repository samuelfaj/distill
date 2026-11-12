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

### Decision quality

After each run the runner reads the root session's `events.jsonl` in the run's
isolated home and adds to the run row, for passing and failing runs alike:
`turn_outcomes` (the `turn_outcome` records), `policy_counts` (summed from
those records, or counted from `mode_policy` events when there are none),
`decision_events` (per lever: asked, answered, no-answers by kind, applied by
label when the events carry `applied`, labels), `decision_labels` (the outcome
label counts of the `decisions` report below), `first_mode_pick` and
`decision_events_found`. `escalations` and `failed_check_facts` are filled
from the turn outcomes (or, for failed checks, from `check_recorded` events)
and left absent when the run has neither, so the silent-failure count stays
unknown rather than zero. `review_overhead_s` and `refutation_before_fix`
cannot be derived from these events and stay absent.

- Every aggregate group carries a `decision_quality` block: per lever asked,
  answered, applied and label counts; the outcome labels, their rates and
  coverage; and `label_agreement`, the agreement with the hidden grader of
  each label that claims a good or bad end state (`mode.lean_ended_failing`
  claims a failing run, `mode.lean_ended_clean` and
  `final_check.false_alarm` a passing one). A label under 0.8 agreement is
  marked `discarded`; it is reported and used by no gate.
- Auto's first pick of a run is the target of the first request-start
  `mode_switched`, else the first `b8_execution_mode` decision of that turn.
  The per-case oracle of `matrix_verdict` treats arms within the A/A
  deviation of the chosen arm as tied, and reports per metric the confusion
  matrix of Auto's first pick against the oracle (a pick of any tied mode
  agrees), `regret` as before and `regret_delta` in credits or seconds. With a
  cohort that declares `expected_mode` (the result records `expected_modes`),
  `first_pick_vs_expected` gives the same matrix against the expected mode.
  Unknown picks are counted but not scored.
- `--jev-off [MODE:]LEVER` (repeatable, `--arms` only) adds an arm
  `<mode>-no-<lever>` (MODE defaults to `normal` and must be one of the arms)
  whose run config sets `[jev.ladder] <lever> = false`; `final_check` turns
  off `c1_premature_stop` and `c3_completion_check` together. Lever names are
  checked against the `[jev.ladder]` keys, and a test keeps that list equal to
  `JevLadderOverlay` in `jev/flags.rs`. Lever-off arms are compared with Normal
  and reported, never used as pinned oracle arms. Planned order, one per
  campaign already scheduled: `final_check`, `c4_diff_risk`,
  `p6_skill_suggestion`, `b2_model_tier`.
- Decision gate line: the candidate's decision rates within the A/A arm's
  deviation from the control, reported as `PASS`, `FAIL` or `NO DATA`.
  `modes_verdict` reports it for every arm (`decision_gate`); no mode claim
  depends on it, so it never blocks one. `auto_verdict` requires it only when
  Auto is compared with a previous Auto (`--compare-versions` with the A/A
  arm): there `FAIL` or `NO DATA` fails the verdict.

All of this is covered by tests on synthetic event streams and synthetic run
rows only. The event types other than `jev_decision` and `jev_no_answer` are
read against the shapes the harness is meant to emit; no real session or
campaign has been read with it yet.

## Mode cohorts

The 20-case cohort is small single-prompt work: Auto never picks Normal or
UltraCode on it and no run reaches history eviction or compaction. Three more
cohorts exist for what it cannot show. Their pass rates, lengths and expected
modes are design targets; none has been run against a model yet.

- `cohort-auto-v1.json`: 12 cases, three per mode a careful human would
  expect (`expected_mode`, reporting only). Three are two-prompt sessions whose
  second prompt needs a different kind of work (`expected_followup_mode`).
- `cohort-long-v1.json`: 4 tasks designed for 150 or more tool rounds
  (`rounds_target`), each with `timeout_s: 5400`. `generate_long_fixtures.py`
  writes the fixtures, references and grader data, and a test checks that
  regenerating reproduces them byte for byte.
- `cohort-ultracode-hard-v1.json` and `cohort-ultracode-hard-heldout-v1.json`:
  6 cases each (4 hard, 1 sequential control, 1 small control; `difficulty`),
  aimed at a 30% to 70% Normal pass rate. Each hard case has an
  obvious-but-wrong solution under `obvious/` (`obvious_ref`) that passes the
  fixture's visible tests and fails the hidden grader; `test_mode_cohorts.py`
  checks that for every hard case.

Case fields the runner reads:

- `followup_prompt_ref`: a second prompt sent in the same session after the
  first turn ends; the run is graded once at the end. It needs the ACP
  transport and is part of the frozen inputs.
- `timeout_s`: replaces `--timeout` for the case. A campaign whose selected
  cases declare different timeouts is rejected, because verdicts compare one
  timeout across runs.

`--workspace-profile realistic` seeds each run's isolated home with 12
synthetic skills, an `AGENTS.md` committed as task input and a local stdio MCP
stub with two read-only tools, so a campaign measures the prefix a configured
user has. The default, `plain`, is unchanged. The second prompt and the MCP
stub are covered by tests against the fake ACP peer only: they have not been
run with the real binary.

## Per-mode requested cohorts

The requested cohorts in `BENCHMARKS.md` compare a native maximum-effort
baseline with one Distill arm on the three `cohort-v1.json` tasks. A per-mode
campaign keeps those arms, families, tasks, the 300-second deadline and the
graders, and runs one Distill arm per execution mode. `compare_codex --variant
distill` and the Distill variants of `compare_additional` take `--mode
normal|auto|ultrasave|ultrafast|ultracode`: `--mode normal` and `--mode auto`
as written, the Ultra modes as `--ultrasave`, `--ultrafast` or `--ultracode`
(the flags `compare_parallel.py` passes on its CLI transport). Normal is pinned
explicitly because a current binary starts a new session in Auto; Distill
2.0.46, which produced the 2026-10-16 results, predates Auto and ran Normal, so
a result without an `execution_mode` key reads as Normal. Without `--mode` the
command is unchanged. Each result records `execution_mode` and
`execution_mode_activation_confirmed` (the headless line `<Mode> enabled for
session <id>`); native variants reject `--mode`.

1. The cohort driver plans every cell; with `--execute` it runs them:

   ```sh
   python3 -B -m tools.task_cost_eval.requested_cohort --families astra,sol,opus,deepseek \
     --distill "$(command -v distill)" --claude-auth "$CLAUDE_AUTH_FILE" [--execute]
   ```

   Per family: the native arm and one Distill arm per `--modes` entry (all five
   by default), three tasks, `--repetitions` (3), so 54 cells, run in order with
   the arm order reversed on alternate task/repetition groups. Each cell gets a
   fresh directory under `--output-root` (default `/tmp/requested-DATE`) and
   runs once: a failed or timed-out cell stays in the result and is never
   rerun. It writes `results/requested-DATE-FAMILY.json` (the 2026-10-16 family
   shape plus `mode` per run, `requested_arms`, `modes`, `timeout_s` and
   per-arm `totals`), and refuses to start when that file or any cell directory
   exists. Without `--execute` it prints the cells and commands and exits.
2. The summary builder:

   ```sh
   python3 -B -m tools.task_cost_eval.requested_summary \
     --families tools/task_cost_eval/results/requested-DATE-{astra,sol,opus,deepseek}.json \
     --output tools/task_cost_eval/results/requested-DATE-summary.json [--quality-tolerance 0]
   ```

   It writes the `requested-20261016-summary.json` shape (`pairs.FAMILY.native`,
   `distill` = the Normal arm, and the native-against-Normal deltas) plus
   `pairs.FAMILY.arms.MODE`: totals priced with `comparison_cost`, deltas
   against native and against Normal, and a quality gate (successes at least
   Normal's minus the tolerance, with model, worker and mode activation
   verified). A missing receipt makes the arm cost a lower bound and its
   deltas bounds. It refuses a pricing record that disagrees with
   `comparison_cost` and never overwrites its output.
3. The renderer:

   ```sh
   python3 -B -m tools.task_cost_eval.render_benchmarks \
     --summary tools/task_cost_eval/results/requested-DATE-summary.json \
     --benchmarks BENCHMARKS.md --readme README.md [--check]
   ```

   It rewrites only the text between `<!-- benchmarks:begin -->` and
   `<!-- benchmarks:end -->` in `BENCHMARKS.md` (the native table, and for a
   per-mode summary the Normal note and a "By execution mode" table) and between
   `<!-- benchmarks:MODE:begin -->` and `<!-- benchmarks:MODE:end -->` in each
   `README.md` mode cell (a headline and one sentence). Every number comes from
   the summary; each claim names the family and models, whether the mode was
   pinned or measured under Auto, the result file and the recorded binary
   version. A mode without a result, or failing the gate, reads `no claim yet`
   with the reason. `--check` exits 1 when a file differs from the rendering.
   The committed 2026-10-16 summary renders the current files unchanged; it has
   no per-mode arms, so the README regions stay empty. The hand-written text
   outside the markers (titles, cohort description, cost basis, the current
   README mode claims) is not regenerated: after the first per-mode render,
   remove the hand-written README headline and number sentence under each
   marker pair and update the `BENCHMARKS.md` title and cohort paragraphs.

## Session usage analyzer

`session_usage.py` reads real session folders and prints aggregates only:
counts, token sums, cost-equivalents, roles and model families. It never
prints a session ID, path, prompt or model ID, never writes, and never calls a
model.

```sh
python3 -B -m tools.task_cost_eval.session_usage --days 14 --check-totals
python3 -B -m tools.task_cost_eval.session_usage --report totals --report cache-breaks \
  --exclude-days 2026-01-05:2026-01-10
python3 -B -m tools.task_cost_eval.session_usage --days 7 --report decisions \
  --expect final_check.catch_rate=0.10,mode.flip_flop_per_switch=20%
```

- `--sessions-root` defaults to `~/.distill/sessions` and expects
  `<project>/<session>/usage.json`, with the optional `events.jsonl`, which is
  streamed, and `subagents/*/meta.json`.
- A root session's `usage.json` copies its subagents' rows. Each `attempt_id`
  is counted once, for the deepest session that holds it, and subagent threads
  are reported apart from the root's own calls.
- `--days N` keeps sessions updated in the last N days. `--exclude-days`
  takes `START[:END]` ranges, comma separated or repeated, and drops rows by
  the date their turn ended.
- Cost-equivalents are in units of one uncached input token: cache read 0.05
  (Opus/Sonnet class), 0.025 (Fable class) or 0.1 (default), cache write 1.25
  (5 minutes) or 2 (1 hour), output 5. Change them with `--prices FILE` or
  `--price FAMILY.FIELD=VALUE`. A main call whose cached tokens fall under
  half of the previous prompt on the same thread is a cache miss and is also
  priced on its own.
- Reports (`--report`, repeatable): `totals` (by role, model family and
  thread), `cache-breaks` (break rate and uncached tokens by cause: the
  `cacheBreaks` counters in `usage.json` when present, otherwise heuristics
  for model or effort change, the 5-minute to 1-hour lifetime switch, a gap
  over 5 minutes before a turn, turn start and unknown), `rounds` (main calls
  per turn, and poll-only rounds from the tool events), `turn-start` (serial
  Jev time before the first main call of a turn) and `compaction`
  (compaction-role calls, cached share, and pass-1 against applied when the
  rows label it). `resume` and `mode-switch` cannot be derived from these files
  and are not offered.
- `decisions` joins the Jev decision events of each session's
  `events.jsonl` with what followed. Sessions without a `jev_decision` or
  `jev_no_answer` event are counted as `no_decision_events` and excluded, not
  counted as zeros. Per lever it prints asked (decisions plus no-answers),
  answered, no-answers by kind, memo share, the 12 most common labels and the
  median and p90 latency, next to the `jev:<lever>` and `policy:<mode>`
  counters of `usage.json`. Only short identifiers and 12-hex hashes are
  printed; any other label prints as `other`. Every event and field name it
  reads is in the `NAMES` table at the top of `decision_quality.py`, and the
  label rules are documented in that module's docstring:
  - mode pick: `abandoned` (a switch away within 3 rounds), `held`,
    `flip_flop` (the switch after the pick returns to a mode left earlier in
    the turn), `lean_ended_failing` or `lean_ended_clean` (the turn ended in
    the picked UltraSave or UltraFast with or without a failed check or an
    escalation); flip-flops are also given per mid-turn switch;
  - final check, per reminder: `catch` (the follow-up edited, changed the
    worktree, delivered, or ran tools followed by a recorded check),
    `false_alarm` (fixed token or zero tool rounds), else `unclear`;
  - skill hint `followed` (the same skill read later in the turn) beside the
    base rate of turns that read a skill nobody suggested;
  - C4 review: an edit tool in the next 2 rounds after a note, and after an
    `ok` review as the base rate (shell edits are not visible);
  - D4 and P2 `acted` counts (0 for a dormant lever); effort only
    `escalated_after_low`.
  Coverage prints the labelled share of mode picks, reminders and C4 reviews
  against the 80% target without enforcing it. `--expect KEY=VALUE,...`
  (fractions or `N%`) prints reference rates beside the measured `rates` and
  flags differences over 3 points. The report is tested on synthetic session
  folders only.
- `--check-totals` compares each file's deduped prompt tokens on its primary
  model with that file's `session.modelUsage`, and exits 1 when any session
  is more than 10% off.
- `--simulate compaction` replays the main-call prompt sizes of each thread
  through a policy (`--soft-threshold` applied at turn boundaries,
  `--hard-cap`, `--post-size`, `--regrowth`) and prints tokens and
  cost-equivalents against the recorded baseline. It makes no API call.

## Jev calibration and scorecard

```sh
python3 -B -m tools.task_cost_eval.jev_calibrate [--sessions-root DIR] [--labels FILE] \
    [--report reliability|scorecard ...] [--lever NAME ...] [--model ID] [--question ID]
```

- Reads the `jev_decision` and `jev_no_answer` events of each session's
  `events.jsonl` (and `turn_started` to count turns). It never calls a model
  and prints no session id, path or text; Jev model ids and question hashes
  are printed because a calibration entry needs them.
- `--labels` is a JSON file of outcome labels, per decision
  (`{"decisions": {"<session folder>": {"<seq>": true}}}`) or per session
  (`{"sessions": {"<session folder>": false}}`). True means what the decision
  predicted or acted on was real.
- `reliability` (default): per lever, probability bins against the observed
  rate. A threshold is proposed, with its fit window, n and question hashes,
  only when the rarer outcome has at least 30 events; otherwise the lever is
  `REFUSED` with the counts. A proposal changes nothing until it passes
  replay, shadow and a paired benchmark ([Jev routing](jev-routing.md)).
- `scorecard`: per lever asked, answered, applied, labelled precision of
  applied decisions with a 95% interval, milliseconds awaited per turn, and
  `DORMANT` when under 1% of at least 500 answers were applied. Until the
  events carry an `applied` field, applied means any label other than the
  deferral labels (`defer`, `keep`, `complete`, `review:ok`, …); `shadow:`
  decisions are reported apart and never count as applied.
