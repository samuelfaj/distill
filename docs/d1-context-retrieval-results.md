# d1 native context retrieval: completed paired comparison

## Outcome

The fresh [completed report](../scripts/benchmarks/context-retrieval/d1-completed-results-2026-10-16.json)
contains **36 main-model calls and 24 auxiliary decisions calls**: 12 samples per
arm, two trials, three tasks and two history-preserving turns. Every requested
combination completed, including Unicode. All main and auxiliary charges are
known for this run. Provider-reported total: **$0.0568904504**.

Native d1 preserved all scored evidence, while Jev lost half the HTML evidence.
That retrieval improvement did not produce better aggregate task-answer accuracy
than local selection. d1 cost **6.67 times** Jev end-to-end and **12.44 times** its
auxiliary cost. These three synthetic tasks are not evidence for a general model
ranking or a production rollout.

## Balanced overall measurements

Every arm has 12 answers and 20 scored evidence groups. Task-complete scoring
checks requested facts; strict-complete scoring requires complete literal source
lines. Answers declaring `MISSING` or ending at the output limit do not count as
complete. Selection recall scores the returned evidence independently of the
main model's use of it.

- **Local:** selection recall **20/20**; task-complete **9/12**;
  strict-complete **5/12**. Main cost **$0.0090287568**; auxiliary **$0**;
  total **$0.0090287568**. Main cache **37,056/98,210 tokens (37.73%)**.
  Retrieval wall latency mean/median **87/83 ms**; main-answer mean/median
  **7,894/8,164 ms**.
- **Jev:** selection recall **16/20**; task-complete **8/12**;
  strict-complete **4/12**. Main cost **$0.0031781176**; auxiliary
  **$0.003063816**; total **$0.0062419336**. Main cache
  **17,792/39,501 tokens (45.04%)**. Retrieval wall latency mean/median
  **1,415/1,410 ms**; main-answer mean/median **4,548/4,457 ms**.
- **d1:** selection recall **20/20**; task-complete **9/12**;
  strict-complete **5/12**. Main cost **$0.003516688**; auxiliary
  **$0.038103072**; total **$0.04161976**. Main cache
  **18,560/41,932 tokens (44.26%)**. Retrieval wall latency mean/median
  **1,782/1,556 ms**; main-answer mean/median **5,338/4,571 ms**.

There were **zero invalid selections, selector errors, fallbacks or main-call
failures** in all three arms. Report totals sum actual response billing, not
catalog estimates or account-balance differences.

## Cold and warm history

Each turn contains six answers per arm. Trial/task/arm-specific system prefixes
isolate cold histories. Turn 1 reuses that arm's previous system, evidence and
answer messages, then appends the follow-up evidence request. No explicit cache
control was sent; the pinned provider advertises implicit caching.

- **Cold, turn 0:** every arm reported zero cached main-input tokens.
  Local task/strict completeness **3/6 and 1/6**, total **$0.0054124**;
  Jev **4/6 and 2/6**, total **$0.004069268**;
  d1 **4/6 and 2/6**, total **$0.021758716**.
- **Warm, turn 1:** local main cache **60.80%**, task/strict completeness
  **6/6 and 4/6**, total **$0.0036163568**;
  Jev cache **82.41%**, completeness **4/6 and 2/6**, total **$0.0021726656**;
  d1 cache **79.68%**, completeness **5/6 and 3/6**, total **$0.019861044**.

Cold and warm selection recall stayed at **10/10** for local and d1 and **8/10**
for Jev. These are observed two-turn cache results, not projected cache savings.
Auxiliary cost is included in both turn totals.

## Per-task results

Each task has four answers per arm.

- **Failure versus keyword decoys:** every arm returned **8/8** evidence groups.
  Local task/strict completeness was **2/4**; both Jev and d1 achieved **4/4**.
  Local's two cold answers declared `MISSING` despite retrieved evidence.
- **HTML policy:** local and d1 returned **8/8** groups; Jev returned **4/8**.
  Task/strict completeness was **3/4 local**, **0/4 Jev**, **1/4 d1**. d1's full
  retrieval recall did not prevent three incomplete main answers.
- **Unicode literal:** every arm returned **4/4** groups and answered the requested
  label `São Paulo — peça №4` correctly in **4/4** samples. Every arm scored
  **0/4 strict whole-record matches**, because the answer was the label rather
  than the complete JSON source line. This is not a Unicode decoding failure.

## Native API, shared contract and differences

OpenRouter's [model endpoints catalog](https://openrouter.ai/api/v1/models/liquid/d1-ultra/endpoints)
advertises `liquid/d1-ultra` as text+image to decisions, Liquid provider, 32,768
context, **$0.042/M input tokens** and zero output price. Actual requests succeeded
at `https://openrouter.ai/api/alpha/decisions`, using the native state/questions
format documented in the [decision tutorial](https://openrouter.ai/docs/guides/community/jev-tutorial.md).
The served snapshots were **`liquid/d1-ultra-20261015`** and
**`typesafe/jev-1.14-20261014`**. No Chat Completions translation was needed.

Main model and provider stayed **`xiaomi/mimo-v2.6-flash`**, **`xiaomi/fp8`**,
with provider fallbacks disabled. Arm order rotates across trials/tasks/turns.
The full-corpus and fallback-recovery arms in
`live-warm-results-2026-10-16.json` remain explicitly historical controls; they
were not substituted into this newly paired comparison.

A `test-support`-gated Rust benchmark bridge exports the exact production lexical
shortlist, 24 KiB-bounded state and unchanged typed include/exclude questions.
Python changes only the model in that native envelope. On application, Rust
rebuilds and compares the entire contract, validates typed choices, preserves
lexical order and uses the production eight-result cap and literal renderer.
The production Jev client and model restrictions remain unchanged.

The d1 arm uses two benchmark adapter processes and Python HTTP rather than the
Jev client. Its retrieval wall latency includes export, native dispatch and
application; `selector_wall_ms` excludes initial export. Rust's production-only
retrieval timer for this arm excludes native dispatch and must not be interpreted
as end-to-end d1 latency. Python uses a three-second socket timeout plus elapsed
deadline check; this is not identical to Rust's hard total-future cancellation.
Successful native calls all passed that elapsed check. Generation/account lookup
time is outside retrieval wall latency. These transport differences limit close
latency comparisons.

## Budget, accounting and the previous interruption

The corrected preflight ran **before any paid dispatch**. It reserved
**$0.792723456** for d1, using 12 calls × 64 KiB complete request bytes × 24
questions × the catalog input-token rate. The question multiplier conservatively
accounts for repeated-state native billing. It also reserved **$0.25** for Jev,
**$0.06** for prior spend and uncached main-input/output upper estimates. The
aggregate estimate was **$1.445927296**, below the **$2 best-effort threshold**.
This is not server-side spend enforcement; Jev's allowance remains unverified
and an in-flight charge can exceed an allowance. Unknown charges halt further
paid calls; known auxiliary charges are checked against the remaining budget
before main dispatch.

The earlier [11-row partial artifact](../scripts/benchmarks/context-retrieval/d1-results-2026-10-16.json)
remains byte-for-byte unchanged. It records **$0.0110683244** in retained charges,
plus a possible unrecorded in-flight charge. The previous runner stopped before
completion. Its recorded interruption label does not establish a provider budget
error or a user-requested stop. This fresh run used a new output path and did not
reuse partial samples. The new run's
**$0.0568904504** is exact for its retained provider-reported charges; it does not
resolve previous unknown charges or constitute an all-session bill.

## Regression checks

- `python3 scripts/benchmarks/context-retrieval/check.py`: **37 passed**, exit 0.
  This covers byte-for-byte preservation of existing reports before the
  annotation lifecycle, safe HTTP-error cost/id extraction without raw error
  text, unknown-charge stops, billing retention after failure, request bounds,
  post-selector budget checks, and the complete 36-row pairing/accounting matrix.
- `python3 -m py_compile scripts/benchmarks/context-retrieval/{live,d1,check}.py`:
  exit 0 after the final code changes.
- `cargo test -p distill-shell --lib --features test-support stored_output_ask`:
  **26 passed**, exit 0, including contract mismatch, typed-choice rejection and
  literal Unicode rendering.
- `cargo build -p distill-shell --example context_retrieval_adapter --features test-support`:
  passed before the run; its SHA-256 is retained in the report. No Rust changes
  occurred between that build and this completed run.
- `shasum -a 256 -c /tmp/d1-partial-sha256.txt`: partial artifact unchanged.

No commit, push or merge. No full-agent evaluation, larger task suite or
resolution of historical unmeasured charges was performed.

## Reproduction

Generate fresh fixtures with `generate.py`, build the adapter with the command
above, then run `scripts/benchmarks/context-retrieval/d1.py` with `--fixtures`,
`--adapter`, a **new** `--output` path and `--allow-network`. Existing output paths
are rejected before network discovery or annotation. The benchmark reads the
existing OpenRouter credential file without printing the key. Paid reruns need
explicit authorization.
