#!/usr/bin/env python3
"""Offline fixture/scorer checks, not production retrieval benchmark results."""

import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from cost_scenarios import example, simulate
from evaluate import UNSUPPORTED_REASON, load_fixtures, run, score
from generate import VERSION, generate


class FixtureChecks(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "fixtures"
        generate(self.root)
        self.manifest, self.sources = load_fixtures(self.root)

    def query(self, query_id):
        return next(query for query in self.manifest["queries"] if query["id"] == query_id)

    def response(self, query, locations):
        excerpts = []
        for source_id, start, end in locations:
            source = self.sources[source_id]
            excerpts.append({"source_id": source_id, "session_id": source["session_id"],
                             "sha256": source["sha256"], "start_line": start, "end_line": end,
                             "text": "\n".join(source["content"][start - 1:end]) + "\n"})
        return {"schema_version": VERSION, "query_id": query["id"],
                "status": "ok" if excerpts else "no_match", "excerpts": excerpts,
                "rendered_text": "".join(excerpt["text"] for excerpt in excerpts),
                "retrieval_latency_ms": 0}

    def test_generation_is_byte_identical_and_refuses_overwrite(self):
        second = Path(self.temp.name) / "second"
        generate(second)
        first_files = {str(path.relative_to(self.root)): path.read_bytes() for path in self.root.rglob("*") if path.is_file()}
        second_files = {str(path.relative_to(second)): path.read_bytes() for path in second.rglob("*") if path.is_file()}
        self.assertEqual(first_files, second_files)
        with self.assertRaisesRegex(ValueError, "refusing to overwrite"):
            generate(self.root)
        self.assertEqual(self.sources["release-a"]["sha256"], self.sources["release-b"]["sha256"])
        self.assertGreater(sum("retry_budget_exhausted" in line for line in self.sources["test-log"]["content"]), 100)

    def test_recall_and_context_precision(self):
        query = self.query("failure-vs-keyword-decoys")
        full = score(query, self.sources, self.response(query, [("test-log", 510, 512)]))
        self.assertEqual(full["evidence_recall"], 1)
        self.assertAlmostEqual(full["line_precision"], 2 / 3)
        partial = score(query, self.sources, self.response(query, [("test-log", 511, 511)]))
        self.assertEqual(partial["evidence_recall"], 0.5)
        self.assertEqual(partial["line_precision"], 1)
        self.assertLess(full["raw_unique_content_bytes"], full["raw_baseline_bytes"])

    def test_duplicate_alternatives_do_not_inflate_recall(self):
        query = self.query("duplicate-source")
        result = score(query, self.sources, self.response(query, [("release-a", 74, 74), ("release-b", 74, 74)]))
        self.assertEqual(result["hit_groups"], 1)
        self.assertEqual(result["evidence_recall"], 1)
        self.assertEqual(result["line_precision"], 1)

    def test_no_match_and_false_positive(self):
        query = self.query("session-no-match")
        empty = score(query, self.sources, self.response(query, []))
        self.assertTrue(empty["no_match_correct"])
        self.assertIsNone(empty["evidence_recall"])
        wrong = score(query, self.sources, self.response(query, [("test-log", 1, 1)]))
        self.assertFalse(wrong["no_match_correct"])
        self.assertEqual(wrong["line_precision"], 0)

    def test_session_and_explicit_path_boundaries(self):
        for query_id, location in [("session-no-match", ("other-session", 3, 3)),
                                   ("path-isolation", ("release-a", 74, 74))]:
            query = self.query(query_id)
            with self.assertRaisesRegex(ValueError, "outside session/path"):
                score(query, self.sources, self.response(query, [location]))

    def test_literal_unicode_long_lines_and_invalid_provenance(self):
        for query_id, location in [("unicode-literal", ("mcp-json", 478, 478)),
                                   ("long-json-line", ("long-json", 18, 18))]:
            query = self.query(query_id)
            response = self.response(query, [location])
            self.assertEqual(score(query, self.sources, response)["evidence_recall"], 1)
            response["excerpts"][0]["text"] += "invented"
            with self.assertRaisesRegex(ValueError, "not literal"):
                score(query, self.sources, response)
        query = self.query("explicit-path")
        for field, value in [("session_id", "wrong"), ("sha256", "bad"), ("start_line", 0)]:
            response = self.response(query, [("test-log", 511, 512)])
            response["excerpts"][0][field] = value
            with self.assertRaises(ValueError):
                score(query, self.sources, response)

    def test_simulation_attachment_does_not_change_adapter_contract(self):
        adapter = Path(self.temp.name) / "unit-test-adapter"
        adapter.touch()
        requests = []

        def unit_response(*args, **kwargs):
            request = json.loads(kwargs["input"])
            requests.append(request)
            query = self.query(request["query_id"])
            response = self.response(query, [])
            response["production"] = {"implementation": "rust-production", "revision": "unit-test-only",
                                      "mode": "lexical", "network_used": False}
            return subprocess.CompletedProcess(args[0], 0, stdout=json.dumps(response), stderr="")

        with patch("evaluate.subprocess.run", side_effect=unit_response):
            plain = run(self.root, adapter, Path(self.temp.name) / "plain.json", "lexical", 1)
            attached = run(self.root, adapter, Path(self.temp.name) / "attached.json", "lexical", 1, example())
        count = len(self.manifest["queries"])
        self.assertEqual(requests[:count], requests[count:])
        self.assertIsNone(plain["cache_cost_simulation"])
        self.assertEqual(attached["cache_cost_simulation"], simulate(example()))
        self.assertIsNone(attached["observed_provider_usage"])
        self.assertIsNone(attached["observed_provider_cost_usd"])
        self.assertIsNone(attached["observed_cache_hit_rate"])

    def test_invalid_cost_input_prevents_adapter_invocation(self):
        with patch("evaluate.subprocess.run") as invocation:
            with self.assertRaisesRegex(ValueError, "schema"):
                run(self.root, Path("unused"), Path(self.temp.name) / "none.json", "lexical", 1, {})
            invocation.assert_not_called()

    def partial_adapter(self, query, live=False):
        locations = [(group["alternatives"][0]["source_id"], group["alternatives"][0]["line"],
                      group["alternatives"][0]["line"]) for group in query["evidence"]]
        response = self.response(query, locations)
        response["rendered_text"] = "Full production header and provenance\n" + response["rendered_text"] + "Read original for more\n"
        response["production"] = {"implementation": "rust-production", "revision": "unit-test-only",
                                  "mode": "jev" if live else "lexical", "network_used": live and "path_source_id" not in query}
        response["selection"] = "Jev selection" if response["production"]["network_used"] else "explicit-path lexical fallback"
        response["fallback"] = None if response["production"]["network_used"] else "utility unavailable"
        return response

    def test_full_corpus_reports_partial_coverage_and_full_payload(self):
        adapter = Path(self.temp.name) / "unit-adapter"
        adapter.touch()

        def invoke(*args, **kwargs):
            request = json.loads(kwargs["input"])
            self.assertFalse(request["network_allowed"])
            if request["query_id"] == "long-json-line":
                return subprocess.CompletedProcess(args[0], 1, "", f"Error: {UNSUPPORTED_REASON}\n")
            response = self.partial_adapter(self.query(request["query_id"]))
            return subprocess.CompletedProcess(args[0], 0, json.dumps(response), "")

        with patch("evaluate.subprocess.run", side_effect=invoke) as calls:
            report = run(self.root, adapter, Path(self.temp.name) / "partial.json", "lexical", 1)
        self.assertEqual(calls.call_count, 9)
        self.assertEqual(len(report["responses"]), 9)
        self.assertEqual(report["aggregate"]["supported_count"], 8)
        self.assertEqual(report["aggregate"]["unsupported_count"], 1)
        self.assertAlmostEqual(report["aggregate"]["micro_evidence_recall"], 9 / 10)
        self.assertEqual(report["supported_only_aggregate"]["micro_evidence_recall"], 1)
        self.assertIsNone(report["queries"][-1]["retrieval_latency_ms"])
        full = report["responses"][0]["rendered_text"]
        self.assertEqual(report["queries"][0]["returned_bytes"], len(full.encode("utf-8")))
        self.assertGreater(report["queries"][0]["returned_bytes"],
                           sum(len(excerpt["text"].encode("utf-8")) for excerpt in report["responses"][0]["excerpts"]))

    def test_only_exact_declared_unsupported_signal_can_continue(self):
        query = self.query("long-json-line")
        response = self.response(query, [])
        response.update(status="unsupported", unsupported_reason=UNSUPPORTED_REASON)
        self.assertEqual(score(query, self.sources, response)["hit_groups"], 0)
        response["unsupported_reason"] += " changed"
        with self.assertRaisesRegex(ValueError, "undeclared unsupported"):
            score(query, self.sources, response)
        adapter = Path(self.temp.name) / "unit-adapter"
        adapter.touch()
        output = Path(self.temp.name) / "failed.json"
        for stderr in (f"Error: {UNSUPPORTED_REASON}\n", "Error: integrity failure\n"):
            with patch("evaluate.subprocess.run", return_value=subprocess.CompletedProcess([], 1, "", stderr)):
                with self.assertRaisesRegex(ValueError, "adapter failed"):
                    run(self.root, adapter, output, "lexical", 1)
            self.assertFalse(output.exists())

    def test_live_permission_and_per_query_fallback_without_network_calls(self):
        adapter = Path(self.temp.name) / "unit-adapter"
        adapter.touch()

        def invoke(*args, **kwargs):
            request = json.loads(kwargs["input"])
            query = self.query(request["query_id"])
            self.assertEqual(request["network_allowed"], "path_source_id" not in query)
            response = self.partial_adapter(query, live="path_source_id" not in query)
            response["production"]["selection"] = response.pop("selection")
            response.pop("fallback")
            if "path_source_id" not in query:
                response["status"] = "fallback"
                response["production"]["selection"] = "lexical fallback: Jev unavailable"
            if query["id"] == "long-json-line":
                response.update(status="unsupported", unsupported_reason=UNSUPPORTED_REASON, excerpts=[])
            return subprocess.CompletedProcess(args[0], 0, json.dumps(response), "")

        with patch("evaluate.subprocess.run", side_effect=invoke) as calls:
            report = run(self.root, adapter, Path(self.temp.name) / "live-unit-only.json", "jev", 1, allow_network=True)
        self.assertEqual(calls.call_count, 9)
        self.assertEqual(report["run_kind"], "live-jev")
        self.assertIsNone(report["observed_provider_usage"])
        self.assertIsNone(report["observed_provider_cost_usd"])
        self.assertTrue(any(row["fallback"] for row in report["queries"]))
        with patch("evaluate.subprocess.run") as calls:
            with self.assertRaisesRegex(ValueError, "requires Jev"):
                run(self.root, adapter, Path(self.temp.name) / "denied.json", "lexical", 1, allow_network=True)
            calls.assert_not_called()

    def test_source_mutation_and_missing_adapter_fail_closed(self):
        output = Path(self.temp.name) / "report.json"
        with self.assertRaisesRegex(ValueError, "adapter missing"):
            run(self.root, Path(self.temp.name) / "missing", output, "lexical", 1)
        self.assertFalse(output.exists())
        path = Path(self.sources["test-log"]["path"])
        path.write_bytes(path.read_bytes() + b"changed\n")
        with self.assertRaisesRegex(ValueError, "hash mismatch"):
            load_fixtures(self.root)


class CacheCostChecks(unittest.TestCase):
    def test_smaller_context_can_cost_more_when_cache_lost(self):
        report = simulate(example())
        raw, preserved, invalidated, rebuilt = report["scenarios"]
        self.assertLess(invalidated["estimated_input_tokens"], raw["estimated_input_tokens"])
        self.assertGreater(invalidated["estimated_total_usd"], raw["estimated_total_usd"])
        self.assertAlmostEqual(raw["estimated_total_usd"], 0.046)
        self.assertAlmostEqual(preserved["estimated_total_usd"], 0.031)
        self.assertAlmostEqual(invalidated["estimated_total_usd"], 0.049)
        self.assertAlmostEqual(rebuilt["estimated_total_usd"], 0.058)
        self.assertAlmostEqual(invalidated["simulated_baseline_minus_scenario_usd"], -0.003)

    def test_break_even_reclassifies_only_uncached_tokens(self):
        config = example()
        report = simulate(config)
        invalidated = report["scenarios"][2]
        threshold = invalidated["break_even_additional_cache_read_tokens"]
        self.assertAlmostEqual(threshold, 1666.6666666667)
        self.assertTrue(invalidated["break_even_feasible_by_reclassifying_uncached_tokens"])
        tokens = config["scenarios"][2]["estimated_tokens"]
        tokens["uncached_input"] -= threshold
        tokens["cache_read"] += threshold
        balanced = simulate(config)["scenarios"][2]
        self.assertAlmostEqual(balanced["simulated_baseline_minus_scenario_usd"], 0)
        self.assertEqual(balanced["estimated_input_tokens"], invalidated["estimated_input_tokens"])

    def test_output_writes_and_auxiliary_cost_are_not_omitted(self):
        config = example()
        rebuilt = simulate(config)["scenarios"][3]
        self.assertAlmostEqual(rebuilt["estimated_cost_components_usd"]["cache_write"], 0.045)
        self.assertAlmostEqual(rebuilt["estimated_cost_components_usd"]["output"], 0.008)
        self.assertAlmostEqual(rebuilt["estimated_cost_components_usd"]["auxiliary_jev"], 0.001)
        config["scenarios"][2]["estimated_auxiliary_jev_usd"] = 1
        impossible = simulate(config)["scenarios"][2]
        self.assertFalse(impossible["break_even_feasible_by_reclassifying_uncached_tokens"])

    def test_unavailable_write_rate_and_no_cache_discount(self):
        config = example()
        config["rates_usd_per_million_tokens"]["cache_write"] = None
        with self.assertRaisesRegex(ValueError, "nonzero cache writes"):
            simulate(config)
        config["scenarios"].pop()
        config["rates_usd_per_million_tokens"]["cache_read"] = 2
        config["scenarios"][2]["estimated_auxiliary_jev_usd"] = 1
        row = simulate(config)["scenarios"][2]
        self.assertIsNone(row["break_even_additional_cache_read_tokens"])
        self.assertFalse(row["break_even_feasible_by_reclassifying_uncached_tokens"])

    def test_observed_usage_stays_null_and_no_hit_rate_is_invented(self):
        report = simulate(example())
        self.assertEqual(report["kind"], "cache_cost_simulation_not_observed_savings")
        for key in ("observed_provider_usage", "observed_provider_cost_usd",
                    "observed_jev_usage", "observed_cache_hit_rate"):
            self.assertIsNone(report[key])

    def test_invalid_or_missing_assumptions_fail_loud(self):
        for value in (-1, float("nan"), float("inf"), True, "100", None):
            config = example()
            config["scenarios"][0]["estimated_tokens"]["cache_read"] = value
            with self.assertRaises(ValueError):
                simulate(config)
            config = example()
            config["rates_usd_per_million_tokens"]["output"] = value
            with self.assertRaises(ValueError):
                simulate(config)
        for field in ("assumptions", "baseline", "rates_usd_per_million_tokens"):
            config = example()
            del config[field]
            with self.assertRaises(ValueError):
                simulate(config)
        config = example()
        del config["scenarios"][0]["estimated_auxiliary_jev_usd"]
        with self.assertRaises(ValueError):
            simulate(config)


class LiveReplayChecks(unittest.TestCase):
    def test_arm_and_trial_prefixes_isolate_cache_before_shared_history(self):
        from live import ARMS, prefix
        values = [prefix("fixed", trial, "task", arm) for trial in range(2) for arm in ARMS]
        self.assertEqual(len(set(value.splitlines()[0] for value in values)), 8)
        self.assertEqual(prefix("fixed", 0, "task", "local"), prefix("fixed", 0, "task", "local"))

    def test_literal_quality_detects_missing_heading_and_unicode_corruption(self):
        from live import quality
        query = {"evidence": [{"alternatives": [{"text": "heading"}]},
                              {"alternatives": [{"text": "São Paulo №4"}]}]}
        self.assertTrue(quality(query, "heading\nSão Paulo №4")["all_evidence"])
        self.assertEqual(quality(query, "Sao Paulo No4")["hit_groups"], 0)
        self.assertFalse(quality(query, "São Paulo №4")["all_evidence"])
        self.assertTrue(quality(query, "MISSING\nheading\nSão Paulo №4")["declares_missing"])
        self.assertTrue(quality(query, "Evidence shows grace_period but no heading")["declares_missing"])

    def test_task_quality_accepts_exact_label_without_json_envelope(self):
        from live import task_quality
        self.assertTrue(task_quality("unicode-literal", "São Paulo — peça №4")["all_facts"])
        self.assertFalse(task_quality("unicode-literal", "Sao Paulo — peca No4")["all_facts"])
        self.assertFalse(task_quality("failure-vs-keyword-decoys", "lock_owner=worker-9 transaction=tx-448")["all_facts"])
        self.assertFalse(task_quality("html-policy", "grace_period=48h")["all_facts"])

    def test_prompt_bound_counts_decoded_content_not_json_escapes(self):
        from live import prompt_bytes
        history = [{"role": "user", "content": '"\\' * 2000}]
        self.assertEqual(prompt_bytes(history), 4512)
        self.assertGreater(len(json.dumps(history).encode()), prompt_bytes(history))

    def test_missing_billing_does_not_become_free(self):
        from live import cost, total_known
        self.assertIsNone(cost({}, {"data": None}))
        self.assertIsNone(total_known([0.1, None]))
        self.assertEqual(cost({}, {"data": {"total_cost": 0.2}}), 0.2)
        self.assertEqual(cost({"cost": 0}, {"data": {"total_cost": 0.2}}), 0)

    def test_generation_missing_id_makes_no_network_call(self):
        from live import generation
        with patch("live.http") as network:
            self.assertIsNone(generation("not-a-key", None)["data"])
            network.assert_not_called()

    def test_mocked_replay_accounts_all_arms_and_preserves_warm_history(self):
        from argparse import Namespace
        from types import SimpleNamespace
        import live
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            generate(root / "fixtures")
            manifest, sources = load_fixtures(root / "fixtures")
            (root / ".distill").mkdir()
            (root / ".distill/openrouter-auth.json").write_text(json.dumps({"key": "test-only"}))
            adapter = root / "adapter"
            adapter.write_bytes(b"test-only")
            requests = []

            def fake_http(path, key=None, payload=None):
                if path == "key":
                    return {"data": {"usage": 0.5}}
                if path.endswith("/endpoints"):
                    return {"data": {"endpoints": [{"tag": live.PROVIDER, "supports_implicit_caching": True,
                                                     "pricing": {"prompt": "0.00000014", "completion": "0.00000028"}}]}}
                requests.append(json.loads(json.dumps(payload)))
                query = next(query for query in manifest["queries"]
                             if query["question"] in payload["messages"][-1]["content"])
                answer = "\n".join(group["alternatives"][0]["text"] for group in query["evidence"])
                return {"id": "test-only", "choices": [{"message": {"content": answer}, "finish_reason": "stop"}],
                        "usage": {"cost": 0.001, "prompt_tokens": 100, "completion_tokens": 10,
                                  "prompt_tokens_details": {"cached_tokens": 40}}}

            def fake_adapter(command, **kwargs):
                request = json.loads(kwargs["input"])
                query = next(query for query in manifest["queries"] if query["id"] == request["query_id"])
                excerpts = []
                for group in query["evidence"]:
                    item = group["alternatives"][0]
                    source = sources[item["source_id"]]
                    excerpts.append({"source_id": item["source_id"], "session_id": query["session_id"],
                                     "sha256": source["sha256"], "start_line": item["line"],
                                     "end_line": item["line"], "text": item["text"] + "\n"})
                response = {"schema_version": 1, "query_id": query["id"], "status": "ok",
                            "retrieval_latency_ms": 1, "excerpts": excerpts,
                            "rendered_text": "".join(item["text"] for item in excerpts),
                            "production": {"network_used": request["network_allowed"]},
                            "jev_usage": {"request_id": "test-only"},
                            "jev_attempts": [{"billing": {"cost_usd_ticks": 2_000_000}}]}
                return SimpleNamespace(returncode=0, stdout=json.dumps(response))

            output = root / "report.json"
            with patch("live.Path.home", return_value=root), patch("live.http", side_effect=fake_http), \
                    patch("live.subprocess.run", side_effect=fake_adapter), \
                    patch("live.generation", return_value={"data": {"total_cost": 0.0002}}), patch("builtins.print"):
                live.run(Namespace(allow_network=True, fixtures=root / "fixtures", adapter=adapter, output=output))
            report = json.loads(output.read_text())
            self.assertEqual(report["status"], "completed")
            self.assertEqual(len(requests), 48)
            self.assertEqual(sorted(len(request["messages"]) for request in requests), [2] * 24 + [4] * 24)
            self.assertAlmostEqual(report["summary"]["jev"]["total_cost_usd"], 12 * 0.0012)
            self.assertEqual(report["summary"]["local"]["complete_answers"], 12)
            self.assertEqual(report["summary"]["baseline"]["cached_tokens"], 480)
            recovery = [row for row in report["rows"] if row["arm"] == "fallback_recovery"]
            self.assertEqual(len(recovery), 12)
            self.assertTrue(all(row["recovery"]["source_id"] == live.KNOWN_SOURCES[row["task"]] for row in recovery))
            self.assertTrue(all(request["provider"] == {"only": [live.PROVIDER], "allow_fallbacks": False} for request in requests))

            def unbilled_adapter(command, **kwargs):
                completed = fake_adapter(command, **kwargs)
                response = json.loads(completed.stdout)
                response["jev_attempts"] = [{"billing": {"cost_usd_ticks": None}}]
                return SimpleNamespace(returncode=0, stdout=json.dumps(response))

            main_calls_before_unknown = len(requests)
            with patch("live.Path.home", return_value=root), patch("live.http", side_effect=fake_http), \
                    patch("live.subprocess.run", side_effect=unbilled_adapter) as unknown_adapter, \
                    patch("live.generation", return_value={"data": {"total_cost": 0.0002}}), patch("builtins.print"):
                unknown_output = root / "unknown-report.json"
                with self.assertRaisesRegex(ValueError, "unknown auxiliary charge"):
                    live.run(Namespace(allow_network=True, fixtures=root / "fixtures", adapter=adapter, output=unknown_output))
            unknown = json.loads(unknown_output.read_text())
            self.assertEqual(unknown["status"], "failed")
            self.assertEqual(len(unknown["rows"]), 4)
            self.assertEqual(unknown_adapter.call_count, 3)
            self.assertEqual(len(requests) - main_calls_before_unknown, 3)
            self.assertIsNone(unknown["summary"]["jev"]["total_cost_usd"])
            self.assertNotIn("main_cost_usd", unknown["rows"][-1])

    def test_auxiliary_zero_requires_confirmed_no_dispatch(self):
        from live import auxiliary_cost
        self.assertEqual(auxiliary_cost({"production": {"network_used": False}, "jev_attempts": []})[0], 0)
        for response in ({"production": {"network_used": False}},
                         {"production": {"network_used": True}, "jev_attempts": []},
                         {"jev_attempts": [{"billing": {"cost_usd_ticks": None}}]},
                         {"jev_attempts": [{"billing": {"cost_usd_ticks": -1}}]}):
            self.assertIsNone(auxiliary_cost(response)[0])

    def test_adapter_failures_preserve_billing_and_never_dispatch_main(self):
        from argparse import Namespace
        from types import SimpleNamespace
        import live
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            generate(root / "fixtures")
            (root / ".distill").mkdir()
            (root / ".distill/openrouter-auth.json").write_text(json.dumps({"key": "test-only"}))
            adapter = root / "adapter"
            adapter.write_bytes(b"test-only")
            for scenario, expected_charge in (("timeout", None), ("nonzero_unknown", None),
                                               ("nonzero_billed", 0.0002), ("malformed", None),
                                               ("scoring", 0.0002), ("huge", 2.0)):
                with self.subTest(scenario=scenario):
                    main_calls = []

                    def fake_http(path, key=None, payload=None):
                        if path.endswith("/endpoints"):
                            return {"data": {"endpoints": [{"tag": live.PROVIDER, "supports_implicit_caching": True,
                                                             "pricing": {"prompt": "0.00000014", "completion": "0.00000028"}}]}}
                        if path == "key":
                            return {"data": {"usage": 0.5}}
                        main_calls.append(path)
                        raise AssertionError("paid main request must not dispatch")

                    def fake_adapter(command, **kwargs):
                        if scenario == "timeout":
                            raise subprocess.TimeoutExpired(command, 45)
                        if scenario == "malformed":
                            return SimpleNamespace(returncode=0, stdout="{")
                        ticks = None if expected_charge is None else round(expected_charge * 10_000_000_000)
                        response = {"production": {"network_used": True}, "rendered_text": "evidence",
                                    "jev_attempts": [{"billing": {"cost_usd_ticks": ticks}}]}
                        return SimpleNamespace(returncode=1 if scenario.startswith("nonzero") else 0,
                                               stdout=json.dumps(response))

                    output = root / (scenario + ".json")
                    with patch("live.Path.home", return_value=root), patch("live.ARMS", ("jev",)), \
                            patch("live.http", side_effect=fake_http), \
                            patch("live.subprocess.run", side_effect=fake_adapter) as adapter_call, \
                            patch("live.score", side_effect=ValueError("invalid excerpt") if scenario == "scoring" else None,
                                  return_value={}) as scoring, patch("builtins.print"):
                        with self.assertRaises((ValueError, subprocess.TimeoutExpired)):
                            live.run(Namespace(allow_network=True, fixtures=root / "fixtures", adapter=adapter, output=output))
                    report = json.loads(output.read_text())
                    self.assertEqual(report["status"], "failed")
                    self.assertEqual(adapter_call.call_count, 1)
                    self.assertEqual(main_calls, [])
                    self.assertEqual(report["rows"][0]["auxiliary_cost_usd"], expected_charge)
                    self.assertEqual(report["summary"]["jev"]["auxiliary_cost_usd"], expected_charge)
                    if scenario in ("nonzero_unknown", "nonzero_billed", "malformed", "timeout"):
                        scoring.assert_not_called()

    def test_live_requires_explicit_paid_authorization(self):
        from argparse import Namespace
        from live import run as live_run
        with patch("live.http") as network:
            with self.assertRaisesRegex(ValueError, "requires --allow-network"):
                live_run(Namespace(allow_network=False))
            network.assert_not_called()


class MeasuredReportChecks(unittest.TestCase):
    def test_completed_d1_report_has_full_pairing_and_known_auxiliary_accounting(self):
        import d1
        import live
        report = json.loads(Path(__file__).with_name("d1-completed-results-2026-10-16.json").read_text())
        self.assertEqual(report["status"], "completed")
        rows = report["rows"]
        expected = {(trial, task, turn, arm) for trial in range(2) for task in live.TASKS
                    for turn in range(2) for arm in d1.ARMS}
        self.assertEqual(len(rows), 36)
        self.assertEqual({(row["trial"], row["task"], row["turn"], row["arm"]) for row in rows}, expected)
        self.assertEqual(report["summary"], live.summary(rows, d1.ARMS))
        self.assertEqual(report["comparison_metrics"], d1.comparison_metrics(rows))
        self.assertAlmostEqual(report["measured_cost_usd"], sum(row["main_cost_usd"] + row["auxiliary_cost_usd"] for row in rows))
        reserve = report["selector_experiment"]["d1_auxiliary_byte_token_bound_usd"]
        endpoint = next(item for item in report["selector_experiment"]["endpoint_snapshot"]["endpoints"]
                        if item["tag"] == "liquid")
        self.assertAlmostEqual(reserve, 12 * d1.MAX_REQUEST_BYTES * d1.MAX_QUESTIONS * float(endpoint["pricing"]["prompt"]))
        self.assertAlmostEqual(report["preflight"]["auxiliary_reserve_usd"], live.AUX_RESERVE + reserve)
        self.assertLessEqual(report["preflight"]["estimated_main_plus_aux_reserve_usd"], 2)
        self.assertEqual(report["preflight"]["max_auxiliary_calls"], 24)
        for row in rows:
            self.assertEqual(row["response_provider"], "Xiaomi")
            self.assertEqual(row["main_cost_usd"], row["usage"]["cost"])
            self.assertIsInstance(row["usage"]["prompt_tokens_details"]["cached_tokens"], int)
            self.assertNotIn("error", row)
            if row["arm"] == "d1":
                result = row["retrieval"]["selector_response"]
                self.assertTrue(result["model"].startswith("liquid/d1-ultra"))
                self.assertEqual(row["auxiliary_cost_usd"], result["usage"]["cost"])
                self.assertNotIn("selector_error", row["retrieval"])
            elif row["arm"] == "jev":
                attempts = row["retrieval"]["jev_attempts"]
                self.assertEqual(len(attempts), 1)
                self.assertEqual(row["auxiliary_cost_usd"], attempts[0]["billing"]["cost_usd_ticks"] / 10_000_000_000)
            else:
                self.assertEqual(row["auxiliary_cost_usd"], 0)

    def test_completed_report_preserves_actual_billing_and_cache_counts(self):
        from live import summary
        report = json.loads(Path(__file__).with_name("live-warm-results-2026-10-16.json").read_text())
        self.assertEqual(report["status"], "completed")
        rows = report["rows"]
        self.assertEqual(len(rows), 48)
        self.assertEqual(len({(row["trial"], row["task"], row["turn"], row["arm"]) for row in rows}), 48)
        self.assertEqual(report["summary"], summary(rows))
        for row in rows:
            self.assertEqual(row["main_cost_usd"], row["usage"]["cost"])
            self.assertEqual(row["response_provider"], "Xiaomi")
            self.assertIsNone(row["accounting"]["data"])
            if row["turn"] == 0:
                self.assertEqual(row["usage"]["prompt_tokens_details"]["cached_tokens"], 0)
            if row["arm"] == "jev":
                attempts = row["retrieval"]["jev_attempts"]
                self.assertEqual(len(attempts), 1)
                self.assertEqual(row["auxiliary_cost_usd"], attempts[0]["billing"]["cost_usd_ticks"] / 10_000_000_000)


class D1Checks(unittest.TestCase):
    def test_existing_report_is_unchanged_and_never_enters_annotation_lifecycle(self):
        from argparse import Namespace
        import d1
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "existing.json"
            original = b'{"historical":true, "rows":[]}\n'
            output.write_bytes(original)
            with patch.object(d1.live, "http") as network, patch.object(d1.live, "run") as replay, \
                 patch.object(d1, "annotate") as annotate:
                with self.assertRaisesRegex(ValueError, "refusing to overwrite"):
                    d1.run(Namespace(allow_network=True, output=output))
            self.assertEqual(output.read_bytes(), original)
            network.assert_not_called()
            replay.assert_not_called()
            annotate.assert_not_called()

    def test_http_error_retains_only_safe_billing_and_never_applies_selection(self):
        import io
        import d1
        import live
        exported = {"selector_contract": {"state": {}, "questions": {"0": {}}}, "status": "fallback"}
        bodies = [
            ({"id": "gen-dec-test123", "usage": {"cost": 0.003},
              "error": {"message": "secret-provider-error"}, "answers": {"0": {"choice": "include"}}}, 0.003, "gen-dec-test123"),
            ({"id": "secret-provider-error", "usage": {"cost": -1}}, None, None),
            ({"id": "gen-test", "usage": {"cost": True}}, None, "gen-test"),
            ({"usage": {"cost": float("nan")}}, None, None),
            ({"usage": "secret-provider-error"}, None, None),
            ([], None, None),
        ]
        for body, charge, request_id in bodies:
            with self.subTest(body=body):
                error = d1.urllib.error.HTTPError(d1.ENDPOINT, 502, "secret-provider-error", {},
                                                 io.BytesIO(json.dumps(body).encode()))
                with patch.object(d1, "adapter", return_value=subprocess.CompletedProcess([], 0, json.dumps(exported))) as adapter, \
                     patch.object(d1.urllib.request, "urlopen", side_effect=error):
                    result = json.loads(d1.select(None, {}, "not-a-real-key", {}).stdout)
                self.assertEqual(adapter.call_count, 1)
                self.assertEqual(result["status"], "fallback")
                self.assertEqual(result["selector_error"], "HTTPError")
                self.assertEqual(result["selector_http_status"], 502)
                self.assertEqual(live.auxiliary_cost(result)[0], charge)
                self.assertEqual(result["selector_billing"].get("request_id"), request_id)
                self.assertNotIn("secret-provider-error", json.dumps(result))
                self.assertNotIn("selector_response", result)
        for body in (b"not JSON secret-provider-error", b"x" * (d1.MAX_REQUEST_BYTES + 2)):
            error = d1.urllib.error.HTTPError(d1.ENDPOINT, 502, "secret-provider-error", {}, io.BytesIO(body))
            with patch.object(d1, "adapter", return_value=subprocess.CompletedProcess([], 0, json.dumps(exported))), \
                 patch.object(d1.urllib.request, "urlopen", side_effect=error):
                result = json.loads(d1.select(None, {}, "not-a-real-key", {}).stdout)
            self.assertIsNone(live.auxiliary_cost(result)[0])
            self.assertNotIn("secret-provider-error", json.dumps(result))

    def test_native_contract_and_literal_renderer_remain_at_benchmark_boundary(self):
        import d1
        contract = {"state": {"question": "needle", "candidates": [{"id": "0", "text": "São Paulo — peça №4"}]},
                    "questions": {"0": {"type": "choice", "instructions": "select", "criteria": {"include": "evidence", "exclude": "decoy"}}}}
        exported = {"selector_contract": contract, "production": {"network_used": False}, "jev_attempts": [],
                    "rendered_text": "fallback", "status": "fallback"}
        rendered = {**exported, "rendered_text": "1→São Paulo — peça №4", "status": "ok"}
        native_response = {"model": d1.MODEL, "answers": {"0": {"type": "choice", "choice": "include"}},
                           "usage": {"cost": 0.00001}}
        with patch.object(d1, "adapter", side_effect=[subprocess.CompletedProcess([], 0, json.dumps(exported)),
                                                       subprocess.CompletedProcess([], 0, json.dumps(rendered))]) as adapter, \
             patch.object(d1, "native", return_value=native_response) as native:
            result = json.loads(d1.select(None, {}, "not-a-real-key", {}).stdout)
        self.assertEqual(native.call_args.args[1], {"model": d1.MODEL, **contract})
        self.assertEqual(adapter.call_args_list[1].args[1]["selector_contract"], contract)
        self.assertEqual(result["rendered_text"], "1→São Paulo — peça №4")
        self.assertEqual(result["selector_billing"]["cost_usd"], 0.00001)

    def test_failed_native_dispatch_never_claims_zero_cost(self):
        import d1
        import live
        exported = {"selector_contract": {"state": {}, "questions": {"0": {}}},
                    "production": {"network_used": False}, "jev_attempts": [], "status": "fallback"}
        with patch.object(d1, "adapter", return_value=subprocess.CompletedProcess([], 0, json.dumps(exported))), \
             patch.object(d1, "native", side_effect=TimeoutError):
            result = json.loads(d1.select(None, {}, "not-a-real-key", {}).stdout)
        self.assertIsNone(live.auxiliary_cost(result)[0])
        self.assertEqual(result["selector_error"], "TimeoutError")

    def test_billed_rendering_failure_keeps_charge_and_rejects_invalid_billing(self):
        import d1
        import live
        exported = {"selector_contract": {"state": {}, "questions": {"0": {}}}, "status": "fallback"}
        with patch.object(d1, "adapter", side_effect=[subprocess.CompletedProcess([], 0, json.dumps(exported)),
                                                       subprocess.CompletedProcess([], 1, "")]), \
             patch.object(d1, "native", return_value={"answers": {}, "usage": {"cost": 0.03}}):
            result = json.loads(d1.select(None, {}, "not-a-real-key", {}).stdout)
        self.assertEqual(live.auxiliary_cost(result)[0], 0.03)
        self.assertEqual(result["status"], "fallback")
        for charge in (None, -1, True, float("nan"), float("inf")):
            self.assertIsNone(live.auxiliary_cost({"selector_billing": {"cost_usd": charge}})[0])

    def test_d1_unknown_and_over_budget_charges_stop_before_main_dispatch(self):
        from argparse import Namespace
        import live
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            generate(root / "fixtures")
            (root / ".distill").mkdir()
            (root / ".distill/openrouter-auth.json").write_text(json.dumps({"key": "test-only"}))
            adapter = root / "adapter"
            adapter.write_bytes(b"test-only")
            for charge in (None, 2.0):
                output = root / (str(charge) + ".json")

                def fake_http(path, key=None, payload=None):
                    if path.endswith("/endpoints"):
                        return {"data": {"endpoints": [{"tag": live.PROVIDER, "supports_implicit_caching": True,
                            "pricing": {"prompt": "0.00000014", "completion": "0.00000028"}}]}}
                    if path == "key":
                        return {"data": {"usage": 0}}
                    self.fail("paid main request must not dispatch")

                response = {"selector_billing": {"cost_usd": charge}, "rendered_text": "evidence"}
                with patch("live.Path.home", return_value=root), patch("live.http", side_effect=fake_http), \
                     patch("live.score", return_value={}), patch("builtins.print"):
                    with self.assertRaisesRegex(ValueError, "unknown auxiliary charge|remaining budget insufficient"):
                        live.run(Namespace(allow_network=True, fixtures=root / "fixtures", adapter=adapter,
                                           output=output, selector_experiment={}),
                                 selector=lambda *args: subprocess.CompletedProcess([], 0, json.dumps(response)),
                                 arms=("d1",))
                report = json.loads(output.read_text())
                self.assertEqual(report["status"], "failed")
                self.assertEqual(report["rows"][0]["auxiliary_cost_usd"], charge)
                self.assertNotIn("main_cost_usd", report["rows"][0])

    def test_request_bound_rejects_before_network(self):
        import d1
        with patch.object(d1.urllib.request, "urlopen") as network:
            with self.assertRaisesRegex(ValueError, "ceiling"):
                d1.native("not-a-real-key", {"state": "x" * d1.MAX_REQUEST_BYTES})
            network.assert_not_called()


if __name__ == "__main__":
    unittest.main()
