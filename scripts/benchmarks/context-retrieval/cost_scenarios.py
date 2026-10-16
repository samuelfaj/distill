#!/usr/bin/env python3
"""Simulate explicit cache-aware costs, never provider usage or observed savings."""

import argparse
import json
import math
from pathlib import Path

from generate import encode

BUCKETS = ("uncached_input", "cache_read", "cache_write", "output")


def number(value, name):
    if type(value) not in (int, float) or not math.isfinite(value) or value < 0:
        raise ValueError(f"{name} must be finite and nonnegative")
    return value


def simulate(config):
    if config.get("schema_version") != 1:
        raise ValueError("unsupported cost scenario schema")
    if not isinstance(config.get("assumptions"), str) or not config["assumptions"].strip():
        raise ValueError("explicit scenario assumptions required")
    rates = config.get("rates_usd_per_million_tokens", {})
    if set(rates) != set(BUCKETS):
        raise ValueError("explicit rates required for all four token buckets")
    for bucket, value in rates.items():
        if value is None and bucket == "cache_write":
            continue
        number(value, bucket + " rate")
    rows = []
    names = set()
    for scenario in config.get("scenarios", []):
        name = scenario.get("name")
        if not isinstance(name, str) or not name.strip() or name in names:
            raise ValueError("scenario names must be nonempty and unique")
        names.add(name)
        tokens = scenario.get("estimated_tokens", {})
        if set(tokens) != set(BUCKETS):
            raise ValueError("explicit estimated tokens required for all four buckets")
        components = {}
        for bucket in BUCKETS:
            count = number(tokens[bucket], bucket + " estimated tokens")
            rate = rates[bucket]
            if rate is None and count:
                raise ValueError("cache_write rate required for nonzero cache writes")
            components[bucket] = number(count * (rate or 0) / 1_000_000, bucket + " cost")
        auxiliary = number(scenario.get("estimated_auxiliary_jev_usd"), "estimated auxiliary Jev USD")
        components["auxiliary_jev"] = auxiliary
        rows.append({"name": name, "estimated_tokens": dict(tokens),
                     "estimated_input_tokens": number(sum(tokens[bucket] for bucket in BUCKETS if bucket != "output"), "estimated input tokens"),
                     "estimated_cost_components_usd": components,
                     "estimated_total_usd": number(sum(components.values()), "estimated total")})
    if len(rows) < 2 or config.get("baseline") not in names:
        raise ValueError("at least two scenarios and a named baseline required")
    baseline = next(row for row in rows if row["name"] == config["baseline"])
    for row in rows:
        row["simulated_baseline_minus_scenario_usd"] = baseline["estimated_total_usd"] - row["estimated_total_usd"]
        discount = rates["uncached_input"] - rates["cache_read"]
        delta = row["estimated_total_usd"] - baseline["estimated_total_usd"]
        if delta <= 0:
            required, feasible = 0.0, True
        elif discount > 0:
            required = number(delta * 1_000_000 / discount, "break-even cache-read tokens")
            feasible = required <= row["estimated_tokens"]["uncached_input"]
        else:
            required, feasible = None, False
        row["break_even_additional_cache_read_tokens"] = required
        row["break_even_feasible_by_reclassifying_uncached_tokens"] = feasible
    return {"schema_version": 1, "kind": "cache_cost_simulation_not_observed_savings",
            "assumptions": config["assumptions"], "rates_usd_per_million_tokens": dict(rates),
            "baseline": config["baseline"], "scenarios": rows,
            "observed_provider_usage": None, "observed_provider_cost_usd": None,
            "observed_jev_usage": None, "observed_cache_hit_rate": None}


def example():
    def scenario(name, uncached, reads, writes, auxiliary):
        return {"name": name, "estimated_tokens": {
            "uncached_input": uncached, "cache_read": reads,
            "cache_write": writes, "output": 1000},
            "estimated_auxiliary_jev_usd": auxiliary}
    return {"schema_version": 1,
            "assumptions": "Illustrative invented rates, not provider pricing. Single-call estimates include full prompt; output fixed at 1000 tokens. Preserved prefix reuses 90000 tokens; invalidation loses it. Cache-write rate is all-in, not an additive surcharge. Jev amounts are hypothetical, not measured.",
            "rates_usd_per_million_tokens": {
                "uncached_input": 2.0, "cache_read": 0.2, "cache_write": 2.5, "output": 8.0},
            "baseline": "raw_cached",
            "scenarios": [scenario("raw_cached", 10000, 90000, 0, 0),
                          scenario("reduced_preserved_prefix", 2000, 90000, 0, 0.001),
                          scenario("reduced_cache_invalidated", 20000, 0, 0, 0.001),
                          scenario("reduced_cache_rebuilt", 2000, 0, 18000, 0.001)]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--example", action="store_true", help="print editable hypothetical input JSON, not results")
    source.add_argument("--input", type=Path, help="simulate explicit scenario input JSON")
    args = parser.parse_args()
    result = example() if args.example else simulate(json.loads(args.input.read_text(encoding="utf-8")))
    print(encode(result), end="")


if __name__ == "__main__":
    main()
