#!/usr/bin/env python3
# Modified for Distill by Samuel Fajreldines, 2026.
"""Replays the production selector contracts of the 2026-10-16 d1 report
against several decision models and scores selection recall offline.

The contracts (`retrieval.selector_contract` of the report's d1 rows) are the
exact state and include/exclude questions `stored_output_ask` builds; only the
model changes. A candidate covers an evidence group when its text holds one of
the group's gold lines (fixtures from `generate.py`). Selection keeps the
production rule: included candidates in lexical order, at most eight. No main
model is called. Paid: about a cent per run.

    OPENROUTER_API_KEY=... python3 selector_models.py --fixtures DIR --output OUT.json
"""

import argparse
import concurrent.futures
import json
import os
import time
import urllib.error
import urllib.request
from pathlib import Path

ENDPOINT = "https://openrouter.ai/api/alpha/decisions"
REPORT = Path(__file__).with_name("d1-completed-results-2026-10-16.json")
MODELS = ["typesafe/jev-1.14", "liquid/d1.2", "liquid/d1.2-ultra"]
CAP = 8


def post(model, contract):
    body = json.dumps({"model": model, **contract}).encode()
    request = urllib.request.Request(ENDPOINT, data=body, headers={
        "Authorization": "Bearer " + os.environ["OPENROUTER_API_KEY"], "Content-Type": "application/json"})
    started = time.time()
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            out = json.loads(response.read())
    except urllib.error.HTTPError as error:
        out = {"http_error": error.code}
    out["_latency_ms"] = int((time.time() - started) * 1000)
    return out


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--fixtures", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.output.exists():
        raise SystemExit(f"{args.output} exists")
    manifest = json.loads((args.fixtures / "manifest.json").read_text())
    sources = {source["source_id"]: source for source in manifest["sources"]}
    by_question = {query["question"]: query for query in manifest["queries"]}
    rows = [row for row in json.loads(REPORT.read_text())["rows"] if row["arm"] == "d1"]
    jobs = []
    for row in rows:
        contract = row["retrieval"]["selector_contract"]
        query = by_question[contract["state"]["question"]]
        groups = [[evidence["text"] for evidence in group["alternatives"]] for group in query["evidence"]]
        for model in MODELS:
            jobs.append((row, contract, groups, model))

    def run(job):
        row, contract, groups, model = job
        response = post(model, contract)
        answers = response.get("answers") or {}
        candidates = contract["state"]["candidates"]
        valid = all((answers.get(c["id"]) or {}).get("choice") in ("include", "exclude") for c in candidates)
        kept = [c for c in candidates if (answers.get(c["id"]) or {}).get("choice") == "include"][:CAP] if valid else []
        hit = sum(any(any(text in c["text"] for text in group) for c in kept) for group in groups)
        return {"task": row["task"], "trial": row["trial"], "turn": row["turn"], "model": model,
                "served": response.get("model"), "candidates": len(candidates), "kept": len(kept),
                "groups": len(groups), "hit_groups": hit, "valid": valid,
                "cost": (response.get("usage") or {}).get("cost"),
                "input_tokens": (response.get("usage") or {}).get("input_tokens"),
                "latency_ms": response["_latency_ms"], "error": response.get("http_error")}

    with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
        results = list(pool.map(run, jobs))
    summary = {}
    for result in results:
        entry = summary.setdefault(result["model"], {"queries": 0, "groups": 0, "hit_groups": 0, "kept": 0,
                                                     "invalid": 0, "cost": 0.0, "latency_ms": []})
        entry["queries"] += 1
        entry["groups"] += result["groups"]
        entry["hit_groups"] += result["hit_groups"]
        entry["kept"] += result["kept"]
        entry["invalid"] += not result["valid"]
        entry["cost"] += result["cost"] or 0.0
        entry["latency_ms"].append(result["latency_ms"])
    for entry in summary.values():
        latencies = sorted(entry.pop("latency_ms"))
        entry["latency_median_ms"] = latencies[len(latencies) // 2]
        entry["cost"] = round(entry["cost"], 7)
    args.output.write_text(json.dumps({"schema_version": 1, "source_report": REPORT.name, "models": MODELS,
                                       "run_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                                       "summary": summary, "rows": results}, indent=1) + "\n")
    for model, entry in summary.items():
        print(model, entry)


if __name__ == "__main__":
    main()
