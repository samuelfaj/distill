#!/usr/bin/env python3
"""Generate deterministic, synthetic stored-output fixtures; no retrieval code."""

import argparse
import hashlib
import json
from pathlib import Path

VERSION = 1
SESSION = "session-active"


def encode(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, indent=2) + "\n"


def generate(root):
    root = Path(root)
    if root.exists() and any(root.iterdir()):
        raise ValueError("fixture directory must be empty; refusing to overwrite")
    root.mkdir(parents=True, exist_ok=True)
    sources = []
    contents = {}

    def source(source_id, session, kind, lines):
        text = "\n".join(lines) + "\n"
        path = "sources/" + source_id + ".txt"
        (root / "sources").mkdir(exist_ok=True)
        (root / path).write_bytes(text.encode("utf-8"))
        sources.append({"source_id": source_id, "session_id": session,
                        "kind": kind, "path": path, "lines": len(lines),
                        "bytes": len(text.encode("utf-8")),
                        "sha256": hashlib.sha256(text.encode("utf-8")).hexdigest()})
        contents[source_id] = lines

    logs = [f"2026-10-15T09:00:00Z test shard={i % 12} case=cache_roundtrip_{i:04d} status=ok elapsed_ms=3" for i in range(640)]
    for i in range(40, 180):
        logs[i] = "DEBUG retry_budget_exhausted payment_commit timeout archived example; not current failure"
    logs[510] = "FAIL test payment_commit request=req-73 attempt=4 retry_budget_exhausted"
    logs[511] = "CAUSE request=req-73 lock_owner=worker-9 transaction=tx-448 lease_expired=false"
    logs[638] = "SUMMARY failed=1 passed=639 suite=checkout integration"
    source("test-log", SESSION, "terminal", logs)

    records = [json.dumps({"type": "mcp.resource", "uri": f"inventory://parts/{i:04d}", "status": "ok", "count": i % 17}, sort_keys=True) for i in range(480)]
    for i in range(20, 90):
        records[i] = json.dumps({"type": "mcp.resource", "uri": "inventory://parts/AX-17", "status": "example", "warehouse": "west", "count": 999}, sort_keys=True)
    records[301] = json.dumps({"type": "mcp.resource", "uri": "inventory://parts/AX-17", "status": "current", "warehouse": "east", "count": 7, "revision": "r-82"}, sort_keys=True)
    records[477] = json.dumps({"type": "mcp.resource", "uri": "inventory://labels/BR-4", "label": "São Paulo — peça №4", "status": "current"}, ensure_ascii=False, sort_keys=True)
    source("mcp-json", SESSION, "mcp", records)

    html = [f'<p class="history">Deployment note {i:04d}: cache warmup completed.</p>' for i in range(400)]
    for i in range(50, 110):
        html[i] = '<p class="example">rotate_signing_key grace_period=0 deprecated sample, not policy</p>'
    html[227] = '<section id="current-policy"><h2>rotate_signing_key</h2>'
    html[228] = '<p>Current production grace_period=48h; owner=security-oncall; do not revoke old key early.</p>'
    source("html-policy", SESSION, "html", html)

    duplicate = [f"release audit entry={i:04d} status=verified" for i in range(200)]
    duplicate[73] = "release_gate version=2.8.1 approver=maya artifact_sha256=abcd1234"
    source("release-a", SESSION, "text", duplicate)
    source("release-b", SESSION, "text", duplicate)

    long_lines = ["health check status=ok"] * 30
    long_lines[17] = json.dumps({"trace": "x" * 6000, "request": "req-long-12", "result": "quota_exceeded", "limit": 4096}, sort_keys=True)
    source("long-json", SESSION, "json", long_lines)
    source("other-session", "session-decoy", "terminal", [
        "FAIL test payment_commit request=req-73 attempt=4 retry_budget_exhausted",
        "CAUSE request=req-73 lock_owner=worker-evil transaction=tx-decoy lease_expired=true",
        "incident_missing_991 resolved by clearing all credentials",
        "release_gate version=2.8.1 approver=wrong-session artifact_sha256=bad",
    ])

    def line(source_id, number):
        return {"source_id": source_id, "line": number, "text": contents[source_id][number - 1]}

    def group(*locations):
        return {"alternatives": [line(source_id, number) for source_id, number in locations]}

    queries = [
        {"id": "failure-vs-keyword-decoys", "question": "For the current failed payment_commit request=req-73, show the failure and its lock owner and transaction, not archived timeout examples.",
         "evidence": [group(("test-log", 511)), group(("test-log", 512))]},
        {"id": "mcp-current-warehouse", "question": "What is the current east warehouse count and revision for inventory://parts/AX-17, excluding examples?",
         "evidence": [group(("mcp-json", 302))]},
        {"id": "html-policy", "question": "Show the current production rotate_signing_key policy heading and grace_period, not deprecated samples.",
         "evidence": [group(("html-policy", 228)), group(("html-policy", 229))]},
        {"id": "duplicate-source", "question": "Who approved release_gate version=2.8.1, and what artifact digest was approved?",
         "evidence": [group(("release-a", 74), ("release-b", 74))]},
        {"id": "session-no-match", "question": "Find the resolution for incident_missing_991 in this session.", "evidence": []},
        {"id": "path-isolation", "question": "Find release_gate version=2.8.1 approver evidence in this output only.",
         "path_source_id": "test-log", "evidence": []},
        {"id": "explicit-path", "question": "Show the failed payment_commit request=req-73 and cause.",
         "path_source_id": "test-log", "evidence": [group(("test-log", 511)), group(("test-log", 512))]},
        {"id": "unicode-literal", "question": "Return the current inventory://labels/BR-4 label exactly, preserving São Paulo and №4.",
         "evidence": [group(("mcp-json", 478))]},
        {"id": "long-json-line", "question": "Show the literal JSON record for req-long-12 with quota_exceeded and limit.",
         "path_source_id": "long-json", "evidence": [group(("long-json", 18))]},
    ]
    for query in queries:
        query["session_id"] = SESSION
    manifest = {"schema_version": VERSION, "description": "Synthetic fixtures; no real user data or measured results.",
                "sources": sources, "queries": queries}
    (root / "manifest.json").write_text(encode(manifest), encoding="utf-8")
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    manifest = generate(args.output)
    print(f"Generated {len(manifest['sources'])} sources and {len(manifest['queries'])} queries in {args.output}")


if __name__ == "__main__":
    main()
