# Benchmarks — 2026-11-12 requested cohorts by execution mode

These three matched cohorts compare native maximum-effort baselines with Distill, one arm per execution mode (Normal, Auto, UltraSave, UltraFast and UltraCode), each configured with automatic effort for its main and worker tiers. Each arm ran three frozen tasks three times, for nine runs per arm, 54 per family and 162 executions in total. Runs used an identical 300-second deadline. Arm order alternated within each task/repetition pair; families overlapped in time. Inside this campaign no cell was rerun and none was dropped; a failed or timed-out run would remain included, and there was none. The campaign as a whole is a second run: see [How this campaign came about](#how-this-campaign-came-about).

Quality means successful completion within 300 seconds, successful agent exit, unchanged frozen inputs, and a passing unchanged external behavioral grader. Functional-only grader passes are reported separately where they differ. These small fixtures had no human review and cannot establish universal quality equivalence.

<!-- benchmarks:begin -->
| Pair | Native baseline | Distill main + worker | Native estimated cost | Distill estimated cost | Δ cost | Native quality | Distill quality | Δ quality | Native time | Distill time | Δ time |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Astra | Codex GPT-6 Astra max | GPT-6 Astra auto + GPT-6 Luna auto | $3.07861 | $1.18301 | −61.6% | 9/9 | 9/9 | +0.0% | 457.60 s | 435.50 s | −4.8% |
| Sol | Codex GPT-6.1 Sol max | GPT-6.1 Sol auto + GPT-6 Luna auto | $0.56277 | $0.20742 | −63.1% | 9/9 | 9/9 | +0.0% | 479.28 s | 414.16 s | −13.6% |
| Opus | Claude Code Opus 5.5 max | Opus 5.5 auto + Haiku 5.5 auto | $2.58112 | $0.31752 | −87.7% | 9/9 | 9/9 | +0.0% | 768.49 s | 129.14 s | −83.2% |

The Distill columns are the Normal arm with the mode pinned; versions and result files are in the table below.

Deltas use `100 * (Distill / native - 1)`. Negative cost and time deltas favor Distill. Quality delta is relative; percentage-point differences are shown separately. There is no measured baseline variation, so baseline variation cells are not applicable.

## By execution mode

One Distill arm per execution mode ran beside the native arm in each family: 3 frozen tasks, 3 repetitions, a 300-second deadline, 162 executions in all ([summary](tools/task_cost_eval/results/requested-20261112-summary.json), 2026-11-12). Every mode was pinned for the whole run except Auto, measured under Auto (Jev picks the mode per request). Deltas use `100 * (arm / baseline - 1)` against the native arm and against the Distill Normal arm of the same family. A mode carries a claim only when its model, worker and mode activation were verified and its successes are at least the Normal arm's minus 0; otherwise its row reads `no claim yet` with the reason.

| Family (models) | Mode | Measured | Estimated cost | Δ cost vs native | Δ cost vs Normal | Quality | Δ quality vs native | Δ quality vs Normal | Time | Δ time vs native | Δ time vs Normal | Source |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| Astra (GPT-6 Astra + GPT-6 Luna) | Normal | mode pinned | $1.18301 | −61.6% | — | 9/9 | +0.0% | — | 435.50 s | −4.8% | — | [Astra result](tools/task_cost_eval/results/requested-20261112-astra.json), distill 2.0.9 (39f1c6fe0bf8) |
| Astra (GPT-6 Astra + GPT-6 Luna) | Auto | under Auto | $0.58074 | −81.1% | −50.9% | 9/9 | +0.0% | +0.0% | 362.86 s | −20.7% | −16.7% | [Astra result](tools/task_cost_eval/results/requested-20261112-astra.json), distill 2.0.9 (39f1c6fe0bf8) |
| Astra (GPT-6 Astra + GPT-6 Luna) | UltraSave | mode pinned | $0.01241 | −99.6% | −99.0% | 9/9 | +0.0% | +0.0% | 215.77 s | −52.8% | −50.5% | [Astra result](tools/task_cost_eval/results/requested-20261112-astra.json), distill 2.0.9 (39f1c6fe0bf8) |
| Astra (GPT-6 Astra + GPT-6 Luna) | UltraFast | mode pinned | $0.94870 | −69.2% | −19.8% | 9/9 | +0.0% | +0.0% | 196.92 s | −57.0% | −54.8% | [Astra result](tools/task_cost_eval/results/requested-20261112-astra.json), distill 2.0.9 (39f1c6fe0bf8) |
| Astra (GPT-6 Astra + GPT-6 Luna) | UltraCode | mode pinned | $2.58553 | −16.0% | +118.6% | 9/9 | +0.0% | +0.0% | 651.78 s | +42.4% | +49.7% | [Astra result](tools/task_cost_eval/results/requested-20261112-astra.json), distill 2.0.9 (39f1c6fe0bf8) |
| Sol (GPT-6.1 Sol + GPT-6 Luna) | Normal | mode pinned | $0.20742 | −63.1% | — | 9/9 | +0.0% | — | 414.16 s | −13.6% | — | [Sol result](tools/task_cost_eval/results/requested-20261112-sol.json), distill 2.0.9 (39f1c6fe0bf8) |
| Sol (GPT-6.1 Sol + GPT-6 Luna) | Auto | under Auto | $0.10060 | −82.1% | −51.5% | 9/9 | +0.0% | +0.0% | 376.65 s | −21.4% | −9.1% | [Sol result](tools/task_cost_eval/results/requested-20261112-sol.json), distill 2.0.9 (39f1c6fe0bf8) |
| Sol (GPT-6.1 Sol + GPT-6 Luna) | UltraSave | mode pinned | $0.01184 | −97.9% | −94.3% | 9/9 | +0.0% | +0.0% | 158.06 s | −67.0% | −61.8% | [Sol result](tools/task_cost_eval/results/requested-20261112-sol.json), distill 2.0.9 (39f1c6fe0bf8) |
| Sol (GPT-6.1 Sol + GPT-6 Luna) | UltraFast | mode pinned | $0.15814 | −71.9% | −23.8% | 9/9 | +0.0% | +0.0% | 170.12 s | −64.5% | −58.9% | [Sol result](tools/task_cost_eval/results/requested-20261112-sol.json), distill 2.0.9 (39f1c6fe0bf8) |
| Sol (GPT-6.1 Sol + GPT-6 Luna) | UltraCode | mode pinned | $0.56270 | −0.0% | +171.3% | 9/9 | +0.0% | +0.0% | 664.82 s | +38.7% | +60.5% | [Sol result](tools/task_cost_eval/results/requested-20261112-sol.json), distill 2.0.9 (39f1c6fe0bf8) |
| Opus (Opus 5.5 + Haiku 5.5) | Normal | mode pinned | $0.31752 | −87.7% | — | 9/9 | +0.0% | — | 129.14 s | −83.2% | — | [Opus result](tools/task_cost_eval/results/requested-20261112-opus.json), distill 2.0.9 (39f1c6fe0bf8) |
| Opus (Opus 5.5 + Haiku 5.5) | Auto | under Auto | $0.13132 | −94.9% | −58.6% | 9/9 | +0.0% | +0.0% | 152.80 s | −80.1% | +18.3% | [Opus result](tools/task_cost_eval/results/requested-20261112-opus.json), distill 2.0.9 (39f1c6fe0bf8) |
| Opus (Opus 5.5 + Haiku 5.5) | UltraSave | mode pinned | $0.01544 | −99.4% | −95.1% | 9/9 | +0.0% | +0.0% | 164.24 s | −78.6% | +27.2% | [Opus result](tools/task_cost_eval/results/requested-20261112-opus.json), distill 2.0.9 (39f1c6fe0bf8) |
| Opus (Opus 5.5 + Haiku 5.5) | UltraFast | mode pinned | $0.32759 | −87.3% | +3.2% | 9/9 | +0.0% | +0.0% | 105.09 s | −86.3% | −18.6% | [Opus result](tools/task_cost_eval/results/requested-20261112-opus.json), distill 2.0.9 (39f1c6fe0bf8) |
| Opus (Opus 5.5 + Haiku 5.5) | UltraCode | mode pinned | $1.00580 | −61.0% | +216.8% | 9/9 | +0.0% | +0.0% | 260.32 s | −66.1% | +101.6% | [Opus result](tools/task_cost_eval/results/requested-20261112-opus.json), distill 2.0.9 (39f1c6fe0bf8) |
<!-- benchmarks:end -->

## Cohort interpretation

All Distill runs enabled external Jev. Distill cost includes Jev calls and child calls. Every mode except Auto was pinned for the whole run; Auto picked a mode per request.

What each arm did, read from the per-call evidence in the family files:

- **Normal** hands the edit to a subagent on the worker model in the GPT families, which is most of its wall time there; in the Opus family it works without one.
- **UltraSave** leads on the worker model (GPT-6 Luna or Haiku 5.5) for the whole session. That is where its cost goes down. In the Opus family it is slower than Normal, because Haiku 5.5 took longer per call than Opus 5.5 on these tasks.
- **UltraFast** keeps the session's model, runs no subagent, waits for no side decision and caps effort at medium. It takes 41% to 81% of Normal's time at 76% to 103% of its cost. In the GPT families its time and UltraSave's are within the run-to-run variation below; in the Opus family it is the fastest arm.
- **UltraCode** adds an independent review after a turn that changed code. Every arm passes these tasks, so the table shows what that review costs and cannot show what it catches. No quality claim is made for it.
- **Auto** carries the cost of its mode decisions on short tasks: it is cheaper than Normal in all three families and slower than Normal in the Opus family.

The DeepSeek family is not in these tables. Its cells ran, but the account's daily quota for OpenRouter's free models, which that family uses for its worker and auxiliary calls, was exhausted during the day: every free-model call failed, the UltraSave arm, which leads on the free router, failed all nine runs, and no Distill arm has complete cost accounting. The result is retained as [the excluded DeepSeek record](tools/task_cost_eval/results/requested-20261112-deepseek-quota-exhausted.json) and is not summarized. The last complete DeepSeek cohort is the 2026-10-16 one ([result](tools/task_cost_eval/results/requested-20261016-deepseek.json), Distill 2.0.46, one Distill arm).

The Distill Normal arm is slower in the GPT families than the Distill arm of the 2026-10-16 campaign on Distill 2.0.46 ([summary](tools/task_cost_eval/results/requested-20261016-summary.json)). A separate diagnostic run of the released 2.0.68 binary in Normal mode on the Sol family, two repetitions, took 281.28 s for six runs against 304.96 s for native ([diagnostic record](tools/task_cost_eval/results/requested-20261112-sol-2.0.68-normal.json)), about the same time per run as the Normal arm above (46.9 s against 46.0 s). It is not part of the tables and was not paired with this campaign's cells.

## Run-to-run variation

This format has no repeated identical arm, so the table alone does not say how much two runs of the same arm differ. The first run of this campaign (see below) gives an estimate: the native, Normal, UltraSave and UltraCode arms ran the same code path in both runs ([first-run summary](tools/task_cost_eval/results/requested-20261112-first-run-summary.json)). Between the two runs those arms changed by:

| Arm | Time | Estimated cost |
|---|---|---|
| Native | −3.5% to −2.6% | −6.0% to −3.6% |
| Normal | −0.8% to +3.0% | −27.9% to +0.8% |
| UltraSave | −9.6% to +39.1% | −11.5% to +12.7% |
| UltraCode | −7.0% to +5.9% | −17.8% to +1.9% |

Read the tables with that in mind. A cost difference under about 30% or a time difference under about 10% between two Distill arms of the GPT families may be variation (and one UltraSave time moved 39%). UltraSave's cost against Normal, UltraFast's time against Normal in the GPT families and UltraCode's cost and time against Normal are well outside it. Auto's time against Normal, and the order of UltraFast and UltraSave on time in the GPT families, are not.

## How this campaign came about

A first full campaign ran earlier the same day on commit `7f909d8f`. It was not published, for three reasons found in it:

- UltraFast was slower than Normal in the Opus family: tier routing sent its rounds to the worker model as the faster one, and it was not. UltraFast now keeps the session's model.
- The DeepSeek Distill arm used a provider alias that had moved to a newer snapshot than the native arm's model, so the pair was not matched. Both arms now name the same model.
- Its result files recorded a local working path. This campaign writes under `/tmp`.

Pilots before it also changed the product: UltraFast stopped delegating to a single subagent, Auto's criteria send a small fix with a stated result to a lean mode, the ChatGPT subscription endpoint is no longer sent the economy serving path it refuses, and a Claude subscription is no longer asked for fast mode. After those changes the whole campaign was run again on commit `39f1c6fe`, and only that run is in the tables.

## Tasks and runtime

- `tax-bug-en`: integer-cent tax totals, including zero rates and fractional-tax rounding.
- `active-records-pt`: strict Boolean filtering, stable order, a new result list, and no input mutation.
- `inventory-report-en`: sum duplicate stock records, sort the rendered report, and preserve the input.

The frozen checks measure these behaviors, not general model capability or code maintainability. Distill was a source build of commit `39f1c6fe`; it reports `distill 2.0.9 (39f1c6fe0bf8)`, because a source build carries the binary crate's version and only the release pipeline stamps a release number. Codex CLI was `0.167.2` and Claude Code `2.1.322`. New repositories and isolated profiles prevented inherited project instructions or MCP integrations from changing the task setup. Cross-family overlap and provider load can affect latency.

## Cost basis

All values are standardized token-reference estimates, not invoices. GPT subscription credits use `$0.04` per reference credit, not subscription charges. Claude figures are API-equivalent list-price estimates using full prompt cost and subtracting known cache reads and 5-minute/1-hour cache writes; native Claude Code does not expose the cache-write lifetime split, so its reported `costBasis=list` / `costUSD` totals supply the estimate. Assuming every native write lasts only five minutes would undercount its reported list cost. Distill uses the explicit cache-write lifetime fields in its token receipts. DeepSeek uses the same fixed public OpenRouter catalog rates for both arms: `$0.036` input, `$0.0054` cache-read, and `$0.6435` output per million tokens. Native Codex's actual OpenRouter invoice cost is unavailable. Jev cost uses reported USD receipts. These figures do not compare monthly subscription charges and should not be described as pure API savings.

## Reproduction and evidence

The family JSON retains per-run evidence and commands. Totals and definitions are in [the requested cohort summary](tools/task_cost_eval/results/requested-20261112-summary.json); pricing inputs are in [the pricing record](tools/task_cost_eval/results/requested-20261016-pricing.json). Per-family receipts: [Astra](tools/task_cost_eval/results/requested-20261112-astra.json), [Sol](tools/task_cost_eval/results/requested-20261112-sol.json) and [Opus](tools/task_cost_eval/results/requested-20261112-opus.json). The 2026-10-16 campaign's files remain beside them.

Native GPT commands used `--variant codex --main-model gpt-6-astra` or `gpt-6.1-sol` with `--codex-effort max`. Their Distill counterparts used `--variant distill --main-model` with the corresponding model, `--worker-model gpt-6-luna`, `--enable-jev` and `--mode`. Opus used `--variant opus-native` and `--variant opus-distill`. Exact invocation arguments, frozen cases, run order, hashes, and receipts are retained in the linked family evidence. Credential contents were not copied into the evidence records, and credential-file paths are redacted.

The runner checks that each arm ran on the models its mode names: Normal and UltraCode root rounds on the main model, UltraSave, UltraFast and Auto root rounds on the main or the worker model, children on the worker model, and UltraCode's and Auto's reviewer on the main model. Any other model fails the run.

Representative commands (use a fresh output directory for every cell):

```sh
python3 -m tools.task_cost_eval.compare_codex --variant codex --main-model gpt-6-astra --codex-effort max --case tax-bug-en --output /tmp/astra-native-1
python3 -m tools.task_cost_eval.compare_codex --variant distill --main-model gpt-6-astra --worker-model gpt-6-luna --enable-jev --distill "$(command -v distill)" --case tax-bug-en --output /tmp/astra-distill-1
# Replace gpt-6-astra with gpt-6.1-sol for the Sol pair.
python3 -m tools.task_cost_eval.compare_additional --variant opus-native --claude-auth "$CLAUDE_AUTH_FILE" --case tax-bug-en --output /tmp/opus-native-1
python3 -m tools.task_cost_eval.compare_additional --variant opus-distill --enable-jev --claude-auth "$CLAUDE_AUTH_FILE" --case tax-bug-en --output /tmp/opus-distill-1
python3 -m tools.task_cost_eval.compare_codex --variant codex --provider openrouter --main-model deepseek/deepseek-v4.1-flash --codex-effort max --case tax-bug-en --output /tmp/deepseek-native-1
python3 -m tools.task_cost_eval.compare_additional --variant deepseek-free-distill --enable-jev --case tax-bug-en --output /tmp/deepseek-distill-1
# Per-mode cohorts: a Distill arm takes --mode normal|auto|ultrasave|ultrafast|ultracode. Distill 2.0.46 had no
# --mode and ran Normal; a current binary without the flag starts in Auto, so pin Normal with --mode normal.
python3 -B -m tools.task_cost_eval.compare_codex --variant distill --main-model gpt-6-astra --worker-model gpt-6-luna --enable-jev --mode ultrasave --distill "$(command -v distill)" --case tax-bug-en --output /tmp/astra-ultrasave-1
python3 -B -m tools.task_cost_eval.compare_additional --variant opus-distill --enable-jev --mode auto --distill "$(command -v distill)" --claude-auth "$CLAUDE_AUTH_FILE" --case tax-bug-en --output /tmp/opus-auto-1
# The cohort driver plans every cell of a family (native plus one arm per mode, 3 tasks x 3 repetitions,
# alternating arm order); --execute runs each cell once and writes results/requested-DATE-FAMILY.json.
python3 -B -m tools.task_cost_eval.requested_cohort --families astra,sol,opus,deepseek --distill "$(command -v distill)" --claude-auth "$CLAUDE_AUTH_FILE"
python3 -B -m tools.task_cost_eval.requested_cohort --families astra,sol,opus,deepseek --distill "$(command -v distill)" --claude-auth "$CLAUDE_AUTH_FILE" --execute
python3 -B -m tools.task_cost_eval.requested_summary --families tools/task_cost_eval/results/requested-DATE-{astra,sol,opus,deepseek}.json --output tools/task_cost_eval/results/requested-DATE-summary.json
python3 -B -m tools.task_cost_eval.render_benchmarks --summary tools/task_cost_eval/results/requested-DATE-summary.json --benchmarks BENCHMARKS.md --readme README.md
```

Use the same authenticated account for both sides of each provider pair. `CLAUDE_AUTH_FILE` identifies the existing private Claude credential file, never its contents. GPT reads existing Codex OAuth credentials; OpenRouter reads the private Distill auth store. Each measured task ran every arm before advancing to the next task, rotating the starting arm.

Run the regression checks with:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest tools.task_cost_eval.test_requested_benchmarks tools.task_cost_eval.test_compare_codex tools.task_cost_eval.test_compare_additional tools.task_cost_eval.test_acp_driver
PYTHONDONTWRITEBYTECODE=1 python3 -B -m unittest tools.task_cost_eval.test_requested_cohort tools.task_cost_eval.test_requested_summary tools.task_cost_eval.test_render_benchmarks
python3 -B -m tools.task_cost_eval.render_benchmarks --summary tools/task_cost_eval/results/requested-20261112-summary.json --benchmarks BENCHMARKS.md --readme README.md --check
```
