# Benchmarks — 2026-10-16 requested cohorts

These four matched cohorts compare native maximum-effort baselines with Distill configured with automatic effort for its main and worker tiers. Each arm ran three frozen tasks three times, for nine runs per arm and 72 executions total. Runs used an identical 300-second deadline. Arm order alternated within each task/repetition pair; families may have overlapped in time. No outcome-based reruns or cherry-picking were used. Failed and timed-out runs remain included.

Quality means successful completion within 300 seconds, successful agent exit, unchanged frozen inputs, and a passing unchanged external behavioral grader. Functional-only grader passes are reported separately where they differ. These small fixtures had no human review and cannot establish universal quality equivalence.

<!-- benchmarks:begin -->
| Pair | Native baseline | Distill main + worker | Native estimated cost | Distill estimated cost | Δ cost | Native quality | Distill quality | Δ quality | Native time | Distill time | Δ time |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Astra | Codex GPT-6 Astra max | GPT-6 Astra auto + GPT-6 Luna auto | $4.07658 | $1.00374 | −75.4% | 9/9 | 9/9 | +0.0% | 440.29 s | 284.99 s | −35.3% |
| Sol | Codex GPT-6.1 Sol max | GPT-6.1 Sol auto + GPT-6 Luna auto | $0.79826 | $0.24981 | −68.7% | 9/9 | 9/9 | +0.0% | 384.09 s | 296.60 s | −22.8% |
| Opus | Claude Code Opus 5.5 max | Opus 5.5 auto + Haiku 5.5 auto | $2.49456 | $0.34875 | −86.0% | 9/9 | 9/9 | +0.0% | 798.16 s | 191.02 s | −76.1% |
| DeepSeek | Codex DeepSeek V4.1 Flash max via OpenRouter Responses | DeepSeek auto + free models (OpenRouter free router) auto | ≥$0.09785 | $0.01177 | ≤−87.9%* | 5/9 | 9/9 | +80.0% | 1973.99 s | 490.27 s | −75.2% |

Deltas use `100 * (Distill / native - 1)`. Negative cost and time deltas favor Distill. Quality delta is relative; percentage-point differences are shown separately. There is no measured baseline variation, so baseline variation cells are not applicable.
<!-- benchmarks:end -->

## Cohort interpretation

All Distill runs enabled external Jev. Distill cost includes Jev calls and child calls; configured worker utilization was not forced, and a configured worker was not necessarily used on every task. The DeepSeek Distill runs used the OpenRouter free router for free-model calls, with zero cost only where receipts reported zero.

Native Codex emitted a missing-model-metadata warning in all nine DeepSeek runs and used its fallback model metadata. This custom-provider integration was not an officially recognized Codex model profile; its warning and integration behavior can affect results.

The DeepSeek native family has three timed-out in-flight calls without usage receipts. Native full cost is unknown, not zero and not a point estimate. The `$0.0978507369` lower bound includes every priceable call across all nine native executions; 109 completed calls had priced receipts. The three unavailable timeout receipts remain unknown. The cost-delta display `≤−87.9%` is an upper bound on the signed delta, conservatively rounded toward zero from −87.9748608209%; actual savings may be greater. DeepSeek native functional graders passed 8/9, including three timed-out task trees, but successful bounded completion was only 5/9. Thus table quality is 5/9, not 8/9; the improvement to 9/9 is 44.4 percentage points (+80.0% relative).

## Tasks and runtime

- `tax-bug-en`: integer-cent tax totals, including zero rates and fractional-tax rounding.
- `active-records-pt`: strict Boolean filtering, stable order, a new result list, and no input mutation.
- `inventory-report-en`: sum duplicate stock records, sort the rendered report, and preserve the input.

The frozen checks measure these behaviors, not general model capability or code maintainability. Distill was `2.0.46`, Codex CLI `0.162.1`, and Claude Code `2.1.300`. New repositories and isolated profiles prevented inherited project instructions or MCP integrations from changing the task setup. Cross-family overlap and provider load can affect latency.

## Cost basis

All values are standardized token-reference estimates, not invoices. GPT subscription credits use `$0.04` per reference credit, not subscription charges. Claude figures are API-equivalent list-price estimates using full prompt cost and subtracting known cache reads and 5-minute/1-hour cache writes; native Claude Code does not expose the cache-write lifetime split, so its reported `costBasis=list` / `costUSD` totals supply the estimate. Assuming every native write lasts only five minutes would undercount its reported list cost. Distill uses the explicit cache-write lifetime fields in its token receipts. DeepSeek uses the same fixed public OpenRouter catalog rates for both arms: `$0.036` input, `$0.0054` cache-read, and `$0.6435` output per million tokens. Native Codex's actual OpenRouter invoice cost is unavailable. Jev cost uses reported USD receipts. These figures do not compare monthly subscription charges and should not be described as pure API savings.

## Reproduction and evidence

The accepted family JSON retains per-run evidence and commands. Canonical totals and definitions are in [the requested cohort summary](tools/task_cost_eval/results/requested-20261016-summary.json); pricing inputs are in [the pricing record](tools/task_cost_eval/results/requested-20261016-pricing.json). Per-family receipts: [Astra](tools/task_cost_eval/results/requested-20261016-astra.json), [Sol](tools/task_cost_eval/results/requested-20261016-sol.json), [Opus](tools/task_cost_eval/results/requested-20261016-opus.json), and [DeepSeek](tools/task_cost_eval/results/requested-20261016-deepseek.json). Historical raw JSON remains on disk but is not included as a current comparison here.

Native GPT commands used `--variant codex --main-model gpt-6-astra` or `gpt-6.1-sol` with `--codex-effort max`. Their Distill counterparts used `--variant distill --main-model` with the corresponding model, `--worker-model gpt-6-luna`, and `--enable-jev`. Opus used `--variant opus-native` and `--variant opus-distill`. DeepSeek native used the Codex runner with `--variant codex --provider openrouter --main-model deepseek/deepseek-v4.1-flash --codex-effort max`; DeepSeek Distill used `--variant deepseek-free-distill`. Distill arms enabled Jev. Exact invocation arguments, frozen cases, run order, hashes, and receipts are retained in the linked family evidence. Credential contents were not copied into the evidence records. Private account identifiers in credential-file paths are redacted; model and effort arguments remain unchanged.


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

Use the same authenticated account for both sides of each provider pair. `CLAUDE_AUTH_FILE` identifies the existing private Claude credential file, never its contents. GPT reads existing Codex OAuth credentials; OpenRouter reads the private Distill auth store. Each measured pair used both arms before advancing to the next task, reversing the starting arm on alternating pairs.

Setup probes are retained separately in [the pilot record](tools/task_cost_eval/results/requested-20261016-pilots.json) and excluded from headline cohort totals. The native Opus cache-cost correction reuses existing list-cost receipts; it did not rerun inference. Run the regression checks with:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest tools.task_cost_eval.test_requested_benchmarks tools.task_cost_eval.test_compare_codex tools.task_cost_eval.test_compare_additional tools.task_cost_eval.test_acp_driver
PYTHONDONTWRITEBYTECODE=1 python3 -B -m unittest tools.task_cost_eval.test_requested_cohort tools.task_cost_eval.test_requested_summary tools.task_cost_eval.test_render_benchmarks
python3 -B -m tools.task_cost_eval.render_benchmarks --summary tools/task_cost_eval/results/requested-20261016-summary.json --benchmarks BENCHMARKS.md --readme README.md --check
```
