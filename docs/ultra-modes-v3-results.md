# UltraSave and UltraFast v3: Jev picks among the user's models

This page records the 20-case paired campaigns for the third design of the two
Ultra execution modes, run on 2026-10-22. All four final campaigns pass their
predeclared gates. The sample is bounded: one repetition of each of 20 cases.
It is an observation, not a statistical claim, and the earlier rounds below show
that the outcome depends strongly on routing details and can vary between runs.

Machine-readable results (scrubbed by the runner; no API keys or bearer tokens):

- [save-vs-normal](../tools/task_cost_eval/results/ultra-v3-20261022-save-vs-normal.json)
- [fast-vs-normal](../tools/task_cost_eval/results/ultra-v3-20261022-fast-vs-normal.json)
- [save-vs-2049](../tools/task_cost_eval/results/ultra-v3-20261022-save-vs-2049.json)
- [fast-vs-2049](../tools/task_cost_eval/results/ultra-v3-20261022-fast-vs-2049.json)

## What changed

- **Per-turn tier choice.** In UltraSave and UltraFast, Jev picks a model tier
  for each turn from the three models the user already configured: the main
  (session) model, the worker model and the utility model. It never selects a
  model outside that set.
- **UltraSave** picks the cheapest of the three that can do the step well, then
  minimizes tokens.
- **UltraFast** picks the fastest capable model, regains `spawn_subagent`, and
  carries a directive to split independent work into parallel subtasks.
- **Directives.** Both modes now ask the model to test the edge cases stated in
  the task before finishing.
- **Authority.** Account Auto routing stays unpinned, and explicit user model
  pins and explicit effort settings keep authority over the Ultra tier choice.
- **Failure handling.** If a routed round fails, the turn falls back to the
  session model.
- **Skills.** The skill catalog is capped at 4 descriptors in both modes.

## Setup

| Item | Value |
|---|---|
| Main model | `chatgpt/gpt-6.1-sol` |
| Worker and utility model | `chatgpt/gpt-6-luna` |
| Jev | `typesafe/jev-1.14`, enabled (`--enable-jev`) |
| Transport | ACP |
| Capacity | 8 concurrent runs, 8 subagents |
| Cohort | 20 cases, `tools/task_cost_eval/cohort-ultrafast-v2.json` |
| Order seed | 20261021 |
| Repetitions | 1 |
| Timeout | 900 s per run |
| Baseline binary (`*-vs-2049`) | 2.0.49, sha256 `69864afa0b5c...`, same Ultra mode as the candidate |
| Baseline binary (`*-vs-normal`) | the candidate binary, Normal mode |
| Candidate binary | 2.0.50 build of commit `69d45049`, sha256 `68d9bdf65b1e...` |

Every cell is a fresh execution graded by the unchanged external graders in
`tools/task_cost_eval/graders/`. Credits use the published subscription credit
rates over LLM calls only. Jev calls are billed in USD and reported separately
as `jev_usd`; they are never converted into credits (`credit_basis` in the
JSON).

## Gates

Defined in `tools/task_cost_eval/ultrasave_verdict.py` and
`tools/task_cost_eval/ultrafast_verdict.py`. A failed candidate run cannot earn
savings: its ratio is clamped to at least 1.0 (UltraFast also charges a failed
run the full timeout).

| Verdict | Metric (median over paired cases) | vs Normal | vs 2.0.49 (`--compare-versions`) |
|---|---|---|---|
| UltraSave | candidate / baseline credits | <= 0.45 | <= 0.85 |
| UltraFast | candidate / baseline completion time (run + grading) | <= 0.60 | <= 0.85 |

Both also require at most 1 quality regression (baseline passed, candidate
failed) and at least 19 of 20 candidate runs accepted. Token ratios are reported
for UltraSave but are not the gate.

## Results

Credits, wall time and Jev USD are sums over the 20 runs per variant. Wall time
is the sum of per-run times, not campaign elapsed time.

| Campaign | Verdict | Accepted (base / cand) | Credits (base -> cand) | Median paired ratio (gate) | Token ratio (median) | Regressions | Wall time (base -> cand) | Jev USD (base -> cand) |
|---|---|---|---|---|---|---|---|---|
| UltraSave vs Normal | PASS | 20 / 20 | 26.34 -> 1.54 | credits 0.069 (<= 0.45) | 0.497 | 0 | 2072 s -> 612 s | 0.0330 -> 0.0073 |
| UltraFast vs Normal | PASS | 20 / 20 | 25.26 -> 2.33 | completion 0.509 (<= 0.60) | n/a | 0 | 2052 s -> 879 s | 0.0349 -> 0.0094 |
| UltraSave vs 2.0.49 UltraSave | PASS | 20 / 19 | 24.19 -> 1.51 | credits 0.067 (<= 0.85) | 1.232 | 1 (`bench-cursor`) | 1205 s -> 610 s | 0.0047 -> 0.0070 |
| UltraFast vs 2.0.49 UltraFast | PASS | 20 / 19 | 25.09 -> 2.50 | completion 0.704 (<= 0.85) | n/a | 1 (`bench-duration`) | 1200 s -> 920 s | 0.0054 -> 0.0100 |

Notes on reading the table:

- In the vs-2.0.49 UltraSave campaign the candidate uses about 23% more tokens
  at the median (ratio 1.232) but far fewer credits, because most of its calls
  go to the cheaper worker/utility model. Credits, not tokens, are the money
  gate.
- In the vs-2.0.49 UltraFast campaign, the failed `bench-duration` run is
  charged the full 900 s timeout by the gate, which raises the candidate's
  charged completion total to 1748 s against 1202 s for the baseline; the
  median ratio is still 0.704.
- The candidate's model calls are almost entirely `gpt-6-luna` (low and medium
  effort) plus Jev decision calls; `gpt-6.1-sol` was not called by either final
  candidate in any of the four campaigns.
- Jev USD for the candidate is higher than for the 2.0.49 baseline in the
  vs-2.0.49 pairs, because the new build asks Jev for a tier choice. The
  amounts are fractions of a cent per campaign.
- UltraFast observed subagent attempts: 23 (peak 1 live) vs Normal's 37 in the
  vs-Normal pairing, and 25 (peak 2 live) vs 0 for 2.0.49 UltraFast.

## Iterations

Three earlier rounds ran the same 20-case cohort and setup before the final
design. They are kept for honesty about how fragile the result is. "Gate ratio"
is the verdict's median paired ratio (failed candidate runs clamped to 1.0);
the raw column is the unclamped median of per-case candidate/baseline credits
and of per-case completion time (run + grading). Failing cases are candidate failures unless noted as
baseline.

**Round 1 - commit `05187902`: sticky route, cheap default.** Rounds are
routed among the three models, the tier is kept for the turn, and the default is
the cheaper/faster model.

| Campaign | Verdict | Accepted (base / cand) | Gate ratio | Raw median credit / time ratio | Failing cases |
|---|---|---|---|---|---|
| UltraSave vs Normal | PASS | 20 / 19 | credits 0.035 | 0.035 / 0.269 | `bench-duration` |
| UltraFast vs Normal | PASS | 20 / 20 | completion 0.390 | 0.070 / 0.390 | none |
| UltraSave vs 2.0.49 | FAIL | 20 / 16 | credits 0.068 | 0.061 / 0.582 | `bench-bytes`, `bench-clock`, `bench-template`, `bench-tokenbucket` (4 regressions) |
| UltraFast vs 2.0.49 | INCOMPLETE | 20 / 18 | no gate value (invalid) | 0.090 / 0.884 (19 valid pairs) | `bench-csv`, `bench-cursor`; `bench-backoff` had incomplete candidate accounting |

Round 1 saved the most, but it had 4 UltraSave regressions against 2.0.49 and
one UltraFast campaign was invalid (incomplete accounting), so it was not
accepted. The final design has 1 regression in each vs-2.0.49 campaign.

**Round 2 - commit `5fe1c321`: per-round asking, main model for
implementation.** Jev was asked for a tier every round and implementation steps
went to the main model.

| Campaign | Verdict | Accepted (base / cand) | Gate ratio | Raw median credit / time ratio | Failing cases |
|---|---|---|---|---|---|
| UltraSave vs Normal | FAIL | 19 / 20 | credits 1.003 | 0.980 / 0.736 | baseline: `bench-intervals` |
| UltraFast vs Normal | FAIL | 20 / 20 | completion 0.753 | 0.656 / 0.753 | none |
| UltraSave vs 2.0.49 | FAIL | 20 / 20 | credits 1.167 | 1.167 / 1.246 | none |
| UltraFast vs 2.0.49 | FAIL | 20 / 19 | completion 1.576 | 0.928 / 1.576 | `bench-csv` |

Quality was fine, but sending implementation to the main model made the savings
vanish: UltraSave cost about as much as Normal, and both modes were slower and
costlier than 2.0.49.

**Round 3 - commit `89960800`: sticky route, main model for implementation.**

| Campaign | Verdict | Accepted (base / cand) | Gate ratio | Raw median credit / time ratio | Failing cases |
|---|---|---|---|---|---|
| UltraSave vs Normal | FAIL | 19 / 20 | credits 1.058 | 1.058 / 0.792 | baseline: `bench-csv` |
| UltraFast vs Normal | FAIL | 20 / 20 | completion 0.888 | 0.827 / 0.888 | none |
| UltraSave vs 2.0.49 | FAIL | 20 / 20 | credits 1.189 | 1.189 / 1.466 | none |
| UltraFast vs 2.0.49 | FAIL | 20 / 20 | completion 1.298 | 0.785 / 1.298 | none |

Same outcome as round 2 with fewer Jev calls: no regressions, no savings.

**Final - commit `69d45049`: sticky route, cheaper or faster model by default.**
All four campaigns PASS (table above), with one regression in each vs-2.0.49
campaign.

Quality varies between runs. Round 1 had four UltraSave regressions against
2.0.49 and the final build has one. `bench-cursor` and `bench-csv` each failed in
two different rounds, so the failures are not confined to one design. With
n = 20 and one repetition, the difference between four and one regressions
cannot be attributed to the design changes rather than run-to-run variance.

## Reproduce

From the repository root, with `/tmp/ultra3/bin/distill` the candidate build and
`/tmp/ultra3/bin/distill-2.0.49` the 2.0.49 binary, and `~/.distill` holding the
chatgpt and OpenRouter credentials:

```bash
# usage: run.sh <name> <baseline-binary> <baseline-mode> <candidate-mode> <verdict> [extra...]
name=$1; bb=$2; bm=$3; cm=$4; v=$5; shift 5
python3 -B -m tools.task_cost_eval.compare_parallel \
  --baseline-binary $bb --candidate-binary /tmp/ultra3/bin/distill \
  --baseline-mode $bm --candidate-mode $cm --transport acp --verdict $v --max-concurrent 8 \
  --model chatgpt/gpt-6.1-sol --worker-model chatgpt/gpt-6-luna --utility-model chatgpt/gpt-6-luna --enable-jev \
  --cohort tools/task_cost_eval/cohort-ultrafast-v2.json --order-seed 20261021 --repetitions 1 --timeout 900 \
  --distill-profile ~/.distill --work-root /tmp/ultra3/runs/$name --output /tmp/ultra3/$name.json --execute "$@"
```

The four campaigns:

```bash
run.sh save-vs-normal /tmp/ultra3/bin/distill        normal    ultrasave ultrasave
run.sh fast-vs-normal /tmp/ultra3/bin/distill        normal    ultrafast ultrafast
run.sh save-vs-2049   /tmp/ultra3/bin/distill-2.0.49 ultrasave ultrasave ultrasave --compare-versions
run.sh fast-vs-2049   /tmp/ultra3/bin/distill-2.0.49 ultrafast ultrafast ultrafast --compare-versions
```

## Known limits

- One repetition per case; results are a bounded sample, not a statistical
  claim.
- The UltraFast speedup comes mostly from routing to the faster worker model,
  not from measured parallelism (peak live subagents was 1 to 2).
- List prices from the OpenRouter catalog are used only to rank the three
  models, not to compute the reported credits.
- The tier choice is sticky for a turn and is re-decided only after a failed
  tool call.
