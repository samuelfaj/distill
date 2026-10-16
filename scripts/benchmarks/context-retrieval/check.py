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


if __name__ == "__main__":
    unittest.main()
