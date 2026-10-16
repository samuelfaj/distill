# Session-scoped stored-output retrieval

**Priority: preserve prompt-cache reuse.** A smaller prompt that invalidates a
cached prefix can cost more. Returned bytes alone do not establish task savings.
This document describes architecture, reproducible evaluation, and limitations;
actual run measurements belong in a separately identified results section.

## Native architecture and boundaries

`ask_stored_output` accepts an optional path. With a path, it reads an approved
stored original under the explicit-path policy. Without a path, it searches
only outputs registered to the current session through a bounded lexical IDF
index, deduplicates candidates, and optionally asks Jev to select relevance.
Evidence stays literal, with source and line provenance; Jev selects evidence
rather than rewriting it. Distill continues to use OpenRouter for Jev transport.

Jev is the only production decision model, regardless of request size. The
production client rejects d1-ultra and other non-Jev model IDs. Disable controls,
shadow mode and local fallback still apply; they do not route to another decision
model. The d1 comparison uses a task-local bridge gated by `test-support` and
cannot reconfigure a normal production task.

Registration gives new outputs session-scoped ownership. The legacy original
policy deliberately retains old explicit-path access; that compatibility is not
authorization to discover all historical outputs through session search. Limit
errors remain visible, never trigger a silent global search. Replacement of a
tool result occurs only for approved content. Retrieval does not delete or
rewrite prior messages or mutate historical conversation prefixes.

Production bounds:

- Source: 16 MiB, 512 chunks; chunk: 2,048 bytes.
- Session: 256 entries, 4,096 chunks, 8 MiB manifest.
- Returned answer: 16 KiB.
- Jev: shortlist of 24 candidates, 24 KiB state, 3-second selection deadline.

These are retrieval bounds, not guarantees of evidence completeness or total
provider spend. Explicit-path legacy fallback has different behavior from
cross-output selection; the benchmark reports that distinction.

## Fixtures and interpretation

`generate.py` creates seven deterministic synthetic sources and nine queries:
realistic test logs, JSON/MCP records, HTML policy text, duplicate releases,
Unicode, keyword-heavy stale/example decoys, a decoy session, no-match cases,
explicit-path isolation, and an oversized JSON line. Gold records exact source
IDs, 1-based lines, and literal text. Equivalent duplicate locations share one
evidence requirement. Do not remove unsupported queries or edit gold to fit
retrieval behavior.

The offline full corpus has nine queries: eight supported and one known
unsupported case, `long-json-line`. This describes coverage, not measured quality,
latency, cost, or savings. Explicit-path queries exercise the legacy fallback.

**The raw baseline is a hypothetical reread of the entire allowed corpus for a
query, not existing Distill behavior and not a full-turn cost baseline.** Reports
include raw bytes and content-deduplicated bytes separately. UTF-8 `bytes / 4` is
only a token estimate, not tokenizer usage, provider billing, or cache savings.

Metrics retain every query:

- Overall evidence recall: satisfied gold groups / all gold groups. Unsupported
  positive evidence counts as missed. Duplicate alternatives do not inflate hits.
- Line precision: relevant distinct source/line pairs / all returned pairs;
  adjacent context and keyword decoys count against precision.
- Supported/unsupported counts and a secondary supported-only aggregate.
- No-match correctness, full returned UTF-8 payload bytes, response-envelope
  bytes, returned/raw ratio, production retrieval latency, and adapter wall time.

Each query starts a process. Production latency excludes registration/startup;
wall time includes them. Compare identical fixtures, build/configuration, modes,
and cold/warm conditions. Retain failures; do not replace overall recall with
the supported-only number or treat size reduction as evidence of equal quality.

## Run commands

Run from the repository root. Supply a reviewed, already-built production Rust
adapter; these scripts do not build Rust or implement another retrieval engine.

```sh
export PYTHONDONTWRITEBYTECODE=1
python3 scripts/benchmarks/context-retrieval/check.py
work=$(mktemp -d)
python3 scripts/benchmarks/context-retrieval/generate.py "$work/fixtures"
adapter=/absolute/path/to/production-retrieval-adapter
python3 scripts/benchmarks/context-retrieval/evaluate.py \
  --fixtures "$work/fixtures" --adapter "$adapter" --mode lexical \
  --timeout-seconds 30 --output "$work/lexical-report.json"
```

Output must not already exist. To verify generation, generate into another
empty directory and compare recursively. Tests exercise fixtures and scorer
boundaries with mocked adapters; they are not production benchmark results.

Only after reviewing the live adapter and explicitly authorizing charges:

```sh
python3 scripts/benchmarks/context-retrieval/evaluate.py \
  --fixtures "$work/fixtures" --adapter "$adapter" --mode jev --allow-network \
  --timeout-seconds 60 --output "$work/live-jev-report.json"
```

Offline is the default. Live requires the nine-query corpus, makes one adapter
invocation per query, and adds no runner retries. The current live adapter
supports cross-output Jev selection; explicit-path queries still request lexical
mode with network disabled. Reports retain suite mode, per-query effective mode,
actual network use, selection, and fallback. A fallback never proves Jev selected
evidence. Process count and timeout are not hard dollar limits: internal calls
or retries can cost money, and timeout cannot undo submitted charges.

## Adapter contract and unsupported cases

Version 1 uses one JSON request on stdin and one JSON response on stdout per
process; stderr is diagnostic. Request fields are `schema_version`, `query_id`,
`session_id`, `question`, `mode`, `network_allowed`, source metadata, and optional
`path_source_id`. The adapter registers originals in isolated temporary storage
and invokes actual Rust production retrieval. Gold is never selector input.

Responses identify the production implementation/revision and actual mode,
`network_used`, status, full unchanged `rendered_text`, literal `excerpts` with
source/session/hash/inclusive line ranges, and measured `retrieval_latency_ms`.
Status is `ok`, `no_match`, `fallback`, or the narrowly declared `unsupported`.
Actual selection is top-level `selection` or `production.selection`; fallback
reason comes from `fallback` or the production selection description. Build
identity stays invariant except per-query mode, network use, and selection.

The scorer rejects invalid scope, hashes, ranges, nonliteral excerpts,
inconsistent statuses, unauthorized network use, and changed build identity.
Structured excerpts must cover all returned source evidence, not a favorable
subset. Adapter review must verify completeness and that production code runs.
`returned_bytes` measures the **full rendered payload**, including headers,
pointers, and provenance, never just structured evidence. Requests are a trusted
local-code contract, not an operating-system network sandbox.

Only `long-json-line` may report `unsupported`, with empty excerpts and reason
exactly `explicit-path long-line answer unsupported; use read_file or jq`.
Prefer a normal structured response carrying identity, full refusal text, and
measured latency. Existing Rust compatibility also accepts that query alone
with exit 1, empty stdout, and stderr exactly the following line plus one LF:

`Error: explicit-path long-line answer unsupported; use read_file or jq`

That legacy signal retains the diagnostic but has null production latency,
network evidence, and independent identity; only wall time is known. Every other
error, changed signal, integrity failure, or unsupported case aborts loudly and
writes no final report. Declared unsupported cases remain in both raw responses
and scored rows, allowing transparent partial coverage across all nine queries.

Per-query provider usage is retained only when actually supplied by measured
calls. Missing usage is null, never zero. Aggregate provider usage, cost, Jev
usage, and cache hit rate remain null; this runner does not infer billing totals
or a cache hit rate. No live measurement is claimed by these instructions.

## Cache-aware accounting

Generate explicit hypothetical assumptions and simulate them independently:

```sh
python3 scripts/benchmarks/context-retrieval/cost_scenarios.py \
  --example > "$work/cost-input.json"
python3 scripts/benchmarks/context-retrieval/cost_scenarios.py \
  --input "$work/cost-input.json" > "$work/cost-simulation.json"
```

Optionally attach the same simulation using evaluator flag
`--cost-scenarios "$work/cost-input.json"`. It stays separate from measured query
metrics. Inputs require explicit USD-per-million rates and estimated tokens for
uncached input, cache reads, cache writes, and output, plus estimated auxiliary
Jev dollars. The input buckets are mutually exclusive; cache-write rate is the
**all-in** rate, including base input if the provider quotes only a surcharge.
A null write rate is permitted only with zero writes. Missing billing is not an
assumed zero. Cost is:

`(uncached * input_rate + reads * read_rate + writes * all_in_write_rate + output * output_rate) / 1,000,000 + auxiliary_jev_usd`

Built-in rates are invented illustrations: $2/M uncached, $0.20/M reads, $2.50/M
writes, $8/M output. With 1,000 output tokens:

- Cached baseline: 10,000 uncached + 90,000 read tokens costs $0.046.
- Preserved prefix: 2,000 uncached + 90,000 reads + $0.001 Jev costs $0.031.
- Lost cache: 20,000 uncached + $0.001 Jev costs $0.049: smaller but dearer.
- Rebuilt cache: 2,000 uncached + 18,000 writes + $0.001 Jev costs $0.058.

These are simulations, not observed savings. For the lost-cache example,
reclassifying about 1,666.67 uncached tokens as reads breaks even, holding total
input, writes, output, and auxiliary work fixed. In general, additional reads
needed are `extra_cost * 1,000,000 / (input_rate - read_rate)` when a discount
exists; feasibility also requires enough uncached tokens. Tests cover this
counterexample and sensitivity. No assumed read count becomes a measured hit rate.

Fair comparisons need the same task, quality, full-prompt horizon, outputs,
retries, and auxiliary work. Model provider TTL, minimum prefixes, block rounding,
routing, eviction, write tiers, and multi-call reuse explicitly. Include billable
reasoning when applicable. Fixture bytes cannot determine these quantities.

## Source insights and deferred compaction experiment

Reviewed upstream [fast-jev-compaction](https://github.com/tamaratran/fast-jev-compaction),
particularly its README and [`src/compact.ts`](https://github.com/tamaratran/fast-jev-compaction/blob/main/src/compact.ts):

- It pins first/recent messages and makes two `noul` keep-call/keep-result
  decisions per call, safely removes the pair, and keeps retained text verbatim.
- Jev state omits outputs; batches repeat state. Its default direct TypeSafe
  transport differs from Distill's OpenRouter transport.

Useful principles here are literal evidence and bounded selection calls. No
upstream code was copied. Do not adopt deletion of prior messages for retrieval:
it can invalidate cached prefixes. A future compaction-only experiment must
compare against existing Distill compaction and measure post-compaction cache
reuse, evidence/task quality, and total cost including Jev. Character reduction
alone is not acceptance evidence. Historical-message mutation is outside this
implementation.
