# Worker and utility models after Jev retrieval

This experiment compares three ways to prepare context for `openai/gpt-6-astra`:

- Jev-selected evidence without another model.
- The same evidence, narrowed by `openai/gpt-6-luna` as a retrieval worker.
- The same evidence, narrowed by `qwen/qwen3.7-flash` as a retrieval utility.

The benchmark runner remains opt-in. This PR also enables the utility extraction step in production session-output search after successful Jev selection. It uses the existing utility lane, with `openrouter/free`, then `nvidia/nemotron-3.5-lightning:floor`, then `inclusionai/ling-3.0-flash:floor` unless the user configured another utility model. It preserves original evidence when extraction fails, is invalid or selects nothing. This is literal line selection, not free-form summaries or a full coding-agent session. The retained measurements below predate this production integration and measured Qwen, not Ling.

## Model choices

The worker is the OpenRouter API equivalent of the configured `chatgpt/gpt-6-luna` worker. This experiment does not use the ChatGPT subscription transport. The utility is a model in Distill's shipped fallback chain. The local configuration has no explicit utility-model override. Ling 3.0 Flash VL, the first paid fallback and runner default, returned HTTP 429 during the initial attempt. The comparison therefore explicitly pins the next shipped fallback, Qwen 3.7 Flash. It does not exercise the automatic utility resolver or claim that Ling's quality was measured.

Astra and Luna use the ordinary `openai` provider; Qwen uses `alibaba`. Provider fallback is disabled. The report retains the public endpoint snapshots, requested models, actual response model/provider and reported billing. Temperature zero is requested only when an endpoint advertises support. Astra requires reasoning, so every main call requests `low` effort with a 2,048-token completion ceiling that also covers reasoning. Helper reasoning is requested disabled. Actual reported reasoning tokens remain part of accounting.

## Paired method

The existing Rust production adapter performs Jev retrieval once for each trial, task and turn. All three alternatives receive exactly that same result. Worker and utility models can select source-line IDs only. The runner rejects invented IDs, duplicates, malformed responses and oversized selections; it copies accepted text from the source rather than trusting generated quotations. Invalid helper output keeps the original Jev evidence and is labeled as a fallback, with its charge retained.

Every alternative uses the same compact `source:line` rendering. A helper that selects every line therefore gives Astra the same evidence as the baseline. This isolates model selection from deterministic formatting savings. The baseline is Jev with this common formatting, not the byte-for-byte default tool response. Original Rust output and source provenance remain in the report.

There are two trials, three existing synthetic tasks and two append-only turns per task. The tasks cover archived log decoys, an HTML policy and an exact Unicode label. Main-model histories use different arm markers before shared history, retain previous turns unchanged and rotate execution order. Worker and utility calls are separate closed tasks without tools. Neither helper receives gold answers or source files outside the Jev result.

## Quality checks

The evaluation keeps evidence quality separate from answer quality:

- Evidence recall before the extra step measures what Jev supplied.
- Evidence recall after the extra step measures what reached Astra. Helpers cannot recover evidence Jev omitted.
- Strict answer completeness checks the original literal evidence lines.
- Task-fact completeness checks the existing fixed fact rubric, including the exact Unicode label. It rejects missing-answer markers. The label task does not require reproducing its entire JSON record.
- Grounded task completeness additionally requires every expected evidence group to remain in the final excerpts. A model guessing a correct fact from the question does not establish that retrieval preserved its support.

These checks do not judge arbitrary prose, prove absence of unrelated errors or establish statistical significance. Raw answers and helper selections remain available for review.

## Cost and cache accounting

Each alternative's comparison cost includes its Astra calls, its helper calls if any, and the same measured Jev charge. Jev is physically called once per paired case, so adding the three alternative costs counts that shared charge three times. The report separately records actual physical spending, which counts each dispatched request once.

Cache and reasoning counts come from provider responses. Missing fields stay null. The first and repeated turns are reported separately; advertised cache capability does not substitute for observed usage. Auxiliary calls, invalid responses and fallback costs remain in the comparison. No additional recovery reads or model-driven rescue turns are allowed.

The conservative preflight uses byte-based token ceilings and published applicable price tiers. A separate known-spend limit checks the next-call allowance before dispatch. These are client-side safeguards, not a provider-enforced dollar cap. Unknown charges stop further paid calls. Paid dispatches are never retried automatically.

## Completed measurements

The [completed Qwen comparison](../scripts/benchmarks/context-retrieval/tiers-qwen-results-2026-10-16.json) contains 36 Astra calls, 12 shared Jev calls, 12 Luna calls and 12 Qwen calls. All 72 requests have known provider-reported charges. The run completed with exit 0. Each alternative has 12 answers and 20 expected evidence groups.

| Alternative | Evidence before extra step | Evidence delivered to Astra | Grounded task-complete answers | Cost per alternative |
| --- | ---: | ---: | ---: | ---: |
| Jev only, common formatting | 16/20 | 16/20 | 8/12 | $0.294309316 |
| Jev + Luna worker | 16/20 | 14/20 | 8/12 | $0.233777016 |
| Jev + Qwen utility | 16/20 | 16/20 | 8/12 | $0.236139576 |

Strict complete-line and task-fact answer scores are also 8/12 in every alternative. The worker reduced the per-alternative cost by 20.57%; the utility reduced it by 19.76%. These are observed costs on a small controlled workload, not full-session or equal-quality guarantees. In particular, the identical final-answer count hides the worker's additional evidence loss.

Luna returned an empty selection on both cold HTML queries, dropping the remaining grace-period fact. Qwen retained all 16 evidence groups supplied by Jev. Jev itself omitted the required heading on all four HTML queries, so no alternative completed those tasks. All alternatives completed the four log tasks and four Unicode tasks. There were no invalid selections, helper fallbacks or failed requests in this completed run. Luna's empty selections were valid outputs, not parser failures.

### Cost breakdown

| Alternative | Astra calls | Jev charge | Extra extraction charge | Total |
| --- | ---: | ---: | ---: | ---: |
| Jev only | $0.291245500 | $0.003063816 | $0 | $0.294309316 |
| Jev + Luna | $0.229921000 | $0.003063816 | $0.000792200 | $0.233777016 |
| Jev + Qwen | $0.232812000 | $0.003063816 | $0.000263760 | $0.236139576 |

Qwen's extraction charge was 66.7% below Luna's. Its combined total was nevertheless about 1.0% higher because Astra's input, output and cache charges differed. This sample does not establish a reliable overall cost ranking between the two helpers.

Actual physical spending for the completed run was **$0.758098276**. Adding the three alternative totals gives $0.764225908 because that counts the shared Jev charge three times instead of once. These are reported response charges, not an independent invoice reconciliation. Earlier failed attempts are excluded from this completed-run total and disclosed below.

### Cache, writes and reasoning

Every cold main call reported zero cache reads. On repeated turns, cached-read input fractions were **85.86% Jev only**, **95.81% Luna** and **95.50% Qwen**. Actual costs include cache writes as well as reads.

| Alternative | Main prompt tokens | Cache reads, both turns | Cache writes, both turns | Completion tokens, including reasoning | Reported reasoning tokens |
| --- | ---: | ---: | ---: | ---: | ---: |
| Jev only | 36,055 | 16,648 | 19,371 | 642 | 139 |
| Jev + Luna | 29,726 | 14,536 | 15,154 | 512 | 46 |
| Jev + Qwen | 29,910 | 14,602 | 15,272 | 539 | 23 |

Completion and reasoning columns overlap; reasoning is not an additional token bucket to charge again. Helper calls reported zero reasoning tokens. Luna used 7,252 input and 134 output tokens; Qwen used 8,220 input and 132 output tokens. Different helper tokenizers can count the same input differently.

Summed final excerpt payloads were 13,660 bytes for Jev only, 1,464 for Luna and 1,688 for Qwen. These byte reductions are secondary to evidence preservation and actual charges.

### Latency

Mean sums of the measured Jev, extraction and main-response stage times were **5.25 seconds Jev only**, **6.42 seconds Luna** and **6.97 seconds Qwen**. These sums describe hypothetical sequential execution of each alternative; the paired runner physically shares the Jev call. Mean helper-call times were 1.77 seconds for Luna and 1.68 seconds for Qwen. Both extra steps reduced cost while increasing the summed stage time in this sample.

### Failed attempts and remaining limits

Two earlier attempts remain separate and are not mixed into the paired results:

- [Disabled-reasoning attempt](../scripts/benchmarks/context-retrieval/tiers-disabled-reasoning-attempt-2026-10-16.json): the first Astra call returned HTTP 400. Public model metadata identifies Astra reasoning as mandatory. The corrected protocol requests `low` effort instead. The rejected call's charge is unknown; its Jev call reported $0.000193578.
- [Ling attempt](../scripts/benchmarks/context-retrieval/tiers-ling-attempt-2026-10-16.json): Astra and Luna calls succeeded, but the Ling provider returned HTTP 429. That run stopped rather than silently switching models. Retained known charges total $0.071287778; the rejected Ling call's charge remains unknown. The Qwen run used a fresh report and fresh histories.

Public [model constraints](../scripts/benchmarks/context-retrieval/tiers-model-constraints-2026-10-16.json) retain the model reasoning metadata and prices used to investigate those failures. The two failed reports preserve their original bytes. Known charges across the completed run and both attempts total $0.829579632, plus the two unknown rejected-request charges. This is not an exact all-attempt bill.

The utility preserved more evidence than the worker in this pilot. Production now uses the existing utility lane after Jev, with original-evidence fallback for invalid, empty or failed selections. Valid but incomplete selections can still lose evidence; missing-evidence recovery, larger workloads and full agent behavior remain untested. This experiment measures extractive selection, not the factual reliability of generated summaries or the configured subscription worker transport.

### Verification

The production Rust adapter built successfully. The new offline suite passes 21 checks, including reconstruction of all retained excerpt selections, original and final evidence scores, answer scores, pair membership and physical-versus-shared cost totals. The existing retrieval suite passes 37 checks. Independent static review found no concrete defect in the runner's reviewed accounting and isolation logic. The full repository suite was not run; production Rust code did not change in this PR.

## Run the experiment

The runner requires an already-built production adapter and an unused output path. It reads the OpenRouter key from the existing environment or Distill credential file without writing it to the report.

```sh
export DEVELOPER_DIR=/Library/Developer/CommandLineTools
export RUST_MIN_STACK=16777216
export PYTHONDONTWRITEBYTECODE=1
cargo build -p distill-shell --features test-support --example context_retrieval_adapter
python3 scripts/benchmarks/context-retrieval/check_tiers.py
python3 scripts/benchmarks/context-retrieval/check.py
work=$(mktemp -d)
python3 scripts/benchmarks/context-retrieval/generate.py "$work/fixtures"
python3 scripts/benchmarks/context-retrieval/tiers.py \
  --fixtures "$work/fixtures" \
  --adapter target/debug/examples/context_retrieval_adapter \
  --output "$work/tiers.json" \
  --allow-network --budget-usd 60 --spend-limit-usd 3.9 \
  --main-reasoning-effort low --main-max-tokens 2048 \
  --utility-model qwen/qwen3.7-flash --utility-provider alibaba
```

The $60 argument admits a deliberately loose whole-run byte ceiling. It does not replace the separate $3.90 incremental spending check. That lower limit leaves $1.10 of the original $5 allowance for known charges from the earlier attempts and conservative allowances for their two rejected requests with unknown charges. Model and provider overrides are available through `--main-model`, `--worker-model`, `--utility-model` and their corresponding `--*-provider` options. Changing these choices produces a different experiment.
