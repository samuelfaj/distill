# Worker and utility models after Jev retrieval

This experiment compares three ways to prepare context for `openai/gpt-6-astra`:

- Jev-selected evidence without another model.
- The same evidence, narrowed by `openai/gpt-6-luna` as a retrieval worker.
- The same evidence, narrowed by `inclusionai/ling-3.0-flash-vl` as a retrieval utility.

The experiment is opt-in and changes no production model routing. It tests an additional extractive step, not free-form summaries or a full coding-agent session.

## Model choices

The worker is the OpenRouter API equivalent of the configured `chatgpt/gpt-6-luna` worker. This experiment does not use the ChatGPT subscription transport. The utility model is the first paid model in Distill's shipped utility fallback chain. The local configuration has no explicit utility-model override. Pinning that model for the experiment does not exercise the automatic utility resolver or its free-tier fallback attempts.

Astra and Luna use the ordinary `openai` provider; Ling uses `deepinfra/fp16`. Provider fallback is disabled. The report retains the public endpoint snapshots, requested models, actual response model/provider and reported billing. Temperature zero is requested only when an endpoint advertises support. Astra requires reasoning, so every main call requests `low` effort with a 2,048-token completion ceiling that also covers reasoning. Helper reasoning is requested disabled. Actual reported reasoning tokens remain part of accounting.

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
  --allow-network --budget-usd 60 --spend-limit-usd 3.9
```

The $60 argument admits a deliberately loose whole-run byte ceiling. It does not replace the separate $3.90 incremental spending check. That lower limit reserves more than $1.10 of the original $5 allowance for the earlier rejected main request and its unknown charge. Model and provider overrides are available through `--main-model`, `--worker-model`, `--utility-model` and their corresponding `--*-provider` options. Changing these choices produces a different experiment.
