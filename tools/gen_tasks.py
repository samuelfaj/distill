#!/usr/bin/env python3
# Modified for Distill by Samuel Fajreldines, 2026.
"""Emit the Rust task table for the harness cheap-task registry.

Every catalogue function that a text model can serve becomes one row:
id, kind, instruction, guard. The kind decides the instruction's closing line and
the guard that must pass before the answer is used.
"""

import json
import pathlib
import sys

CATALOG = pathlib.Path(
    "/Users/samuelfajreldines/dev/Distill/plan-new-llm/function-catalog.json"
)

HOST_BOUND = {
    "screenshot_ocr", "video_keyframe_ocr", "screenshot_diff_digest", "doc_text_extract",
    "spreadsheet_extract", "audio_transcribe_local", "lsp_query", "ast_grep_query",
    "path_wrappers", "graphify_query", "batch_codemod_apply", "unchanged_reread_stub",
    "predicate_watch", "terminal_scrollback_intake", "attachment_handle_intake",
    "stale_tool_result_evict", "stale_tool_args_evict", "pbxproj_digest",
    "codesign_notarize_digest",
}
LOCAL_MODEL = {
    "encoder_rank", "semantic_cache_lookup", "workspace_semantic_search",
    "session_transcript_search", "history_recall_search", "semantic_dedup",
    "repeated_failure_notice", "nli_gate", "nli_claim", "metric_delta",
    "correction_exemplar_store",
}
HOST_AUTHORITY = {"provider_tier_suggest", "thread_title_generate"}

DETERMINISTIC = {
    "retrieve", "stats", "search_store", "json_crusher", "log_crusher", "stack_crusher",
    "test_crusher", "diff_crusher", "html_crusher", "source_skeleton", "toon_codec",
    "notebook_crusher", "lockfile_crusher", "generated_asset_notice",
    "embedded_blob_crusher", "progress_bar_crusher", "secret_redact_view", "retrieve_range",
    "handle_grep", "handle_query_eval", "handle_token_estimate", "identifier_alias_codec",
    "ansi_escape_strip", "padded_table_compact", "write_ack_verify", "stdout_budget_elide",
    "svg_crusher", "error_site_autoquote", "schema_validate_eval", "test_baseline_diff",
    "secret_presence_flag", "pii_presence_flag", "injection_pattern_flag", "compact_span",
    "duplicate_handle_notice", "repo_map_budget", "workspace_change_notice",
}

# id -> (kind, guard). Everything not listed keeps the kind its summary implies.
EXPLICIT = {
    "distill_command_output": ("Compress", "Literals"),
    "distill_long_text": ("Compress", "Literals"),
    "watch_summary": ("Compress", "Literals"),
    "subagent_brief_compact": ("Compress", "Literals"),
    "git_theme_summary": ("Compress", "Literals"),
    "docker_error_tail": ("Compress", "Literals"),
    "kubectl_digest": ("Compress", "Literals"),
    "gh_run_digest": ("Compress", "Literals"),
    "tree_listing_digest": ("Compress", "Literals"),
    "mcp_result_digest": ("Compress", "Literals"),
    "playwright_trace_digest": ("Compress", "Literals"),
    "xcodebuild_error_extract": ("Extract", "Literals"),
    "bundler_build_digest": ("Compress", "Literals"),
    "pkg_install_digest": ("Compress", "Literals"),
    "http_response_digest": ("Compress", "Literals"),
    "process_snapshot_digest": ("Compress", "Literals"),
    "sql_explain_digest": ("Compress", "Literals"),
    "snapshot_diff_digest": ("Compress", "Literals"),
    "test_verdict": ("Classify", "ClosedSet"),
    "flake_classify": ("Classify", "ClosedSet"),
    "file_role_classify": ("Classify", "ClosedSet"),
    "classify_closed": ("Classify", "ClosedSet"),
    "subagent_outcome_classify": ("Classify", "ClosedSet"),
    "command_intent": ("Classify", "ClosedSet"),
    "skill_name_pick": ("Pick", "CandidateIds"),
    "pick_candidates": ("Pick", "CandidateIds"),
    "chunk_select": ("Pick", "CandidateIds"),
    "grep_hits_rank": ("Pick", "CandidateIds"),
    "context_pack": ("Pick", "CandidateIds"),
    "mcp_schema_trim": ("Pick", "CandidateIds"),
    "openapi_digest": ("Pick", "CandidateIds"),
    "impacted_test_pick": ("Pick", "CandidateIds"),
    "cite_spans": ("Ask", "Spans"),
    "ask_handle": ("Ask", "Spans"),
    "multi_handle_ask": ("Ask", "Spans"),
    "nli_claim": ("Ask", "Spans"),
    "constraint_extract": ("Extract", "Literals"),
    "extract_schema": ("Extract", "JsonObject"),
    "json_shape": ("Extract", "JsonObject"),
    "table_extract": ("Extract", "JsonObject"),
    "log_records": ("Extract", "JsonObject"),
    "vuln_scan_digest": ("Extract", "JsonObject"),
    "entity_list": ("Extract", "Literals"),
    "metric_extract": ("Extract", "Literals"),
    "env_key_list": ("Extract", "Literals"),
    "compiler_diagnostics_extract": ("Extract", "Literals"),
    "log_timeline": ("Extract", "Literals"),
    "cluster_lines": ("Digest", "Literals"),
    "delta_handles": ("Digest", "Literals"),
    "outline_structure": ("Digest", "Literals"),
    "import_summary": ("Digest", "Literals"),
    "boilerplate_strip": ("Digest", "Literals"),
    "lint_group": ("Digest", "Literals"),
    "har_summary": ("Digest", "Literals"),
    "blame_digest": ("Digest", "Literals"),
    "merge_conflict_digest": ("Digest", "Literals"),
    "pr_thread_digest": ("Digest", "Literals"),
    "chat_thread_digest": ("Digest", "Literals"),
    "email_thread_digest": ("Digest", "Literals"),
    "meeting_transcript_digest": ("Digest", "Literals"),
    "multi_log_timeline_merge": ("Digest", "Literals"),
    "changelog_range_extract": ("Digest", "Literals"),
    "commit_message_draft": ("Digest", "Literals"),
    "pr_description_draft": ("Digest", "Literals"),
    "release_notes_draft": ("Digest", "Literals"),
    "plan_candidate": ("Digest", "Literals"),
    "review_comment_draft": ("Digest", "Literals"),
    "refactor_suggestion_list": ("Digest", "Literals"),
    "test_plan_draft": ("Digest", "Literals"),
    "root_cause_hypothesis_draft": ("Digest", "Literals"),
    "i18n_key_diff": ("Digest", "Literals"),
    "i18n_translation_draft": ("Digest", "Literals"),
    "issue_triage_draft": ("Digest", "Literals"),
    "sql_query_draft": ("Ask", "Literals"),
    "search_query_suggest": ("Ask", "Literals"),
    "patch_explain": ("Ask", "Literals"),
    "json_path_select": ("Ask", "Literals"),
    "filter_line_numbers": ("Ask", "Literals"),
    "map_error_to_files": ("Ask", "CandidateIds"),
    "help_flags_extract": ("Extract", "Literals"),
    "db_schema_digest": ("Extract", "Literals"),
    "tabular_digest": ("Extract", "Literals"),
    "dependency_graph_digest": ("Extract", "Literals"),
    "crash_report_digest": ("Extract", "Literals"),
    "sanitizer_report_digest": ("Extract", "Literals"),
    "syscall_trace_digest": ("Extract", "Literals"),
    "debugger_output_digest": ("Extract", "Literals"),
    "memory_report_digest": ("Extract", "Literals"),
    "binary_inspect_digest": ("Extract", "Literals"),
    "lighthouse_digest": ("Extract", "Literals"),
    "gitlab_api_digest": ("Extract", "Literals"),
    "coverage_summary": ("Extract", "Literals"),
    "terraform_plan_digest": ("Extract", "Literals"),
    "ci_job_failures": ("Extract", "Literals"),
    "linear_issue_digest": ("Extract", "Literals"),
    "graphql_error_extract": ("Extract", "Literals"),
    "profiler_hotspots": ("Extract", "Literals"),
    "ui_tree_digest": ("Extract", "Literals"),
    "wire_encode": ("Compress", "Literals"),
}

CLOSING = {
    "Compress": (
        "Answer with the compressed text only. Keep every command, path, "
        "file:line, number, identifier and error message verbatim; drop repetition "
        "and progress noise; aim for at most one third of the payload."
    ),
    "Digest": (
        "Answer with the digest only, in short lines: what changed or what matters, "
        "with the paths and identifiers quoted exactly as they appear."
    ),
    "Extract": (
        "Answer with the extracted items only, one per line, each quoted exactly as "
        "it appears in the payload."
    ),
    "Classify": "Answer with the single label only, exactly as one of the listed labels.",
    "Pick": (
        "Answer with the chosen ids only, one per line, and only ids that appear in "
        "the candidate list."
    ),
    "Ask": (
        "Answer with the smallest extract that answers the question, quoting the "
        "payload verbatim; do not paraphrase."
    ),
}


LABELS = {
    "test_verdict": ["PASS", "FAIL"],
    "flake_classify": ["flake", "consistent"],
    "file_role_classify": ["test", "impl", "config", "generated", "skill"],
    "subagent_outcome_classify": ["done", "partial", "failed", "off-task"],
    "classify_closed": [],
    "command_intent": [
        "cwdAndFiles", "cwdOnly", "tokenVerdict", "pathsOnly", "existsMissing",
        "statusCode", "jsonOnly",
    ],
}


def main():
    data = json.loads(CATALOG.read_text())
    rows = []
    for fn in data["functions"]:
        fid = fn["id"]
        if not fn["llm"]:
            continue
        if fn["safety"] == "forbidden" or fn["runtime"] == "35b-only":
            continue
        if fid in HOST_BOUND or fid in LOCAL_MODEL or fid in HOST_AUTHORITY:
            continue
        kind, guard = EXPLICIT.get(fid, ("Digest", "Literals"))
        summary = fn["summary"].replace("\\", "").replace('"', "'")
        instruction = f"{summary}. {CLOSING[kind]}"
        rows.append((fid, kind, guard, instruction))

    print("/// The catalogue's cheap tasks, one row per function a text model can serve.")
    print("///")
    print("/// Generated by `tools/gen_tasks.py` from the catalogue JSON, then reviewed:")
    print("/// the instruction is the catalogue's own summary plus the closing line its")
    print("/// kind needs, and the guard is what must pass before the answer is used.")
    print("pub const TASKS: &[TaskSpec] = &[")
    for fid, kind, guard, instruction in sorted(rows):
        if guard == "ClosedSet":
            labels = LABELS.get(fid, [])
            guard_rust = "Guard::ClosedSet(&[" + ", ".join(f'"{l}"' for l in labels) + "])"
        else:
            guard_rust = f"Guard::{guard}"
        print(f'    TaskSpec {{ id: "{fid}", kind: Kind::{kind}, guard: {guard_rust},')
        print(f'        instruction: "{instruction}" }},')
    print("];")
    print(f"// {len(rows)} tasks", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
