# Context retrieval measured results — 2026-10-16

Production retrieval uses Jev for evidence selection, with local fallback when
selection is unavailable. The release keeps that tradeoff explicit: controlled
measurements show lower cost but do not establish equal answer quality or savings
across a full agent session. Known omissions and recovery limits remain below.

The latest [paired local/Jev/d1 comparison](d1-context-retrieval-results.md) retains
12 answers per alternative. The [larger-model cost estimates](jev-model-cost-estimates.md)
reuse those measurements without running new model benchmarks. This report also
preserves the earlier four-arm comparison and retrieval-only measurements.

## Completed live comparison

The [complete report](../scripts/benchmarks/context-retrieval/live-warm-results-2026-10-16.json)
contains **48 completed main calls and 12 completed production Jev calls**, with
**zero transport/adapter failures**. Two independent trials each cover three
synthetic tasks and two append-only turns in each of four arms. The run exited 0
in 711.58 seconds. Each arm has 12 answers and 20 expected evidence groups.

### Actual reported totals

- **Full-corpus reread control**: **$0.0935646320 total**; 996,496 main input, 865 main output, 336,640 cached input tokens (33.78% of main input). Task-complete answers **11/12**; strict complete-line answers **11/12**; literal evidence **20/20**.
- **Known-path fallback + recovery control**: **$0.0305758880 total**; 329,762 main input, 453 main output, 114,560 cached input tokens (34.74% of main input). Task-complete answers **11/12**; strict complete-line answers **7/12**; literal evidence **14/20**.
- **Local production retrieval**: **$0.0087856776 total**; 97,671 main input, 668 main output, 36,992 cached input tokens (37.87% of main input). Task-complete answers **8/12**; strict complete-line answers **4/12**; literal evidence **10/20**.
- **Jev production retrieval**: **$0.0066984512 total**; 39,594 main input, 428 main output, 14,784 cached input tokens (37.34% of main input). Task-complete answers **7/12**; strict complete-line answers **3/12**; literal evidence **8/20**.

Jev total includes **$0.0036346352 main + $0.0030638160 auxiliary**. Its 12
auxiliary calls report **72,948 input + 4,444 output tokens**. Auxiliary cost is
**45.74% of Jev's measured total**, not negligible. Auxiliary cache-token counts
are absent and remain null; they are not added to main-model cache counts.
The four-arm completed experiment totals **$0.1396246488**.

Local cost is 71.27% below the controlled known-path fallback/recovery cost;
Jev including overhead is 78.09% below it and 23.76% below local. These are
**observed cost ratios, not equivalent-quality savings**: task-complete scores are
11/12 for fallback/recovery, 8/12 for local, and 7/12 for Jev.

### Cold versus warm observations

Each entry below covers six calls: three tasks across two trials. Costs include
Jev auxiliary calls where applicable. Cache fractions use summed cached tokens
/ summed main input tokens, not averages of per-call percentages.

**Cold first turn:**

- Full-corpus reread control: **$0.0472836000**, cached **0/336,784** (0.00%); task-complete **5/6**.
- Known-path fallback + recovery control: **$0.0161033600**, cached **0/114,638** (0.00%); task-complete **5/6**.
- Local production retrieval: **$0.0052939600**, cached **0/37,228** (0.00%); task-complete **2/6**.
- Jev production retrieval: **$0.0040967080**, cached **0/17,910** (0.00%); task-complete **3/6**.

**Warm repeated turn:**

- Full-corpus reread control: **$0.0462810320**, cached **336,640/659,712** (51.03%); task-complete **6/6**.
- Known-path fallback + recovery control: **$0.0144725280**, cached **114,560/215,124** (53.25%); task-complete **6/6**.
- Local production retrieval: **$0.0034917176**, cached **36,992/60,443** (61.20%); task-complete **6/6**.
- Jev production retrieval: **$0.0026017432**, cached **14,784/21,684** (68.18%); task-complete **4/6**.

Warm cache hits occurred in **23/24** repeated-turn calls (6/6 in each non-Jev
arm, 5/6 in Jev); all 24 cold calls report
zero cached tokens. Warm local answers reach 6/6 task completeness, versus 4/6
for Jev. Baseline reread cost changes little because the repeated turn appends
another full corpus: caching the earlier prefix does not make newly appended
rereads free. These observations do not show how often a real agent would reread.

### Quality and manual review

Two explicit quality views prevent formatting from masquerading as factual loss:

- **Literal evidence / strict complete-line score** checks exact full fixture
  lines. A strict-complete answer includes every evidence group, contains no
  `MISSING` or `no heading` marker, and does not finish at the output limit.
- **Task-fact completeness** checks fixed expected task facts, preserves the
  exact Unicode label, and uses the same missing-answer/truncation exclusions.
  This additional metric was computed offline after reviewing retained answers;
  it is not a preregistered score or a general semantic judge. Every answer and
  both scores remain in JSON for review.

For example, local, Jev and fallback/recovery all return the exact Unicode label
`São Paulo — peça №4` in all four label answers. They fail strict whole-JSON-line
reproduction but **pass the actual label task**. Reporting these as factual
failures would be wrong. Full-corpus control reproduces the full JSON records.

Production local retrieval contains **20/20** gold evidence groups, but the main
model sometimes claims evidence is missing. Jev retrieval contains **16/20**:
it omits the HTML policy heading on all four HTML queries. All four corresponding
main answers are incomplete: three say `MISSING`; the fourth explicitly says
there is no heading while returning the grace period. No production Jev fallback
or transport failure occurred. Answer and retrieval quality must remain separate.
One full-corpus answer includes both gold lines but contradictorily says `MISSING`;
it gets literal evidence credit but is not task-complete.

### Method, controls and scope

[`live.py`](../scripts/benchmarks/context-retrieval/live.py) uses the existing Rust
production adapter, not a duplicate retrieval implementation. Fixture manifest
SHA-256 is `2c3ee531ec7aa674a549f2bd91306b8164b4ac2d33570ab8a4b6df2647e84f93`.
Every local, Jev and explicit-path call uses compiled production source identity
`ae87f075186d4c487fee7342e57451e3fea18bafb2181ba941f73f1c52f85636`.
The report retains the adapter binary SHA-256 and live endpoint/pricing snapshot.

All main calls use configured OpenRouter option **`xiaomi/mimo-v2.6-flash`**,
pinned to **`xiaomi/fp8`**, with provider fallback disabled. Every response reports
provider **Xiaomi**. The endpoint advertised implicit caching, $0.14/M input,
$0.28/M output, and $0.0028/M cached input; no explicit cache-control directive is
sent. Reported costs, rather than multiplication by these rates, are the results.
This is **not the configured default main model**, `chatgpt/gpt-6.1-sol`, whose
configuration uses a different transport. Reasoning is disabled, temperature is
0, and output is capped at 512 tokens in all arms. Results do not generalize to
the default reasoning model or its configured effort.

Each task/trial uses one common run/trial/task identity with a distinct arm marker
on the first system line, before shared history text. This isolates cache prefixes
between arms and trials. The identical 256-line synthetic history padding is
retained; each warm turn appends the prior user evidence and actual assistant
answer without rewriting history. Arm order rotates across tasks, trials and
turns. The repeated question and task/model/provider are identical across arms;
only evidence/workflow and required cache-isolation marker differ.

The controls are deliberately distinct:

- **Full-corpus reread** appends every allowed session source each turn. This is
  a controlled stress comparison, **not actual existing Distill behavior**.
- **Known-path fallback/recovery** calls the actual production explicit-path
  **utility-unavailable fallback**, then unconditionally reads the complete
  history-known source from disk and supplies both results to the main model.
  This models `read_file` recovery contents, but does not invoke the full harness
  `read_file` tool or pay for an agent turn deciding to recover. History names
  `test-log`, `html-policy`, or `mcp-json` upfront for the respective tasks in
  **all** arms. No gold line offsets or gold-derived source selection are used.
  Recovery narrows the source, not the line window. It deliberately rereads that
  source each turn and does not represent all existing Distill workflows.
- **Local / Jev** invoke production cross-output retrieval on the same allowed
  session corpus. No additional main-model recovery is allowed in these arms;
  thus quality is not equalized by hidden rescue reads.

### Billing, bounds and earlier failed attempt

Main charges and cache counts come from raw OpenRouter `usage`. All **48 main
generation-accounting lookups also returned 404** after their bounded read-only
retries; their data remains null, not a successful independent cross-check.
All main responses nevertheless include numeric `usage.cost` and cache counters.
These are provider-reported charges, not independently reconciled invoice data.
Jev decision IDs begin `gen-dec-`; querying
the generation endpoint for the earlier decision returned 404. The completed run
instead captures **existing provider response billing through a benchmark-scoped
attempt observer**, including `cost_usd_ticks` at 10^10 ticks/USD. All 12 actual
auxiliary charges were recovered. Selection behavior and provider requests are
unchanged. Account-wide before/after deltas are also retained but are explicitly
**not attributed** to Jev: they can lag or include concurrent spend and were zero
here despite nonzero response charges.

The **best-effort $2 threshold is not a hard spending cap**. Preflight combines
per-arm conservative decoded-UTF-8-byte/token input estimates, no assumed cache
discount, a $0.25 auxiliary allowance, and $0.06 for prior spend. Its computed
estimate is **$1.35674304**, not a guaranteed total-cost bound. The retained catalog
contains no verified decision-endpoint token-price ceiling. The client defaults
to 2,048 completion tokens, but this alone does not bound input charges or verify
the effective configuration. The $0.25 allowance therefore remains unverified;
an in-flight call can exceed it or the $2 threshold.

Maximum calls are 48 main and 12 Jev; main output caps at 512 tokens; production
Jev uses one attempt with a 3-second selection deadline. There are no paid
generation retries. Read-only generation accounting can retry 404 up to three
times. Actual completed-run charges are far below the allowance.

After independent review, the runner now starts authorized Jev calls with null
auxiliary cost, parses available billing before scoring or rejecting a nonzero
adapter exit, and **stops all further paid dispatch on unknown auxiliary cost**.
Timeouts and malformed responses retain null rather than zero. Zero without
reported billing requires explicit production confirmation of no dispatch and an
empty attempt list. After adding a reported Jev charge, the runner checks remaining
main-call allowance again before submitting the main request. These guards do not
retroactively impose a hard cap on an already dispatched call. Completed measured
costs and answers were not changed or rerun; all 12 measured auxiliary costs were
known. Older report preflight field names describe the historical estimate only.

The [initial aborted report](../scripts/benchmarks/context-retrieval/live-results-2026-10-16.json)
is retained separately. It incurred **$0.00878052 known main charges plus one
unmeasured Jev charge** before the old runner stopped on missing generation
accounting. Combined known measured charges are **$0.1484051688 plus that unknown**;
this is not an exact all-session bill. A separate advisory Jev methodology review
also lacked cost telemetry and is outside the benchmark ledger. The earlier
Python input-bound failure is fixed: transport JSON escaping is no longer counted
as model input. No aborted rows are substituted into the completed comparison.

### Validation and reproduction

- `PYTHONDONTWRITEBYTECODE=1 python3 scripts/benchmarks/context-retrieval/check.py`:
  **29 passed**. Checks cover decoded-content input bounds, prefix isolation,
  literal/task scores, pinned routing, history-known recovery, complete mocked
  cold/warm replay, actual-cost aggregation, stop-on-unknown auxiliary billing,
  timeout/malformed/nonzero exits, billing retained on scoring failure, confirmed
  no-dispatch zero cost, and prevention of main dispatch after an excessive Jev
  charge. These post-review checks make no paid calls.
- `cargo build -p distill-shell --example context_retrieval_adapter --features test-support`:
  **passed** with the benchmark-only billing observer.
- `cargo test -p distill-shell --lib --features test-support stored_output_ask`:
  **25 passed, 0 failed**.
- Live four-arm command: **exit 0**, 48/48 main and 12/12 auxiliary calls complete.
  Fixture identity, provider consistency, aggregate arithmetic and credential
  absence in persisted artifacts are independently checked.

For this completed run only, Jev adapter wall time also includes the after-call
account lookup. Use production `retrieval_latency_ms`, not that wall field, for
retrieval latency. The runner now stops that timer before account lookup. Costs,
cache tokens, answers and production timings are unaffected by this reporting fix.

```sh
cargo build -p distill-shell --example context_retrieval_adapter --features test-support
python3 scripts/benchmarks/context-retrieval/check.py
FIXTURES="$(mktemp -d)/fixtures"
python3 scripts/benchmarks/context-retrieval/generate.py "$FIXTURES"
python3 scripts/benchmarks/context-retrieval/live.py --fixtures "$FIXTURES" --adapter target/debug/examples/context_retrieval_adapter --output /tmp/context-live-new-report.json --allow-network
```

The output path must not exist. The runner reads `~/.distill/openrouter-auth.json`
field `key` without printing it. Reproduction incurs paid usage and must satisfy
the live preflight; do not run concurrently with other spend if interpreting
account-wide deltas.

### Merge implication and remaining limits

The measured replay establishes that local/Jev can reduce billed replay cost
**even with warm cache reuse and actual Jev overhead included**. It does **not**
establish equal-quality end-to-end Distill savings. Local warm task quality
matches both controls here; Jev still loses the required HTML heading. Across
cold plus warm turns, both retrieval arms underperform the controls on task
completeness. This is evidence **against treating the optimization as default-rollout
ready**, not a universal PR merge verdict independent of its rollout policy.

Only three synthetic tasks, two trials, two turns, one configured non-default
model/provider and a forced utility-unavailable workflow were measured. No real
coding task, utility-available existing baseline, adaptive recovery decision,
default main model, or full agent harness was run. Synthetic history padding and
forced rereads affect cache economics. No confidence intervals or general
production savings claim follow from this sample. No merge or commit was made.

## Earlier retrieval-only measurements

### Provenance and scope

Measurements come from `final-lexical.json` (offline) and `final-live-jev.json` (live).
The [compact results JSON](../scripts/benchmarks/context-retrieval/results-2026-10-16.json)
retains every query metric, selection/fallback, raw reported usage, response
metadata, original aggregates, and computed cohort aggregates. Large raw
responses, excerpt bodies, rendered text, and personal temporary paths are
omitted to keep this artifact compact and portable.

Both runs use fixture manifest SHA-256
`2c3ee531ec7aa674a549f2bd91306b8164b4ac2d33570ab8a4b6df2647e84f93`
and `rust-production`, with explicit-path selector
`production utility-unavailable fallback`. Compiled production source identities:

- Final lexical: `57c6ce1082d44ea6663b202f4b6c4f04e1a2f64ae24fc426d4ca06a1d3d3d6dc`.
- Live Jev: `57c6ce1082d44ea6663b202f4b6c4f04e1a2f64ae24fc426d4ca06a1d3d3d6dc`.

The final-binary live rerun completed with exit 0 after the membership-only fix.
Both headline runs use the same compiled production source identity.

### Quality and coverage

Nine queries include five new positive cross-output queries, three legacy
explicit-path cases, and one session no-match query. Whole-corpus evidence recall
is **70% (7/10) lexical, 60% (6/10) live Jev**, retaining unsupported evidence in
the denominator. Supported-only recall is 7/9 and 6/9, not the headline result.

- New positive queries: lexical **7/7** evidence groups, live **6/7**. Jev lost
  the HTML heading in `html-policy` (1/2), despite returning less text.
- Legacy `explicit-path`: **0/2** evidence groups in both modes; status `ok`
  does not mean the answer was correct.
- Legacy `long-json-line`: **unsupported, 0/1** in both modes. Diagnostic:
  `explicit-path long-line answer unsupported; use read_file or jq`.
- Legacy `path-isolation` and `session-no-match`: both correctly empty in both
  modes (2/2 no-match checks). Live session no-match uses `lexical: no candidates`
  fallback; legacy supported cases use the utility-unavailable fallback.
- Whole-corpus micro line precision: lexical **1.3671875%**, live **6%**.
  This remains synthetic line evidence, not an end-to-end task score.

### Returned bytes and latency

The baseline is a **hypothetical reread of the entire allowed corpus per query**,
not existing Distill behavior, an existing `ask_stored_output` baseline, or a
full-turn cost comparison. Values below sum returned text bytes and baseline
bytes only for supported cases, then divide sums. They exclude response-envelope
bytes. Token estimates using UTF-8 bytes / 4 are not tokenizer usage or billing.

- Five new positive cross-output queries: **747,960 raw bytes**;
  lexical **52,027 returned (6.9559%; 93.0441% smaller)**;
  live **7,121 returned (0.9521%; 99.0479% smaller)**.
- Three legacy explicit-path cases plus session no-match: only **three supported
  cases** contribute bytes, totaling **257,874 raw**; lexical **4,593 returned
  (1.7811%)**, live **4,603 (1.7850%)**. The unsupported fourth case contributes
  neither raw bytes nor its 71-byte diagnostic to these ratios.
  Within this cohort, the two supported legacy cases contribute **108,282 raw /
  4,351 returned** in either mode; session no-match contributes **149,592 raw /
  242 lexical or 252 live returned**. Low bytes do not excuse the 0/2 failure.
- All eight supported cases: **1,005,834 raw**; lexical **56,620 returned
  (5.6292%)**, live **11,724 (1.1656%)**. These totals must not hide cohort quality.

Median production retrieval latency for the five positive queries is **12.146167
ms lexical vs 1,322.651083 ms live**. Across all eight supported cases it is
**11.710896 ms vs 1,169.554229 ms**. These are medians of reported retrieval latency,
not adapter wall time; both per-query timings remain in JSON. No production
latency exists for the unsupported case. These are single-run descriptive
measurements, not controlled cold/warm latency claims.

The five positive live calls report **39,948 input + 2,257 output = 42,205 tokens**.
Raw per-query counts remain in JSON; missing usage remains null, not zero.
Original report-level usage aggregates are null; the positive-call sum is a
separately labeled computation. Exact cost and observed cache-hit rate are
**null/unmeasured**. Smaller returned text cannot establish billed savings once
Jev calls, changed evidence, and prompt-cache reuse are considered.

### Validation and limits

Final checks: Rust `session_output` **23 passed**,
`stored_output_ask` **25 passed**, `jev_store` **8 passed**; these filter counts
overlap and must not be summed as unique tests. Python **18 passed**; build
**passed**. The cache-history test passes, but provider cache behavior was not
measured. Arithmetic and source preservation checks pass.

See [architecture, benchmark contract, and production bounds](context-retrieval-benchmarks.md#native-architecture-and-boundaries):
256 session entries, 4,096 session chunks, 8 MiB manifest; 16 MiB and 512 chunks
per source; 2,048-byte chunks; 16 KiB answer; Jev shortlist 24, state 24 KiB,
3-second selection deadline. These limits do not guarantee evidence completeness
or bound total provider spend. Oversized-line handling remains unsupported here.

### Source insights and next acceptance gate

[fast-jev-compaction](https://github.com/tamaratran/fast-jev-compaction), especially
[`src/compact.ts`](https://github.com/tamaratran/fast-jev-compaction/blob/main/src/compact.ts),
shows literal retention, bounded keep decisions, first/recent-message pinning,
and paired call/result deletion. Useful lessons are literal evidence and bounded
selection; deletion of history is not adopted. It can invalidate cached prefixes
and increase total cost. See the [existing source analysis](context-retrieval-benchmarks.md#source-insights-and-deferred-compaction-experiment).

This earlier retrieval-only checkpoint did not establish total cost or cache
reuse. The later paid comparisons above measure both, including Jev overhead.
Evidence loss and legacy fallback limits remain unresolved. Realistic agent-task
comparisons are still needed before claiming equal-quality savings; byte
reduction alone is not evidence of that benefit.
