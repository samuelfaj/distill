# Jev cost estimates for GPT-6.1-Sol, GPT-6-Astra and Opus 5.5

These are price projections, not new model benchmarks. They use existing retrieval measurements and the public [OpenRouter model catalog](https://openrouter.ai/api/v1/models), retrieved on October 16, 2026. The [machine-readable estimates](../scripts/benchmarks/context-retrieval/jev-model-cost-estimates-2026-10-16.json) include the pricing snapshot, calculation inputs, results and cache sensitivity.

## Estimated cost per 1,000 similar retrieval queries

The main model still answers the question. Jev selects literal evidence before that answer. The baseline is the same main model using local lexical retrieval without an auxiliary selector, not rereading the whole session.

| Main model | Local retrieval baseline | Jev-assisted retrieval | Estimated reduction |
| --- | ---: | ---: | ---: |
| GPT-6.1-Sol | $11.58 | $5.10 | 55.9% |
| GPT-6-Astra | $59.46 | $25.24 | 57.6% |
| Claude Opus 5.5 | $23.17 | $9.95 | 57.0% |

These estimates describe the retrieval-query portion of a workload. They do not predict average savings across a full Distill session or establish equal answer quality. In the source experiment, local retrieval produced 9/12 task-complete answers and Jev produced 8/12. Recovery reads and retries to repair incomplete answers would add cost.

## Inputs and formula

The source is the completed [paired retrieval report](../scripts/benchmarks/context-retrieval/d1-completed-results-2026-10-16.json). Each arm contains 12 main calls:

- Local retrieval: 98,210 prompt tokens, including 37,056 cached-read tokens.
- Jev retrieval: 39,501 prompt tokens, including 17,792 cached-read tokens, plus $0.003063816 in measured auxiliary charges.
- Both projections use 1,298 output tokens per 12 calls. Holding output constant avoids counting shorter incomplete answers as savings.

The estimated cost is:

```text
(prompt_tokens - cached_read_tokens) * input_rate
+ cached_read_tokens * cache_read_rate
+ fixed_output_tokens * output_rate
+ auxiliary_Jev_cost
```

Rates below are USD per million tokens. Calculations use per-token prices.

| OpenRouter model ID | Uncached input | Cache read | Output |
| --- | ---: | ---: | ---: |
| `openai/gpt-6.1-sol` | $2.00 | $0.10 | $10.00 |
| `openai/gpt-6-astra` | $10.00 | $1.00 | $50.00 |
| `anthropic/claude-opus-5.5` | $4.00 | $0.20 | $20.00 |

These are ordinary API rates, not batch or subscription prices. The GPT projections stay below the 272,000-token higher-price tier. No explicit cache writes are modeled. Providers that charge for cache creation require those charges to be added. The retained snapshot includes their advertised write prices.

Input tokenization, cache reuse and output lengths on these models may differ from the Xiaomi model used in the measured experiment. Reasoning, model-driven tool selection, recovery turns, retries and account subscription costs are outside this calculation.

## Cache sensitivity

The table below assigns the same hypothetical cache-read fraction to both arms while retaining their different prompt sizes, fixed output and measured Jev charge. This separates the price effect of cache reuse from the measured arms' different hit rates.

| Main model | 0% cache reads | 75% cache reads | 95% cache reads |
| --- | ---: | ---: | ---: |
| GPT-6.1-Sol | 54.6% reduction | 44.2% reduction | 26.1% reduction |
| GPT-6-Astra | 55.8% reduction | 48.9% reduction | 39.6% reduction |
| Claude Opus 5.5 | 55.3% reduction | 46.4% reduction | 30.9% reduction |

A smaller prompt can still be more expensive if it loses a previously cached prefix. These sensitivity cases assume the specified cache fraction remains available; they do not model cache invalidation or prove cache behavior on these models.

## Reproduce the arithmetic

The estimate file contains one `simulation_input` object per model. Feed each object into the existing offline calculator:

```sh
python3 - <<'PY'
import json
import subprocess
import tempfile
from pathlib import Path

report = json.loads(Path('scripts/benchmarks/context-retrieval/jev-model-cost-estimates-2026-10-16.json').read_text())
with tempfile.TemporaryDirectory() as directory:
    for model in report['models']:
        source = Path(directory) / 'input.json'
        source.write_text(json.dumps(model['simulation_input']))
        print(model['model'], flush=True)
        subprocess.run([
            'python3', 'scripts/benchmarks/context-retrieval/cost_scenarios.py',
            '--input', str(source),
        ], check=True)
PY
```

This calculation makes no paid API calls.
