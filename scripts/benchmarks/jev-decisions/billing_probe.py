#!/usr/bin/env python3
# Modified for Distill by Samuel Fajreldines, 2026.
"""Checks how a decision model bills a battery: the same state with 1 and with
8 `noul` questions, on d1.2 and on Jev 1.14. A model that bills the state once
per request reports nearly the same input tokens for both. Paid: well under a
cent. Usage::

    OPENROUTER_API_KEY=... python3 billing_probe.py docs/jev-routing.md
"""
import json, os, sys, time, urllib.request
KEY = os.environ["OPENROUTER_API_KEY"]
def post(path, body):
    req = urllib.request.Request("https://openrouter.ai/api/alpha/" + path, data=json.dumps(body).encode(),
        headers={"Authorization": "Bearer " + KEY, "Content-Type": "application/json"})
    t = time.time()
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            out = json.loads(r.read())
    except urllib.error.HTTPError as e:
        out = {"http_error": e.code, "body": e.read().decode()[:800]}
    out["_latency_ms"] = int((time.time() - t) * 1000)
    return out
doc = open(sys.argv[1]).read()[:12000]
state = {"document": doc}
def q(i):
    return {"type": "noul", "instructions": f"Does the document mention topic number {i}: " + ["effort", "cache", "compaction", "worker model", "OpenRouter", "permissions", "UltraSave", "replay"][i % 8] + "?",
            "criteria": {"true": "It mentions it", "false": "It does not"}}
res = {}
for model in ["liquid/d1.2", "typesafe/jev-1.14"]:
    for n in [1, 8]:
        body = {"model": model, "state": state, "questions": {f"q{i}": q(i) for i in range(n)}}
        r = post("decisions", body)
        res[f"{model} n={n}"] = {k: r.get(k) for k in ["model", "usage", "_latency_ms", "http_error", "body"]}
print(json.dumps(res, indent=1))
