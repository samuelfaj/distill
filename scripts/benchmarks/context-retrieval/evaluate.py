#!/usr/bin/env python3
"""Score literal production retrieval via an external Rust adapter; offline unless explicitly authorized."""

import argparse
import hashlib
import json
import math
from pathlib import Path
import subprocess
import time

from cost_scenarios import simulate
from generate import VERSION, encode

UNSUPPORTED_REASON = "explicit-path long-line answer unsupported; use read_file or jq"
UNSUPPORTED_QUERY = "long-json-line"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def load_fixtures(root):
    root = Path(root).resolve()
    manifest = json.loads((root / "manifest.json").read_text(encoding="utf-8"))
    require(manifest["schema_version"] == VERSION, "unsupported fixture schema")
    sources = {}
    for source in manifest["sources"]:
        source_id = source["source_id"]
        require(source_id not in sources, "duplicate source id")
        path = (root / source["path"]).resolve()
        require(path.is_relative_to(root), "source escapes fixture directory")
        raw = path.read_bytes()
        require(hashlib.sha256(raw).hexdigest() == source["sha256"], "source hash mismatch")
        require(len(raw) == source["bytes"], "source size mismatch")
        text = raw.decode("utf-8")
        require(text.endswith("\n") and "\r" not in text, "fixtures require LF with final newline")
        lines = text[:-1].split("\n")
        require(len(lines) == source["lines"], "source line count mismatch")
        sources[source_id] = {**source, "path": str(path), "content": lines}
    ids = set()
    for query in manifest["queries"]:
        require(query["id"] not in ids, "duplicate query id")
        ids.add(query["id"])
        scoped_sources(query, sources)
        for group in query["evidence"]:
            require(bool(group["alternatives"]), "empty evidence group")
            for evidence in group["alternatives"]:
                source = sources[evidence["source_id"]]
                require(source["source_id"] in scoped_sources(query, sources), "gold outside query scope")
                number = evidence["line"]
                require(type(number) is int and 1 <= number <= source["lines"], "gold line out of bounds")
                require(source["content"][number - 1] == evidence["text"], "gold text mismatch")
    return manifest, sources


def scoped_sources(query, sources):
    candidates = {key: value for key, value in sources.items() if value["session_id"] == query["session_id"]}
    if "path_source_id" in query:
        key = query["path_source_id"]
        require(key in candidates, "query path outside session")
        return {key: candidates[key]}
    return candidates


def score(query, sources, response):
    require(response["schema_version"] == VERSION, "unsupported response schema")
    require(response["query_id"] == query["id"], "wrong response query")
    unsupported = response["status"] == "unsupported"
    require(response["status"] in ("ok", "no_match", "unsupported", "fallback"), "production retrieval failed")
    if unsupported:
        require(query["id"] == UNSUPPORTED_QUERY and response.get("unsupported_reason") == UNSUPPORTED_REASON,
                "undeclared unsupported signal")
        require(response["excerpts"] == [], "unsupported response contains evidence")
    require(isinstance(response["rendered_text"], str), "missing literal tool response")
    latency = response["retrieval_latency_ms"]
    require(type(latency) in (int, float) and math.isfinite(latency) and latency >= 0,
            "invalid retrieval latency")
    allowed = scoped_sources(query, sources)
    returned = set()
    for excerpt in response["excerpts"]:
        source_id = excerpt["source_id"]
        require(source_id in allowed, "returned source outside session/path scope")
        source = allowed[source_id]
        require(excerpt["session_id"] == source["session_id"], "wrong provenance session")
        require(excerpt["sha256"] == source["sha256"], "wrong provenance hash")
        start, end = excerpt["start_line"], excerpt["end_line"]
        require(type(start) is int and type(end) is int and 1 <= start <= end <= source["lines"],
                "invalid excerpt line range")
        expected = "\n".join(source["content"][start - 1:end]) + "\n"
        require(excerpt["text"] == expected, "excerpt is not literal source text")
        returned.update((source_id, number) for number in range(start, end + 1))
    require(unsupported or response["status"] == "fallback" or
            (response["status"] == "no_match") == (not returned), "status disagrees with excerpts")
    groups = [{(entry["source_id"], entry["line"]) for entry in group["alternatives"]}
              for group in query["evidence"]]
    relevant = set().union(*groups) if groups else set()
    hits = sum(bool(group & returned) for group in groups)
    true_lines = len(returned & relevant)
    raw_bytes = sum(source["bytes"] for source in allowed.values())
    unique_bytes = sum({source["sha256"]: source["bytes"] for source in allowed.values()}.values())
    returned_bytes = len(response["rendered_text"].encode("utf-8"))
    return {
        "query_id": query["id"], "status": response["status"],
        "unsupported_reason": response.get("unsupported_reason") if unsupported else None, "gold_groups": len(groups), "hit_groups": hits,
        "returned_unique_lines": len(returned), "relevant_returned_unique_lines": true_lines,
        "evidence_recall": hits / len(groups) if groups else None,
        "line_precision": true_lines / len(returned) if returned else (1.0 if not groups else 0.0),
        "no_match_correct": (not returned and not unsupported) if not groups else None,
        "raw_baseline_bytes": raw_bytes, "raw_unique_content_bytes": unique_bytes,
        "raw_baseline_token_estimate_bytes_div_4": raw_bytes / 4,
        "returned_bytes": returned_bytes,
        "returned_token_estimate_bytes_div_4": returned_bytes / 4,
        "returned_to_raw_byte_ratio": returned_bytes / raw_bytes if raw_bytes else None,
        "retrieval_latency_ms": latency,
    }


def aggregate(rows):
    gold = sum(row["gold_groups"] for row in rows)
    returned = sum(row["returned_unique_lines"] for row in rows)
    return {"query_count": len(rows),
            "supported_count": sum(row["status"] != "unsupported" for row in rows),
            "unsupported_count": sum(row["status"] == "unsupported" for row in rows),
            "micro_evidence_recall": sum(row["hit_groups"] for row in rows) / gold if gold else None,
            "micro_line_precision": sum(row["relevant_returned_unique_lines"] for row in rows) / returned if returned else None,
            "no_match_correct": sum(row["no_match_correct"] is True for row in rows),
            "no_match_queries": sum(row["no_match_correct"] is not None for row in rows)}


def run(root, adapter, output, mode, timeout, cost_config=None, allow_network=False):
    cost_simulation = simulate(cost_config) if cost_config is not None else None
    manifest, sources = load_fixtures(root)
    require(mode in ("lexical", "jev"), "invalid retrieval mode")
    require(not allow_network or mode == "jev", "network permission requires Jev mode")
    if allow_network:
        require(len(manifest["queries"]) == 9, "live run requires bounded nine-query corpus")
    require(not output.exists(), "report already exists; refusing to overwrite")
    require(adapter.is_file(), "production adapter missing; hookup not implemented here")
    rows, responses = [], []
    identity = None
    for query in manifest["queries"]:
        effective_mode = "lexical" if allow_network and "path_source_id" in query else mode
        query_network_allowed = allow_network and effective_mode == "jev"
        request = {"schema_version": VERSION, "query_id": query["id"], "session_id": query["session_id"],
                   "question": query["question"], "mode": effective_mode, "network_allowed": query_network_allowed,
                   "sources": [{key: value for key, value in source.items() if key != "content"}
                               for source in sources.values()]}
        if "path_source_id" in query:
            request["path_source_id"] = query["path_source_id"]
        started = time.perf_counter()
        result = subprocess.run([str(adapter)], input=json.dumps(request, ensure_ascii=False),
                                capture_output=True, text=True, encoding="utf-8", timeout=timeout)
        elapsed = (time.perf_counter() - started) * 1000
        legacy_unsupported = (query["id"] == UNSUPPORTED_QUERY and result.returncode == 1
                              and result.stdout == "" and result.stderr == f"Error: {UNSUPPORTED_REASON}\n")
        if legacy_unsupported:
            response = {"schema_version": VERSION, "query_id": query["id"], "status": "unsupported",
                        "unsupported_reason": UNSUPPORTED_REASON, "excerpts": [],
                        "rendered_text": result.stderr, "retrieval_latency_ms": elapsed,
                        "latency_scope": "adapter_wall_only", "selection": "unsupported",
                        "fallback": None, "legacy_unsupported_signal": True}
            row = score(query, sources, response)
            row["adapter_wall_latency_ms"] = elapsed
            row["retrieval_latency_ms"] = None
            row["response_envelope_bytes"] = 0
            row["selection"], row["fallback"] = "unsupported", None
            row["effective_mode"], row["network_used"], row["jev_usage"] = effective_mode, None, None
            rows.append(row)
            responses.append(response)
            continue
        require(result.returncode == 0, f"adapter failed for {query['id']}: {result.stderr[-2000:]}")
        response = json.loads(result.stdout)
        production = response["production"]
        require(production["implementation"] == "rust-production", "not production retrieval")
        require(isinstance(production["revision"], str) and production["revision"].strip(), "missing production revision")
        require(production["mode"] == effective_mode, "wrong production mode")
        require(type(production["network_used"]) is bool, "missing actual network use")
        require(query_network_allowed or production["network_used"] is False, "network use forbidden in this runner")
        if not query_network_allowed:
            require(response.get("jev_usage") is None, "offline run cannot claim live Jev costs")
        if response.get("jev_usage") is not None:
            require(isinstance(response["jev_usage"], dict), "Jev usage must retain actual provider object")
        stable_identity = {key: value for key, value in production.items() if key not in ("network_used", "selection", "mode")}
        if identity is None:
            identity = stable_identity
        require(identity == stable_identity, "production identity changed during run")
        selection = response.get("selection", production.get("selection"))
        fallback = response.get("fallback")
        if fallback is None and isinstance(selection, str) and (response["status"] == "fallback" or "fallback" in selection):
            fallback = selection
        if allow_network:
            require(isinstance(selection, str) and selection.strip(), "live response needs actual selection")
            require(fallback is None or isinstance(fallback, str), "invalid actual fallback")
            require(response["status"] != "fallback" or fallback, "fallback response needs actual reason")
        row = score(query, sources, response)
        row["selection"] = selection
        row["fallback"] = fallback
        row["effective_mode"] = effective_mode
        row["network_used"] = production["network_used"]
        row["jev_usage"] = response.get("jev_usage")
        row["adapter_wall_latency_ms"] = elapsed
        row["response_envelope_bytes"] = len(result.stdout.encode("utf-8"))
        rows.append(row)
        responses.append(response)
    report = {"schema_version": VERSION, "production": identity,
              "mode": mode, "network_allowed": allow_network,
              "run_kind": "live-jev" if allow_network else "offline",
              "fixture_manifest_sha256": hashlib.sha256((Path(root) / "manifest.json").read_bytes()).hexdigest(),
              "token_estimate_method": "UTF-8 bytes / 4; not tokenizer usage or billing",
              "jev_usage": None, "observed_provider_usage": None,
              "observed_provider_cost_usd": None, "observed_cache_hit_rate": None,
              "cache_cost_simulation": cost_simulation, "queries": rows, "responses": responses,
              "aggregate": aggregate(rows),
              "supported_only_aggregate": aggregate([row for row in rows if row["status"] != "unsupported"]) }
    with output.open("x", encoding="utf-8") as handle:
        handle.write(encode(report))
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixtures", type=Path, required=True)
    parser.add_argument("--adapter", type=Path, required=True, help="executable invoking actual Rust production retrieval")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--mode", choices=("lexical", "jev"), default="lexical")
    parser.add_argument("--timeout-seconds", type=float, default=30)
    parser.add_argument("--allow-network", action="store_true", help="authorize bounded live Jev calls; may incur charges")
    parser.add_argument("--cost-scenarios", type=Path, help="optional explicit hypothetical cache-cost input JSON")
    args = parser.parse_args()
    require(math.isfinite(args.timeout_seconds) and args.timeout_seconds > 0, "timeout must be positive and finite")
    cost_config = json.loads(args.cost_scenarios.read_text(encoding="utf-8")) if args.cost_scenarios else None
    report = run(args.fixtures, args.adapter.resolve(), args.output, args.mode, args.timeout_seconds, cost_config, args.allow_network)
    print(encode(report["aggregate"]), end="")


if __name__ == "__main__":
    main()
