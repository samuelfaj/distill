# Context retrieval measured results — 2026-10-16

**DRAFT — not ready for default roll-out.** Synthetic retrieval evidence only;
these runs do not establish realistic task quality, billed cost reduction, or
provider cache-hit benefits.

## Provenance and scope

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

## Quality and coverage

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

## Returned bytes and latency

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

## Validation and limits

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

## Source insights and next acceptance gate

[fast-jev-compaction](https://github.com/tamaratran/fast-jev-compaction), especially
[`src/compact.ts`](https://github.com/tamaratran/fast-jev-compaction/blob/main/src/compact.ts),
shows literal retention, bounded keep decisions, first/recent-message pinning,
and paired call/result deletion. Useful lessons are literal evidence and bounded
selection; deletion of history is not adopted. It can invalidate cached prefixes
and increase total cost. See the [existing source analysis](context-retrieval-benchmarks.md#source-insights-and-deferred-compaction-experiment).

Default roll-out requires fixing evidence loss and legacy failures, realistic
task comparisons against existing Distill/ask behavior, and measured total
provider cost/main-model cache reuse including Jev overhead.
Byte reduction alone is not acceptance evidence.
