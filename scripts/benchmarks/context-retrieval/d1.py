#!/usr/bin/env python3
"""Benchmark-only native d1 selector; production Jev routing stays unchanged."""

import argparse
import json
import math
import re
import statistics
import subprocess
import time
import urllib.error
import urllib.request
from pathlib import Path

import live
from evaluate import require

MODEL = "liquid/d1-ultra"
ENDPOINT = "https://openrouter.ai/api/alpha/decisions"
MAX_REQUEST_BYTES = 64 * 1024
MAX_QUESTIONS = 24
DEADLINE_SECONDS = 3
ARMS = ("local", "jev", "d1")


def native(key, payload):
    data = json.dumps(payload, ensure_ascii=False, separators=(",", ":")).encode()
    require(len(data) <= MAX_REQUEST_BYTES, "d1 serialized request exceeds production ceiling")
    request = urllib.request.Request(ENDPOINT, data=data, headers={
        "Content-Type": "application/json", "Authorization": "Bearer " + key})
    with urllib.request.urlopen(request, timeout=DEADLINE_SECONDS) as response:
        return json.load(response)


def adapter(args, request, env):
    return subprocess.run([str(args.adapter.resolve())], input=json.dumps(request),
                          capture_output=True, text=True, timeout=45, env=env)


def select(args, request, key, env):
    request = {**request, "mode": "selector", "network_allowed": False}
    exported = adapter(args, request, env)
    require(exported.returncode == 0, "production candidate export failed before dispatch")
    response = json.loads(exported.stdout)
    contract = response.get("selector_contract")
    require(isinstance(contract, dict) and contract.get("questions"), "missing production selector contract")
    require(len(contract["questions"]) <= MAX_QUESTIONS, "d1 question count exceeds production ceiling")
    payload = {"model": MODEL, **contract}
    require(len(json.dumps(payload, ensure_ascii=False, separators=(",", ":")).encode()) <= MAX_REQUEST_BYTES,
            "d1 serialized request exceeds production ceiling")
    response["selector_billing"] = {"cost_usd": None}
    started = time.perf_counter()
    try:
        result = native(key, payload)
        response["selector_response"] = result
        charge = (result.get("usage") or {}).get("cost")
        response["selector_billing"] = {"cost_usd": charge}
        require(isinstance(charge, (int, float)) and not isinstance(charge, bool)
                and math.isfinite(charge) and charge >= 0, "unknown d1 charge")
        require(time.perf_counter() - started <= DEADLINE_SECONDS, "d1 deadline exceeded")
        # Rust validates the unchanged contract and every choice before rendering.
        applied = adapter(args, {**request, "selector_contract": contract,
                                 "selector_answers": result.get("answers")}, env)
        require(applied.returncode == 0, "production selector application failed")
        rendered = json.loads(applied.stdout)
        require(rendered.get("selector_contract") == contract, "production contract changed")
        response.update({name: value for name, value in rendered.items()
                         if name not in ("selector_billing", "selector_response")})
    except urllib.error.HTTPError as error:
        response["selector_error"] = "HTTPError"
        response["selector_http_status"] = error.code
        response["status"] = "fallback"
        try:
            # Persist only validated accounting, never provider error text.
            body = json.loads(error.read(MAX_REQUEST_BYTES + 1))
            usage = body.get("usage") if isinstance(body, dict) else None
            charge = usage.get("cost") if isinstance(usage, dict) else None
            charge, _ = live.auxiliary_cost({"selector_billing": {"cost_usd": charge}})
            response["selector_billing"] = {"cost_usd": charge}
            request_id = body.get("id") if isinstance(body, dict) else None
            if isinstance(request_id, str) and re.fullmatch(r"gen[-A-Za-z0-9_]{1,200}", request_id):
                response["selector_billing"]["request_id"] = request_id
        except (OSError, ValueError):
            pass
        finally:
            error.close()
    except Exception as error:
        # Keep any known charge even when validation or rendering fails.
        response["selector_error"] = type(error).__name__
        response["status"] = "fallback"
    response["selector_wall_ms"] = (time.perf_counter() - started) * 1000
    response["selector_contract"] = contract
    response["selector_model"] = MODEL
    response["selector_endpoint"] = ENDPOINT
    return subprocess.CompletedProcess([], 0, json.dumps(response), "")


def comparison_metrics(rows):
    metrics = {}
    for arm in ARMS:
        selected = [row for row in rows if row["arm"] == arm]
        scores = [row["retrieval_score"] for row in selected if "retrieval_score" in row]
        gold = sum(item["gold_groups"] for item in scores)
        timings = {}
        for field in ("retrieval_wall_ms", "main_wall_ms"):
            values = [row[field] for row in selected if field in row]
            timings[field] = {"mean": statistics.mean(values), "median": statistics.median(values)} if values else None
        metrics[arm] = {
            "selection_recall": sum(item["hit_groups"] for item in scores) / gold if gold else None,
            "selection_hit_groups": sum(item["hit_groups"] for item in scores), "selection_gold_groups": gold,
            "fallbacks": sum(row.get("retrieval", {}).get("status") == "fallback" for row in selected),
            "selector_errors": sum("selector_error" in row.get("retrieval", {}) for row in selected),
            "invalid_outputs": sum("invalid" in row.get("retrieval", {}).get("production", {}).get("selection", "") for row in selected),
            "latency_ms": timings}
    return metrics


def annotate(output):
    report = json.loads(output.read_text())
    report["selector_experiment"]["contract"] = report["selector_experiment"]["contract"].replace("6-result cap", "8-result cap")
    report["comparison_metrics"] = comparison_metrics(report["rows"])
    report["comparison_metrics_by_turn"] = {str(turn): comparison_metrics([
        row for row in report["rows"] if row["turn"] == turn]) for turn in range(live.TURNS)}
    report["measured_cost_usd"] = live.total_known([row.get("main_cost_usd") for row in report["rows"]] +
                                                 [row.get("auxiliary_cost_usd") for row in report["rows"]])
    output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")


def run(args):
    require(args.allow_network, "paid run requires --allow-network")
    require(not args.output.exists(), "refusing to overwrite existing report")
    endpoints = live.http("models/" + MODEL + "/endpoints")["data"]
    endpoint = next(item for item in endpoints["endpoints"] if item["tag"] == "liquid")
    require(endpoints["architecture"]["output_modalities"] == ["decisions"], "d1 is not a native decision model")
    rates = endpoint["pricing"]
    require(not rates.get("overrides") and float(rates["completion"]) == 0, "unsupported d1 price schedule")
    auxiliary_bound = live.TRIALS * len(live.TASKS) * live.TURNS * MAX_REQUEST_BYTES * MAX_QUESTIONS * float(rates["prompt"])
    require(math.isfinite(auxiliary_bound) and auxiliary_bound >= 0, "invalid d1 price bound")
    args.selector_experiment = {
        "model": MODEL, "endpoint": ENDPOINT, "endpoint_snapshot": endpoints,
        "d1_auxiliary_byte_token_bound_usd": auxiliary_bound,
        "d1_billing_bound_basis": "64 KiB complete request bytes times 24 questions per call; native billing may charge shared state once per question",
        "contract": "Rust production shortlist, 24 KiB state, identical typed questions, include/exclude validation, 8-result cap and literal renderer",
        "differences": ["Benchmark-only two-pass export/application; d1 uses Python native HTTP, not JevClient.",
                        "Same native state/questions envelope; only model differs. No Chat Completions translation.",
                        "Three-second socket timeout plus elapsed deadline check; not a hard total-body cancellation timer.",
                        "d1 wall latency includes two adapter processes; selector_wall_ms excludes initial export.",
                        "Jev has an unverified allowance, not a server-side spend cap; unknown charges stop all paid calls."],
        "historical_control": "live-warm-results-2026-10-16.json: baseline and fallback_recovery are historical, not newly paired",
        "documentation": ["https://openrouter.ai/api/v1/models/liquid/d1-ultra/endpoints",
                          "https://openrouter.ai/docs/guides/community/jev-tutorial.md"]}
    try:
        live.run(args, selector=select, arms=ARMS, auxiliary_reserve=live.AUX_RESERVE + auxiliary_bound)
    finally:
        if args.output.exists():
            annotate(args.output)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixtures", type=Path, required=True)
    parser.add_argument("--adapter", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--allow-network", action="store_true")
    run(parser.parse_args())
