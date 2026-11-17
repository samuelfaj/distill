#!/usr/bin/env python3
# Modified for Distill by Samuel Fajreldines, 2026.
"""Compares liquid/d1.2 and liquid/d1.2-ultra on typed field extraction over
eight synthetic tool outputs (the kind `jev_decide` fields read).

Each field has one expected answer: a string or a number that must come back
with a span that reads the same characters from the state, `None` for a value
the state does not hold, or a list of substrings every one of which must be
inside some returned item. A value whose span does not read it back counts as
wrong, as the client drops it. Paid: well under a cent. Usage::

    OPENROUTER_API_KEY=... python3 extraction_probe.py --output extraction-<date>.json
"""

import argparse
import json
import os
import time
import urllib.error
import urllib.request
from pathlib import Path

ENDPOINT = "https://openrouter.ai/api/alpha/extractions"
MODELS = ["liquid/d1.2", "liquid/d1.2-ultra"]

T = lambda instructions: {"type": "text", "null_threshold": 0.5, "instructions": instructions}
N = lambda instructions: {"type": "number", "null_threshold": 0.5, "instructions": instructions}
L = lambda instructions: {"type": "list", "items": "text", "instructions": instructions}

CASES = [
    {"id": "x1-cargo-test", "state": """running 6 tests
test parser::tests::reads_empty_input ... ok
test parser::tests::rejects_bad_header ... FAILED
test cache::tests::evicts_oldest ... ok
test cache::tests::keeps_hot_entry ... FAILED

---- parser::tests::rejects_bad_header stdout ----
thread 'parser::tests::rejects_bad_header' panicked at src/parser.rs:212:9:
assertion `left == right` failed
  left: Ok(Header { version: 2 })
 right: Err(BadHeader)

---- cache::tests::keeps_hot_entry stdout ----
thread 'cache::tests::keeps_hot_entry' panicked at src/cache.rs:88:5:
hot entry was evicted after 3 inserts

test result: FAILED. 4 passed; 2 failed; 0 ignored; finished in 0.02s
""", "fields": {
        "first_panic": (T("The file:line:column of the first panic."), "src/parser.rs:212:9"),
        "failed": (N("How many tests failed, from the summary line."), 2),
        "deploy_url": (T("The URL the build was deployed to."), None),
        "causes": (L("Every line that says why a test failed: panic locations and assertion messages."),
                   ["src/parser.rs:212:9", "src/cache.rs:88:5", "hot entry was evicted after 3 inserts"]),
    }},
    {"id": "x2-pytest", "state": """============================= test session starts ==============================
collected 44 items

tests/test_api.py ..F.....                                               [ 18%]
tests/test_models.py ....................                                [ 63%]
tests/test_views.py .....F....F.....                                     [100%]

=================================== FAILURES ===================================
___________________________________ test_login _________________________________
    def test_login(client):
>       assert client.post("/login", json=creds).status_code == 200
E       assert 401 == 200
tests/test_api.py:31: AssertionError
=========================== short test summary info ============================
FAILED tests/test_api.py::test_login - assert 401 == 200
FAILED tests/test_views.py::test_profile - KeyError: 'avatar'
FAILED tests/test_views.py::test_logout - assert 302 == 200
========================= 3 failed, 41 passed in 2.31s =========================
""", "fields": {
        "failed": (N("How many tests failed."), 3),
        "passed": (N("How many tests passed."), 41),
        "key_error_test": (T("The test id of the test that failed with a KeyError."), "tests/test_views.py::test_profile"),
        "failed_tests": (L("The id of every failed test."),
                         ["tests/test_api.py::test_login", "tests/test_views.py::test_profile", "tests/test_views.py::test_logout"]),
    }},
    {"id": "x3-tsc", "state": """> web@1.4.2 build
> tsc && vite build

src/App.tsx(14,7): error TS2322: Type 'string' is not assignable to type 'number'.
src/hooks/useCart.ts(52,19): error TS2339: Property 'total' does not exist on type 'Cart'.

Found 2 errors in 2 files.
""", "fields": {
        "first_code": (T("The TypeScript error code of the first error."), "TS2322"),
        "second_file": (T("The file path of the second error."), "src/hooks/useCart.ts"),
        "errors": (N("How many errors were found."), 2),
        "warnings": (N("How many warnings were reported."), None),
    }},
    {"id": "x4-git-log", "state": """499aba66 chore: prepare v2.0.77
fdf2bd5b fix(turn): mark the answer when the final check sends the model back to work (#73)
45bcd206 fix(turn): send a text reply to the final check as part of the answer (#72)
3b6f48cb chore(release): bump version to 2.0.76
43a62203 feat(code-mode): enable JavaScript tool calls by default (#69)
""", "fields": {
        "bump_commit": (T("The hash of the commit that bumps the version to 2.0.76."), "3b6f48cb"),
        "pr_of_code_mode": (N("The pull request number of the code-mode commit."), 69),
        "revert_commit": (T("The hash of a commit that reverts another commit."), None),
    }},
    {"id": "x5-json", "state": """{"status":"degraded","checked_at":"2026-11-17T12:04:11Z","services":[{"name":"api","latency_ms":41,"ok":true},{"name":"db","latency_ms":812,"ok":false},{"name":"cache","latency_ms":3,"ok":true}]}
""", "fields": {
        "status": (T("The overall status."), "degraded"),
        "db_latency": (N("The latency of the db service in milliseconds."), 812),
        "failing": (L("The name of every service that is not ok."), ["db"]),
    }},
    {"id": "x6-traceback", "state": """Traceback (most recent call last):
  File "/app/worker.py", line 140, in run
    self.handle(job)
  File "/app/worker.py", line 88, in handle
    user = self.users[job["user_id"]]
KeyError: 'u_5512'
""", "fields": {
        "exception": (T("The exception type."), "KeyError"),
        "line": (N("The line number where the exception was raised."), 88),
        "missing_key": (T("The key that was missing."), "u_5512"),
    }},
    {"id": "x7-docker", "state": """#8 [4/6] RUN npm ci
#8 DONE 31.2s
#9 [5/6] COPY . .
#9 DONE 0.4s
#10 [6/6] RUN npm run build
#10 DONE 18.9s
#11 exporting to image
#11 writing image sha256:9f2c1e0b7d44 done
#11 naming to docker.io/library/app:1.4.2 done
""", "fields": {
        "tag": (T("The image name and tag that was built."), "docker.io/library/app:1.4.2"),
        "npm_ci_seconds": (N("How many seconds the npm ci step took."), 31.2),
        "push_digest": (T("The digest reported by docker push."), None),
    }},
    {"id": "x8-unicode", "state": """2026-11-17 09:12:01 INFO  fila=pedidos região=São Paulo-3 consumidores=4
2026-11-17 09:12:31 ERRO  falha ao conectar em São Paulo-3 (timeout 30s) — tentativa 2 de 5
2026-11-17 09:13:01 INFO  reconectado em São Paulo-1
""", "fields": {
        "failed_region": (T("The region the connection failed in."), "São Paulo-3"),
        "timeout": (N("The timeout in seconds."), 30),
        "recovered_region": (T("The region it reconnected in."), "São Paulo-1"),
    }},
]


def post(body):
    request = urllib.request.Request(ENDPOINT, data=json.dumps(body).encode(), headers={
        "Authorization": "Bearer " + os.environ["OPENROUTER_API_KEY"], "Content-Type": "application/json"})
    started = time.time()
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            out = json.loads(response.read())
    except urllib.error.HTTPError as error:
        out = {"http_error": error.code, "body": error.read().decode(errors="replace")[:300]}
    out["_latency_ms"] = int((time.time() - started) * 1000)
    return out


def reads_back(state, value, span):
    return isinstance(span, list) and len(span) == 2 and state[span[0]:span[1]] == value


def number_at(state, span):
    try:
        return float(state[span[0]:span[1]].replace(",", "").strip())
    except (TypeError, ValueError, IndexError):
        return None


def correct(state, expected, answer):
    value = answer.get("value")
    if expected is None:
        return value is None
    if isinstance(expected, list):
        items = [item for item, span in zip(value or [], answer.get("spans") or []) if reads_back(state, item, span)]
        return all(any(want in item for item in items) for want in expected)
    if isinstance(expected, str):
        return value == expected and reads_back(state, value, answer.get("span"))
    return value is not None and float(value) == float(expected) and number_at(state, answer.get("span")) == float(expected)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.output.exists():
        raise SystemExit(f"{args.output} exists")
    rows, summary = [], {}
    for model in MODELS:
        entry = summary.setdefault(model, {"fields": 0, "correct": 0, "cost": 0.0, "latency_ms": [], "errors": 0})
        for case in CASES:
            fields = {name: spec for name, (spec, _) in case["fields"].items()}
            response = post({"model": model, "state": case["state"], "fields": fields})
            answers = response.get("answers") or {}
            entry["errors"] += "http_error" in response
            entry["cost"] += (response.get("usage") or {}).get("cost") or 0.0
            entry["latency_ms"].append(response["_latency_ms"])
            for name, (_, expected) in case["fields"].items():
                ok = name in answers and correct(case["state"], expected, answers[name])
                entry["fields"] += 1
                entry["correct"] += ok
                rows.append({"model": model, "case": case["id"], "field": name, "expected": expected,
                             "ok": ok, "answer": answers.get(name), "error": response.get("http_error")})
        latencies = sorted(entry.pop("latency_ms"))
        entry["latency_median_ms"] = latencies[len(latencies) // 2]
        entry["latency_max_ms"] = latencies[-1]
        entry["cost"] = round(entry["cost"], 7)
    args.output.write_text(json.dumps({"schema_version": 1, "endpoint": ENDPOINT, "models": MODELS,
                                       "run_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                                       "summary": summary, "rows": rows}, indent=1, ensure_ascii=False) + "\n")
    for model, entry in summary.items():
        print(model, entry)
    for row in rows:
        if not row["ok"]:
            print("MISS", row["model"], row["case"], row["field"], "expected", row["expected"], "got",
                  json.dumps((row["answer"] or {}).get("value"), ensure_ascii=False)[:160], row["error"] or "")


if __name__ == "__main__":
    main()
