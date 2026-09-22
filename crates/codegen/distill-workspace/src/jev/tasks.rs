//! The cheap-task registry: every catalogue function a text model can serve.
//!
//! One row per catalogue id (the rows `list.md` tracks), each saying what kind of
//! task it is, the instruction the cheap worker follows, and the **guard** its
//! answer must pass before the harness uses it. The guard is the whole point:
//! a cheap model is allowed to help only where its answer can be checked against
//! the payload it was given, so a hallucination is rejected rather than trusted.
//!
//! `run` is the single entry point: it builds the closed task, sends **one**
//! request, gates the answer, and returns `None` on anything else — which every
//! caller treats as "keep today's bytes".

use super::cheap::{CheapAnswer, CheapClient, CheapTask};
use super::crushers;
use super::reduce;

/// What shape of work the task is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Shrink the payload, keeping its literals.
    Compress,
    /// Say what matters in the payload, in short lines.
    Digest,
    /// Pull items out, verbatim.
    Extract,
    /// Pick one label from a closed set.
    Classify,
    /// Choose ids from a candidate list.
    Pick,
    /// Answer a question from the payload, quoting it.
    Ask,
}

/// What must pass before the answer is used at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    /// Every literal of the payload (paths, `file:line`, numbers, error words)
    /// must still appear in the answer, and the answer must be shorter.
    Literals,
    /// The answer must parse as a non-empty JSON object satisfying this
    /// declaration's required fields and types.
    JsonObject(&'static JsonContract),
    /// The answer must be exactly one of these labels (empty ⇒ the caller
    /// supplies them).
    ClosedSet(&'static [&'static str]),
    /// The answer must name at least one id, and every id must appear in the
    /// candidates the caller offered.
    CandidateIds,
    /// The answer must quote at least one span, and every quoted span must be a
    /// substring of the payload.
    Spans,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonFieldKind {
    NonEmptyString,
    StringOrNull,
    NonEmptyStringArray,
    NonEmptyObjectArray(&'static [JsonField]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct JsonField {
    name: &'static str,
    kind: JsonFieldKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonContract {
    fields: Option<&'static [JsonField]>,
}

const JSON_SHAPE_FIELDS: &[JsonField] = &[JsonField {
    name: "keys",
    kind: JsonFieldKind::NonEmptyStringArray,
}];

const TABLE_FIELDS: &[JsonField] = &[JsonField {
    name: "rows",
    kind: JsonFieldKind::NonEmptyObjectArray(&[]),
}];

const LOG_RECORD_FIELDS: &[JsonField] = &[
    JsonField {
        name: "ts",
        kind: JsonFieldKind::NonEmptyString,
    },
    JsonField {
        name: "level",
        kind: JsonFieldKind::NonEmptyString,
    },
    JsonField {
        name: "msg",
        kind: JsonFieldKind::NonEmptyString,
    },
    JsonField {
        name: "file",
        kind: JsonFieldKind::NonEmptyString,
    },
];

const LOG_RECORDS_FIELDS: &[JsonField] = &[JsonField {
    name: "records",
    kind: JsonFieldKind::NonEmptyObjectArray(LOG_RECORD_FIELDS),
}];

const VULNERABILITY_FIELDS: &[JsonField] = &[
    JsonField {
        name: "package",
        kind: JsonFieldKind::NonEmptyString,
    },
    JsonField {
        name: "severity",
        kind: JsonFieldKind::NonEmptyString,
    },
    JsonField {
        name: "fixedIn",
        kind: JsonFieldKind::StringOrNull,
    },
];

const VULNERABILITIES_FIELDS: &[JsonField] = &[JsonField {
    name: "vulnerabilities",
    kind: JsonFieldKind::NonEmptyObjectArray(VULNERABILITY_FIELDS),
}];

const EXTRACT_SCHEMA_CONTRACT: JsonContract = JsonContract { fields: None };
const JSON_SHAPE_CONTRACT: JsonContract = JsonContract {
    fields: Some(JSON_SHAPE_FIELDS),
};
const TABLE_EXTRACT_CONTRACT: JsonContract = JsonContract {
    fields: Some(TABLE_FIELDS),
};
const LOG_RECORDS_CONTRACT: JsonContract = JsonContract {
    fields: Some(LOG_RECORDS_FIELDS),
};
const VULN_SCAN_DIGEST_CONTRACT: JsonContract = JsonContract {
    fields: Some(VULNERABILITIES_FIELDS),
};

fn json_object_matches(value: &serde_json::Value, fields: &[JsonField]) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    !object.is_empty()
        && fields.iter().all(|field| {
            object
                .get(field.name)
                .is_some_and(|value| json_field_matches(value, field.kind))
        })
}

fn json_field_matches(value: &serde_json::Value, kind: JsonFieldKind) -> bool {
    match kind {
        JsonFieldKind::NonEmptyString => value
            .as_str()
            .is_some_and(|text| !text.trim().is_empty()),
        JsonFieldKind::StringOrNull => {
            value.is_null()
                || value
                    .as_str()
                    .is_some_and(|text| !text.trim().is_empty())
        }
        JsonFieldKind::NonEmptyStringArray => value.as_array().is_some_and(|items| {
            !items.is_empty()
                && items.iter().all(|item| {
                    item.as_str()
                        .is_some_and(|text| !text.trim().is_empty())
                })
        }),
        JsonFieldKind::NonEmptyObjectArray(fields) => value.as_array().is_some_and(|items| {
            !items.is_empty()
                && items
                    .iter()
                    .all(|item| json_object_matches(item, fields))
        }),
    }
}

/// One catalogue task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskSpec {
    pub id: &'static str,
    pub kind: Kind,
    pub guard: Guard,
    pub instruction: &'static str,
}

/// The task a payload of this shape calls for.
///
/// One table, and the command decides before the class does: the same shape means
/// different work depending on what produced it (`git status` and a build log are
/// both columns of text, and only one of them wants a build digest). `None` means
/// no registered task fits, and the caller keeps today's bytes.
///
/// Every id named here is a row of [`TASKS`], and
/// `every_task_the_mapping_names_is_registered` fails if one stops being so.
pub fn task_for_payload(
    command: &str,
    class: super::reduce::PayloadClass,
) -> Option<&'static str> {
    use super::reduce::PayloadClass as P;
    let command = command.to_ascii_lowercase();
    let has = |needle: &str| command.contains(needle);

    // The command's own name for the work, first: it is the strongest signal.
    if has("git log") || has("git status") || has("git show --stat") {
        return Some("git_theme_summary");
    }
    if has("git blame") {
        return Some("blame_digest");
    }
    if has("gh pr") || has("glab mr") {
        return Some("pr_thread_digest");
    }
    if has("gh run") || has("gh workflow") {
        return Some("gh_run_digest");
    }
    if has("kubectl") {
        return Some("kubectl_digest");
    }
    if has("terraform") {
        return Some("terraform_plan_digest");
    }
    if has("lldb") || has("gdb") {
        return Some("debugger_output_digest");
    }
    if has("strace") || has("dtruss") || has("fs_usage") {
        return Some("syscall_trace_digest");
    }
    if has("vmmap") || has("leaks") || has("footprint") {
        return Some("memory_report_digest");
    }
    if has("otool") || has("objdump") || has("nm -") {
        return Some("binary_inspect_digest");
    }
    if has("curl") || has("httpie") || has("wget") {
        return Some("http_response_digest");
    }
    if has("lsof") || has("netstat") || has("ps aux") {
        return Some("process_snapshot_digest");
    }
    if has("lighthouse") || has("axe ") {
        return Some("lighthouse_digest");
    }
    if has("npm audit") || has("trivy") || has("grype") {
        return Some("vuln_scan_digest");
    }
    if has("glab ") {
        return Some("gitlab_api_digest");
    }
    if has("explain analyze") || has("explain ") {
        return Some("sql_explain_digest");
    }
    if has("pip freeze") || has("npm ls") || has("swift package resolve") {
        return Some("dependency_graph_digest");
    }
    if has("merge") && has("conflict") {
        return Some("merge_conflict_digest");
    }
    if has(".schema") || has("show create") {
        return Some("db_schema_digest");
    }
    if has("sanitize") || has("asan") || has("tsan") || has("ubsan") {
        return Some("sanitizer_report_digest");
    }
    if has("playwright") {
        return Some("playwright_trace_digest");
    }
    // A build or test run: what the command says it was doing decides which
    // digest reads it.
    if has("test") {
        return Some("test_verdict");
    }
    if has("xcodebuild") || has("swift build") || has("swiftc") {
        return Some("xcodebuild_error_extract");
    }
    if has("tsc") || has("cargo") || has("go build") || has("javac") || has("clippy") {
        return Some("compiler_diagnostics_extract");
    }
    if has("webpack") || has("vite") || has("next ") || has("esbuild") {
        return Some("bundler_build_digest");
    }
    if has("docker") || has("podman") {
        return Some("docker_error_tail");
    }
    if has("brew") || has("apt-get") || has("npm install") || has("pip install") {
        return Some("pkg_install_digest");
    }

    // Nothing in the command named it, so the shape decides.
    match class {
        P::TestReport => Some("test_verdict"),
        P::BuildLog => Some("compiler_diagnostics_extract"),
        P::Stack => Some("map_error_to_files"),
        P::Json => Some("json_shape"),
        P::Lockfile => Some("dependency_graph_digest"),
        P::Listing => Some("tree_listing_digest"),
        P::Diff => Some("patch_explain"),
        P::CommandOutput => Some("distill_command_output"),
        P::Prose => Some("distill_long_text"),
        P::Notebook | P::Html | P::Unknown => None,
    }
}

/// The catalogue's cheap tasks, one row per function a text model can serve.
///
/// Generated by `tools/gen_tasks.py` from the catalogue JSON, then reviewed:
/// the instruction is the catalogue's own summary plus the closing line its
/// kind needs, and the guard is what must pass before the answer is used.
pub const TASKS: &[TaskSpec] = &[
    TaskSpec { id: "ask_handle", kind: Kind::Ask, guard: Guard::Spans,
        instruction: "Extractive QA over a recovery-scope handle with obligatory spans. Answer with the smallest extract that answers the question, quoting the payload verbatim; do not paraphrase." },
    TaskSpec { id: "binary_inspect_digest", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "nm/otool/objdump/strings output → symbols and sections matching question. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "blame_digest", kind: Kind::Digest, guard: Guard::Literals,
        instruction: "git blame output → per-range author/commit/date relevant to question. Answer with the digest only, in short lines: what changed or what matters, with the paths and identifiers quoted exactly as they appear." },
    TaskSpec { id: "boilerplate_strip", kind: Kind::Digest, guard: Guard::Literals,
        instruction: "Drop license headers/generated banners. Answer with the digest only, in short lines: what changed or what matters, with the paths and identifiers quoted exactly as they appear." },
    TaskSpec { id: "bundler_build_digest", kind: Kind::Compress, guard: Guard::Literals,
        instruction: "webpack/vite/next build output → errors, warnings, emitted sizes. Answer with the compressed text only. Keep every command, path, file:line, number, identifier and error message verbatim; drop repetition and progress noise; aim for at most one third of the payload." },
    TaskSpec { id: "changelog_range_extract", kind: Kind::Digest, guard: Guard::Literals,
        instruction: "CHANGELOG/release notes → entries between two versions. Answer with the digest only, in short lines: what changed or what matters, with the paths and identifiers quoted exactly as they appear." },
    TaskSpec { id: "chat_thread_digest", kind: Kind::Digest, guard: Guard::Literals,
        instruction: "Slack/comment thread JSON → participants, decisions, open questions with spans. Answer with the digest only, in short lines: what changed or what matters, with the paths and identifiers quoted exactly as they appear." },
    TaskSpec { id: "chunk_select", kind: Kind::Pick, guard: Guard::CandidateIds,
        instruction: "Which line ranges of a file answer a question. Answer with the chosen ids only, one per line, and only ids that appear in the candidate list." },
    TaskSpec { id: "ci_job_failures", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "CI log → failed job names. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "cite_spans", kind: Kind::Ask, guard: Guard::Spans,
        instruction: "Quotes that must be substrings of original. Answer with the smallest extract that answers the question, quoting the payload verbatim; do not paraphrase." },
    TaskSpec { id: "classify_closed", kind: Kind::Classify, guard: Guard::ClosedSet(&[]),
        instruction: "Closed enum: pass/fail, file role, error class. Answer with the single label only, exactly as one of the listed labels." },
    TaskSpec { id: "cluster_lines", kind: Kind::Digest, guard: Guard::Literals,
        instruction: "Group similar failures; 1 example per group. Answer with the digest only, in short lines: what changed or what matters, with the paths and identifiers quoted exactly as they appear." },
    TaskSpec { id: "command_intent", kind: Kind::Classify, guard: Guard::ClosedSet(&["cwdAndFiles", "cwdOnly", "tokenVerdict", "pathsOnly", "existsMissing", "statusCode", "jsonOnly"]),
        instruction: "Existing CommandOutputIntent classifier. Answer with the single label only, exactly as one of the listed labels." },
    TaskSpec { id: "commit_message_draft", kind: Kind::Digest, guard: Guard::Literals,
        instruction: "Draft commit message from diff handle. Answer with the digest only, in short lines: what changed or what matters, with the paths and identifiers quoted exactly as they appear." },
    TaskSpec { id: "compiler_diagnostics_extract", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "Any compiler/typechecker output (tsc, cargo, go, javac, swiftc) → {file, line, severity, message}. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "constraint_extract", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "Acceptance criteria from a ticket blob. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "context_pack", kind: Kind::Pick, guard: Guard::CandidateIds,
        instruction: "Question → ranked reading list of handles, paths+ranges and skeletons; no prose claims. Answer with the chosen ids only, one per line, and only ids that appear in the candidate list." },
    TaskSpec { id: "coverage_summary", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "Coverage % and uncovered files. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "crash_report_digest", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "macOS .ips/crash log → exception type, faulting thread, top frames, relevant binary images. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "db_schema_digest", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "sqlite .schema / SHOW CREATE dump → relevant tables, columns, indexes. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "debugger_output_digest", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "lldb/gdb session output → relevant frames, variables, breakpoint hits. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "delta_handles", kind: Kind::Digest, guard: Guard::Literals,
        instruction: "What changed between two handles. Answer with the digest only, in short lines: what changed or what matters, with the paths and identifiers quoted exactly as they appear." },
    TaskSpec { id: "dependency_graph_digest", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "npm ls / pip freeze / SwiftPM resolve output → direct deps, versions, conflicts. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "distill_command_output", kind: Kind::Compress, guard: Guard::Literals,
        instruction: "Extractive compress of noisy stdout with question. Answer with the compressed text only. Keep every command, path, file:line, number, identifier and error message verbatim; drop repetition and progress noise; aim for at most one third of the payload." },
    TaskSpec { id: "distill_long_text", kind: Kind::Compress, guard: Guard::Literals,
        instruction: "Long prose/log extractive compress. Answer with the compressed text only. Keep every command, path, file:line, number, identifier and error message verbatim; drop repetition and progress noise; aim for at most one third of the payload." },
    TaskSpec { id: "docker_error_tail", kind: Kind::Compress, guard: Guard::Literals,
        instruction: "Last error in long docker/build log. Answer with the compressed text only. Keep every command, path, file:line, number, identifier and error message verbatim; drop repetition and progress noise; aim for at most one third of the payload." },
    TaskSpec { id: "email_thread_digest", kind: Kind::Digest, guard: Guard::Literals,
        instruction: "Email chains (.eml/mbox/M365 JSON) → participants, latest ask, decisions; quote-chain deduped. Answer with the digest only, in short lines: what changed or what matters, with the paths and identifiers quoted exactly as they appear." },
    TaskSpec { id: "entity_list", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "Files, tests, error tokens mentioned in blob. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "env_key_list", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "Env key names, never values. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "extract_schema", kind: Kind::Extract, guard: Guard::JsonObject(&EXTRACT_SCHEMA_CONTRACT),
        instruction: "Fill a caller-supplied closed JSON schema from handle. If the source schema is absent or ambiguous, answer NONE; otherwise answer one non-empty JSON object only; do not add prose or markdown." },
    TaskSpec { id: "file_role_classify", kind: Kind::Classify, guard: Guard::ClosedSet(&["test", "impl", "config", "generated", "skill"]),
        instruction: "test/impl/config/generated/skill. Answer with the single label only, exactly as one of the listed labels." },
    TaskSpec { id: "filter_line_numbers", kind: Kind::Ask, guard: Guard::Literals,
        instruction: "Line numbers matching an NL predicate. Answer with the smallest extract that answers the question, quoting the payload verbatim; do not paraphrase." },
    TaskSpec { id: "flake_classify", kind: Kind::Classify, guard: Guard::ClosedSet(&["flake", "consistent"]),
        instruction: "flake vs consistent fail (classify only). Answer with the single label only, exactly as one of the listed labels." },
    TaskSpec { id: "gh_run_digest", kind: Kind::Compress, guard: Guard::Literals,
        instruction: "gh run view / api noise. Answer with the compressed text only. Keep every command, path, file:line, number, identifier and error message verbatim; drop repetition and progress noise; aim for at most one third of the payload." },
    TaskSpec { id: "git_theme_summary", kind: Kind::Compress, guard: Guard::Literals,
        instruction: "git log/status noisy summary. Answer with the compressed text only. Keep every command, path, file:line, number, identifier and error message verbatim; drop repetition and progress noise; aim for at most one third of the payload." },
    TaskSpec { id: "gitlab_api_digest", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "glab/GitLab API JSON (MRs, pipelines, discussions) → unresolved threads + failed jobs. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "graph_ask", kind: Kind::Digest, guard: Guard::Literals,
        instruction: "Prose wrapper over graphify query result. Answer with the digest only, in short lines: what changed or what matters, with the paths and identifiers quoted exactly as they appear." },
    TaskSpec { id: "graphql_error_extract", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "GraphQL errors array digest. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "grep_hits_rank", kind: Kind::Pick, guard: Guard::CandidateIds,
        instruction: "Rank/group hits of an already-stored grep dump handle; voluntary paid call only — never auto-distills rg/grep. Answer with the chosen ids only, one per line, and only ids that appear in the candidate list." },
    TaskSpec { id: "har_summary", kind: Kind::Digest, guard: Guard::Literals,
        instruction: "HAR/network log: status and URLs only. Answer with the digest only, in short lines: what changed or what matters, with the paths and identifiers quoted exactly as they appear." },
    TaskSpec { id: "help_flags_extract", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "--help/man output → flags and subcommands relevant to question. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "http_response_digest", kind: Kind::Compress, guard: Guard::Literals,
        instruction: "curl/httpie output → status, key headers, body digest relevant to question. Answer with the compressed text only. Keep every command, path, file:line, number, identifier and error message verbatim; drop repetition and progress noise; aim for at most one third of the payload." },
    TaskSpec { id: "i18n_key_diff", kind: Kind::Digest, guard: Guard::Literals,
        instruction: "Missing translation keys. Answer with the digest only, in short lines: what changed or what matters, with the paths and identifiers quoted exactly as they appear." },
    TaskSpec { id: "i18n_translation_draft", kind: Kind::Digest, guard: Guard::Literals,
        instruction: "Draft translations for missing resource keys. Answer with the digest only, in short lines: what changed or what matters, with the paths and identifiers quoted exactly as they appear." },
    TaskSpec { id: "import_summary", kind: Kind::Digest, guard: Guard::Literals,
        instruction: "Import/include list from a file. Answer with the digest only, in short lines: what changed or what matters, with the paths and identifiers quoted exactly as they appear." },
    TaskSpec { id: "issue_triage_draft", kind: Kind::Digest, guard: Guard::Literals,
        instruction: "Suggest labels/duplicates/severity for a new issue from handles. Answer with the digest only, in short lines: what changed or what matters, with the paths and identifiers quoted exactly as they appear." },
    TaskSpec { id: "json_path_select", kind: Kind::Ask, guard: Guard::Literals,
        instruction: "Which JSON paths match a question. Answer with the smallest extract that answers the question, quoting the payload verbatim; do not paraphrase." },
    TaskSpec { id: "json_shape", kind: Kind::Extract, guard: Guard::JsonObject(&JSON_SHAPE_CONTRACT),
        instruction: "Infer keys of a JSON blob without values if secret-like. Answer with one JSON object only in the form {\"keys\":[\"key\"]}; include at least one non-empty key and no prose or markdown." },
    TaskSpec { id: "kubectl_digest", kind: Kind::Compress, guard: Guard::Literals,
        instruction: "Wide kubectl get output. Answer with the compressed text only. Keep every command, path, file:line, number, identifier and error message verbatim; drop repetition and progress noise; aim for at most one third of the payload." },
    TaskSpec { id: "lighthouse_digest", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "Lighthouse/axe JSON → scores + top violations grouped. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "linear_issue_digest", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "Extract title/AC/labels from issue JSON, no invention. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "lint_group", kind: Kind::Digest, guard: Guard::Literals,
        instruction: "Group linter hits by rule. Answer with the digest only, in short lines: what changed or what matters, with the paths and identifiers quoted exactly as they appear." },
    TaskSpec { id: "log_records", kind: Kind::Extract, guard: Guard::JsonObject(&LOG_RECORDS_CONTRACT),
        instruction: "Log lines → {ts,level,msg,file}. Answer with one JSON object only in the form {\"records\":[{\"ts\":\"...\",\"level\":\"...\",\"msg\":\"...\",\"file\":\"...\"}]}; include at least one complete record and no prose or markdown." },
    TaskSpec { id: "log_timeline", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "Timestamped event list. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "map_error_to_files", kind: Kind::Ask, guard: Guard::CandidateIds,
        instruction: "Stack/error → likely paths. Answer with the smallest extract that answers the question, quoting the payload verbatim; do not paraphrase." },
    TaskSpec { id: "mcp_result_digest", kind: Kind::Compress, guard: Guard::Literals,
        instruction: "Compress verbose MCP tool results (question-aware) where the provider hook supports rewrite. Answer with the compressed text only. Keep every command, path, file:line, number, identifier and error message verbatim; drop repetition and progress noise; aim for at most one third of the payload." },
    TaskSpec { id: "mcp_schema_trim", kind: Kind::Pick, guard: Guard::CandidateIds,
        instruction: "Filter huge MCP tool catalogs to matching names. Answer with the chosen ids only, one per line, and only ids that appear in the candidate list." },
    TaskSpec { id: "meeting_transcript_digest", kind: Kind::Digest, guard: Guard::Literals,
        instruction: "Meeting transcript → decisions, action items, owners, with spans. Answer with the digest only, in short lines: what changed or what matters, with the paths and identifiers quoted exactly as they appear." },
    TaskSpec { id: "memory_report_digest", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "vmmap/leaks/footprint output → top regions and allocations relevant to question. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "merge_conflict_digest", kind: Kind::Digest, guard: Guard::Literals,
        instruction: "Conflicted files + per-conflict ours/theirs summary; conflict markers quoted verbatim. Answer with the digest only, in short lines: what changed or what matters, with the paths and identifiers quoted exactly as they appear." },
    TaskSpec { id: "metric_extract", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "Numbers from bench output. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "multi_handle_ask", kind: Kind::Ask, guard: Guard::Spans,
        instruction: "One question over N handles. Answer with the smallest extract that answers the question, quoting the payload verbatim; do not paraphrase." },
    TaskSpec { id: "multi_log_timeline_merge", kind: Kind::Digest, guard: Guard::Literals,
        instruction: "Merge timestamped events across N log handles into one ordered timeline with source tags. Answer with the digest only, in short lines: what changed or what matters, with the paths and identifiers quoted exactly as they appear." },
    TaskSpec { id: "openapi_digest", kind: Kind::Pick, guard: Guard::CandidateIds,
        instruction: "OpenAPI/GraphQL SDL spec → endpoints/types matching question. Answer with the chosen ids only, one per line, and only ids that appear in the candidate list." },
    TaskSpec { id: "outline_structure", kind: Kind::Digest, guard: Guard::Literals,
        instruction: "Headings/functions with line ranges. Answer with the digest only, in short lines: what changed or what matters, with the paths and identifiers quoted exactly as they appear." },
    TaskSpec { id: "patch_explain", kind: Kind::Ask, guard: Guard::Literals,
        instruction: "Describe a unified diff; never apply. Answer with the smallest extract that answers the question, quoting the payload verbatim; do not paraphrase." },
    TaskSpec { id: "pick_candidates", kind: Kind::Pick, guard: Guard::CandidateIds,
        instruction: "Top-k ids from path/symbol list for a question. Answer with the chosen ids only, one per line, and only ids that appear in the candidate list." },
    TaskSpec { id: "pkg_install_digest", kind: Kind::Compress, guard: Guard::Literals,
        instruction: "brew/apt/npm install and upgrade logs → installed versions + warnings/errors. Answer with the compressed text only. Keep every command, path, file:line, number, identifier and error message verbatim; drop repetition and progress noise; aim for at most one third of the payload." },
    TaskSpec { id: "playwright_trace_digest", kind: Kind::Compress, guard: Guard::Literals,
        instruction: "Playwright log → failed spec + error. Answer with the compressed text only. Keep every command, path, file:line, number, identifier and error message verbatim; drop repetition and progress noise; aim for at most one third of the payload." },
    TaskSpec { id: "pr_description_draft", kind: Kind::Digest, guard: Guard::Literals,
        instruction: "Draft PR body from diff+tests. Answer with the digest only, in short lines: what changed or what matters, with the paths and identifiers quoted exactly as they appear." },
    TaskSpec { id: "pr_thread_digest", kind: Kind::Digest, guard: Guard::Literals,
        instruction: "gh pr review/comment JSON → unresolved threads {file, line, ask}. Answer with the digest only, in short lines: what changed or what matters, with the paths and identifiers quoted exactly as they appear." },
    TaskSpec { id: "process_snapshot_digest", kind: Kind::Compress, guard: Guard::Literals,
        instruction: "ps/lsof/netstat snapshots → entries matching question (orphans, sockets, ports). Answer with the compressed text only. Keep every command, path, file:line, number, identifier and error message verbatim; drop repetition and progress noise; aim for at most one third of the payload." },
    TaskSpec { id: "profiler_hotspots", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "Top stacks from profiler text. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "release_notes_draft", kind: Kind::Digest, guard: Guard::Literals,
        instruction: "Draft release notes from diff/log handles. Answer with the digest only, in short lines: what changed or what matters, with the paths and identifiers quoted exactly as they appear." },
    TaskSpec { id: "sanitizer_report_digest", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "ASan/TSan/UBSan report → leak/race class + key frames. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "search_query_suggest", kind: Kind::Ask, guard: Guard::Literals,
        instruction: "Suggest rg/graphify queries for a question; paid chooses and executes. Answer with the smallest extract that answers the question, quoting the payload verbatim; do not paraphrase." },
    TaskSpec { id: "skill_name_pick", kind: Kind::Pick, guard: Guard::CandidateIds,
        instruction: "Pick relevant skill NAMES only; never distill SKILL.md bodies. Answer with the chosen ids only, one per line, and only ids that appear in the candidate list." },
    TaskSpec { id: "snapshot_diff_digest", kind: Kind::Compress, guard: Guard::Literals,
        instruction: "Snapshot-test failure diffs → minimal changed-subtree summary. Answer with the compressed text only. Keep every command, path, file:line, number, identifier and error message verbatim; drop repetition and progress noise; aim for at most one third of the payload." },
    TaskSpec { id: "sql_explain_digest", kind: Kind::Compress, guard: Guard::Literals,
        instruction: "EXPLAIN/analyze output. Answer with the compressed text only. Keep every command, path, file:line, number, identifier and error message verbatim; drop repetition and progress noise; aim for at most one third of the payload." },
    TaskSpec { id: "sql_query_draft", kind: Kind::Ask, guard: Guard::Literals,
        instruction: "NL → SQL draft against a schema digest (live-DB triage). Answer with the smallest extract that answers the question, quoting the payload verbatim; do not paraphrase." },
    TaskSpec { id: "subagent_brief_compact", kind: Kind::Compress, guard: Guard::Literals,
        instruction: "Compress explore subagent dump before parent context. Answer with the compressed text only. Keep every command, path, file:line, number, identifier and error message verbatim; drop repetition and progress noise; aim for at most one third of the payload." },
    TaskSpec { id: "subagent_outcome_classify", kind: Kind::Classify, guard: Guard::ClosedSet(&["done", "partial", "failed", "off-task"]),
        instruction: "Subagent output → done/partial/failed/off-task with evidence spans. Answer with the single label only, exactly as one of the listed labels." },
    TaskSpec { id: "syscall_trace_digest", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "strace/dtruss/fs_usage trace → files/sockets touched + error syscalls relevant to question. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "table_extract", kind: Kind::Extract, guard: Guard::JsonObject(&TABLE_EXTRACT_CONTRACT),
        instruction: "Markdown/HTML tables → JSON rows. Answer with one JSON object only in the form {\"rows\":[{...}]}; include at least one non-empty row and no prose or markdown." },
    TaskSpec { id: "tabular_digest", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "Big CSV/TSV → columns, row count, question-relevant sample rows. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "terraform_plan_digest", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "Add/change/destroy counts + names. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "test_verdict", kind: Kind::Classify, guard: Guard::ClosedSet(&["PASS", "FAIL"]),
        instruction: "PASS/FAIL + failing names from test stdout. Answer with the single label only, exactly as one of the listed labels." },
    TaskSpec { id: "tree_listing_digest", kind: Kind::Compress, guard: Guard::Literals,
        instruction: "Huge find/ls -R listing → subtree summary relevant to question. Answer with the compressed text only. Keep every command, path, file:line, number, identifier and error message verbatim; drop repetition and progress noise; aim for at most one third of the payload." },
    TaskSpec { id: "ui_tree_digest", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "Accessibility/DOM tree dumps (browser or computer-use) → elements matching question {role, label, coords}. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
    TaskSpec { id: "vuln_scan_digest", kind: Kind::Extract, guard: Guard::JsonObject(&VULN_SCAN_DIGEST_CONTRACT),
        instruction: "npm audit/trivy/grype JSON → {package, severity, fixedIn} grouped. Answer with one JSON object only in the form {\"vulnerabilities\":[{\"package\":\"...\",\"severity\":\"...\",\"fixedIn\":\"...\"}]}; include at least one complete finding and no prose or markdown." },
    TaskSpec { id: "watch_summary", kind: Kind::Compress, guard: Guard::Literals,
        instruction: "Existing watchSummary mode. Answer with the compressed text only. Keep every command, path, file:line, number, identifier and error message verbatim; drop repetition and progress noise; aim for at most one third of the payload." },
    TaskSpec { id: "wire_encode", kind: Kind::Compress, guard: Guard::Literals,
        instruction: "Existing DistillMode.wireEncode. Answer with the compressed text only. Keep every command, path, file:line, number, identifier and error message verbatim; drop repetition and progress noise; aim for at most one third of the payload." },
    TaskSpec { id: "xcodebuild_error_extract", kind: Kind::Extract, guard: Guard::Literals,
        instruction: "xcodebuild/swift test errors. Answer with the extracted items only, one per line, each quoted exactly as it appears in the payload." },
];

/// The row for one catalogue id, if a text model can serve it here.
pub fn spec(id: &str) -> Option<&'static TaskSpec> {
    TASKS.iter().find(|task| task.id == id)
}

/// Every id the registry answers for.
pub fn ids() -> impl Iterator<Item = &'static str> {
    TASKS.iter().map(|task| task.id)
}

/// How many registry rows exist (the inventory's count for this family).
pub fn len() -> usize {
    TASKS.len()
}

/// The closed task one catalogue id becomes, given a payload and a question.
pub fn task_for(spec: &TaskSpec, payload: &str, question: &str) -> CheapTask {
    let instruction = if question.trim().is_empty() {
        spec.instruction.to_owned()
    } else {
        format!("{}\nQUESTION: {}", spec.instruction, question.trim())
    };
    let mut task = CheapTask::new(spec.id, instruction, payload);
    if spec.kind == Kind::Compress {
        // A compression answer is the payload, shorter: the ceiling has to allow
        // the payload's own literals to survive.
        let ceiling = (payload.len() / 2).max(512);
        task = task.with_max_answer_chars(ceiling);
    }
    task
}

/// Why an answer was not used. Every variant means the same thing to a caller:
/// keep today's bytes and record the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejected {
    /// The worker answered `NONE`: nothing in the payload for this task.
    Nothing,
    /// A literal of the payload is missing from the answer.
    LostLiterals(Vec<String>),
    /// The answer was not shorter than the payload.
    NotShorter,
    /// The answer was not a JSON object satisfying the task contract.
    NotJson,
    /// The task has no source schema that can be enforced safely.
    NoContract,
    /// The answer was not one of the labels the task allows.
    NotALabel(String),
    /// The answer named an id the caller never offered.
    InventedId(String),
    /// The answer contained no evidence for a task that requires it.
    NoEvidence,
    /// The answer quoted something that is not in the payload.
    NotQuoted(String),
}

impl Rejected {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Nothing => "nothing",
            Self::LostLiterals(_) => "lost_literals",
            Self::NotShorter => "not_shorter",
            Self::NotJson => "not_json",
            Self::NoContract => "no_contract",
            Self::NotALabel(_) => "not_a_label",
            Self::InventedId(_) => "invented_id",
            Self::NoEvidence => "no_evidence",
            Self::NotQuoted(_) => "not_quoted",
        }
    }

    /// One line for the decision record, carrying no payload text.
    pub fn detail(&self) -> String {
        match self {
            Self::Nothing => "the worker answered NONE".to_owned(),
            Self::LostLiterals(lost) => format!("{} literal(s) missing from the answer", lost.len()),
            Self::NotShorter => "the answer was not shorter than the payload".to_owned(),
            Self::NotJson => "the answer was not a valid JSON object for the task contract".to_owned(),
            Self::NoContract => "the task has no enforceable source schema".to_owned(),
            Self::NotALabel(label) => format!("`{label}` is not one of the allowed labels"),
            Self::InventedId(id) => format!("`{id}` was not in the candidate list"),
            Self::NoEvidence => "the answer contained no required evidence".to_owned(),
            Self::NotQuoted(span) => {
                format!("the answer quoted {} characters that are not in the payload", span.len())
            }
        }
    }
}

/// Applies a task's guard. `candidates` and `labels` are the caller's lists for
/// the `Pick` and `Classify` kinds.
pub fn gate(
    spec: &TaskSpec,
    payload: &str,
    answer: &str,
    candidates: &[String],
    labels: &[&str],
) -> Result<String, Rejected> {
    let trimmed = answer.trim();
    if trimmed.eq_ignore_ascii_case("none") {
        return Err(Rejected::Nothing);
    }
    match spec.guard {
        Guard::Literals => {
            if trimmed.len() >= payload.len() {
                return Err(Rejected::NotShorter);
            }
            let lost = reduce::lost_literals(payload, trimmed);
            if !lost.is_empty() {
                return Err(Rejected::LostLiterals(lost));
            }
        }
        Guard::JsonObject(contract) => {
            let Some(fields) = contract.fields else {
                return Err(Rejected::NoContract);
            };
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(trimmed);
            match parsed {
                Ok(value) if json_object_matches(&value, fields) => {}
                _ => return Err(Rejected::NotJson),
            }
        }
        Guard::ClosedSet(spec_labels) => {
            let allowed: Vec<&str> = if spec_labels.is_empty() {
                labels.to_vec()
            } else {
                spec_labels.to_vec()
            };
            let matched = allowed.iter().any(|label| trimmed == *label);
            if !matched {
                return Err(Rejected::NotALabel(trimmed.to_owned()));
            }
        }
        Guard::CandidateIds => {
            let ids = identifiers_in(trimmed);
            if ids.is_empty() {
                return Err(Rejected::NoEvidence);
            }
            for id in ids {
                if !candidates.iter().any(|candidate| candidate == &id) {
                    return Err(Rejected::InventedId(id));
                }
            }
        }
        Guard::Spans => {
            let spans = quoted_spans(trimmed);
            if spans.is_empty() {
                return Err(Rejected::NoEvidence);
            }
            for span in spans {
                if !payload.contains(&span) {
                    return Err(Rejected::NotQuoted(span));
                }
            }
        }
    }
    Ok(trimmed.to_owned())
}

/// Identifiers the answer names: one unadorned id per line, allowing the list
/// markers and quoting the task instruction commonly asks the worker to use.
fn identifiers_in(answer: &str) -> Vec<String> {
    answer
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line
                .strip_prefix('-')
                .or_else(|| line.strip_prefix('*'))
                .map(str::trim)
                .unwrap_or(line);
            let line = strip_numbered_list_marker(line);
            let line = line.trim().trim_end_matches(',').trim();
            let line = line
                .strip_prefix('`')
                .and_then(|line| line.strip_suffix('`'))
                .or_else(|| {
                    line.strip_prefix('"')
                        .and_then(|line| line.strip_suffix('"'))
                })
                .unwrap_or(line)
                .trim();
            (!line.is_empty()).then(|| line.to_owned())
        })
        .collect()
}

fn strip_numbered_list_marker(line: &str) -> &str {
    let digit_count = line
        .bytes()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digit_count > 0 {
        if let Some(marker) = line.as_bytes().get(digit_count) {
            if matches!(marker, b'.' | b')') {
                return line[digit_count + 1..].trim();
            }
        }
    }
    line
}

/// Quoted spans in an answer: backticks and double quotes, the shapes a citation
/// uses. Empty quotes are not evidence.
fn quoted_spans(answer: &str) -> Vec<String> {
    let mut spans = Vec::new();
    for quote in ['`', '"'] {
        let mut rest = answer;
        while let Some(start) = rest.find(quote) {
            let after = &rest[start + quote.len_utf8()..];
            let Some(end) = after.find(quote) else {
                break;
            };
            let span = &after[..end];
            if !span.is_empty() {
                spans.push(span.to_owned());
            }
            rest = &after[end + quote.len_utf8()..];
        }
    }
    spans
}

/// One answered, gated cheap task: the answer the turn may use, plus what it cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskOutcome {
    pub id: String,
    pub text: String,
    pub answer: CheapAnswer,
}

/// Runs one catalogue task: build the closed prompt, send one request, gate the
/// answer. `None` means "keep today's bytes", for any reason at all.
pub async fn run(
    client: &CheapClient,
    id: &str,
    payload: &str,
    question: &str,
) -> Option<TaskOutcome> {
    run_with(client, id, payload, question, &[], &[]).await
}

/// Same, with the caller's candidate ids and labels for the `Pick`/`Classify`
/// kinds.
pub async fn run_with(
    client: &CheapClient,
    id: &str,
    payload: &str,
    question: &str,
    candidates: &[String],
    labels: &[&str],
) -> Option<TaskOutcome> {
    let spec = spec(id)?;
    // The deterministic pass runs first: there is no reason to pay a model for
    // what a crusher already answered.
    let prepared = prepare(spec, payload)?;
    let task = task_for(spec, &prepared, question);
    if !task.fits(client.config().max_input_bytes) {
        return None;
    }
    let answer = client.ask(&task).await.ok()?;
    let accepted = gate(spec, &prepared, &answer.text, candidates, labels);
    tracing::info!(target: "jev.decision", event_kind = "utility_acceptance",
        task_id = id, request_id = answer.request_id.as_deref().unwrap_or(""),
        accepted = accepted.is_ok(), "utility validation outcome");
    let text = accepted.ok()?;
    Some(TaskOutcome {
        id: spec.id.to_owned(),
        text,
        answer,
    })
}

/// How many candidate answers a compression may ask for before choosing.
pub const BEST_OF: usize = 3;

/// Runs a compression up to [`BEST_OF`] times and keeps the best candidate.
///
/// "Best" is decided by the guard first (a candidate that lost a literal is out)
/// and by size second, so quality is never traded for a smaller answer. One
/// accepted candidate short-circuits the rest: the extra samples are for the
/// cases where the first answer is refused, not a default tax on every call.
pub async fn run_best_of(
    client: &CheapClient,
    id: &str,
    payload: &str,
    question: &str,
    samples: usize,
) -> Option<TaskOutcome> {
    let spec = spec(id)?;
    if spec.kind != Kind::Compress {
        // Only a compression has an honest "which of these is better" question.
        return run(client, id, payload, question).await;
    }
    let prepared = prepare(spec, payload)?;
    let mut best: Option<TaskOutcome> = None;
    for attempt in 0..samples.max(1) {
        let task = task_for(spec, &prepared, question);
        if !task.fits(client.config().max_input_bytes) {
            return best;
        }
        let Ok(answer) = client.ask(&task).await else {
            continue;
        };
        let Ok(text) = gate(spec, &prepared, &answer.text, &[], &[]) else {
            continue;
        };
        let candidate = TaskOutcome {
            id: spec.id.to_owned(),
            text,
            answer,
        };
        // A later candidate only wins by being shorter; ties keep the first, so
        // the same payload produces the same choice.
        let better = best
            .as_ref()
            .is_none_or(|current| candidate.text.len() < current.text.len());
        if better {
            best = Some(candidate);
        }
        if attempt == 0 && best.is_some() {
            // The first answer passed the guard and is already the shortest we
            // have: asking again would only cost money.
            break;
        }
    }
    best
}

/// The payload a task actually sends: for the compression kinds the crusher runs
/// first, so the model sees less and the literals are already protected.
pub fn prepare(spec: &TaskSpec, payload: &str) -> Option<String> {
    if payload.trim().is_empty() {
        return None;
    }
    match spec.kind {
        Kind::Compress => {
            let (cleaned, _) = crushers::preclean(payload);
            (!cleaned.trim().is_empty()).then_some(cleaned)
        }
        _ => Some(payload.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_for(id: &str) -> TaskSpec {
        *spec(id).unwrap_or_else(|| panic!("registry has no `{id}`"))
    }

    #[test]
    fn the_registry_covers_the_catalogue_rows_it_claims() {
        // The count is the inventory's: 93 catalogue functions a text model can
        // serve. A row that disappears must fail here, not in `list.md` later.
        assert_eq!(len(), 93, "registry size");
        for id in [
            "distill_command_output", "test_verdict", "compiler_diagnostics_extract",
            "commit_message_draft", "cite_spans", "pick_candidates", "wire_encode",
        ] {
            assert!(spec(id).is_some(), "`{id}` must be registered");
        }
        assert!(spec("local_agent_with_tools").is_none(), "a forbidden row is not a task");
        assert!(spec("screenshot_ocr").is_none(), "a host-bound row is not a task");
        // Every row names itself and a real instruction.
        for task in TASKS {
            assert!(!task.instruction.trim().is_empty(), "{}", task.id);
            assert!(task.instruction.len() > 40, "{}", task.id);
        }
    }

    #[test]
    fn the_compression_guard_needs_the_literals_and_a_shorter_answer() {
        let spec = spec_for("distill_command_output");
        let payload = "error[E0308]: mismatched types\n  --> src/client.rs:868:5\nwarning: unused";
        // A good compression keeps the error word and the location.
        let good = "error[E0308] at src/client.rs:868:5";
        assert_eq!(
            gate(&spec, payload, good, &[], &[]).expect("gated"),
            good
        );
        // One that drops the location is refused, and the detail names the count.
        let lossy = "a build error happened";
        let rejected = gate(&spec, payload, lossy, &[], &[]).expect_err("lost literals");
        assert!(matches!(rejected, Rejected::LostLiterals(_)), "{rejected:?}");
        assert_eq!(rejected.as_str(), "lost_literals");
        // Growth is not a compression.
        let grown = format!("{payload}{payload}");
        assert_eq!(
            gate(&spec, payload, &grown, &[], &[]).expect_err("not shorter"),
            Rejected::NotShorter
        );
        // `NONE` is a valid worker answer and never an outcome.
        assert_eq!(
            gate(&spec, payload, "NONE", &[], &[]).expect_err("nothing"),
            Rejected::Nothing
        );
    }

    #[test]
    fn the_classify_pick_and_span_guards_refuse_invented_answers() {
        let verdict = spec_for("test_verdict");
        assert!(gate(&verdict, "test result: FAILED", "FAIL", &[], &[]).is_ok());
        assert!(gate(&verdict, "test result: ok", "PASS", &[], &[]).is_ok());
        assert!(matches!(
            gate(&verdict, "x", "MAYBE", &[], &[]).expect_err("not a label"),
            Rejected::NotALabel(_)
        ));
        assert!(matches!(
            gate(&verdict, "x", "PASS because the tests passed", &[], &[])
                .expect_err("substring label"),
            Rejected::NotALabel(_)
        ));

        let dynamic = spec_for("classify_closed");
        assert!(gate(&dynamic, "x", "ready", &[], &["ready", "blocked"]).is_ok());
        assert!(matches!(
            gate(&dynamic, "x", "ready", &[], &[]).expect_err("empty closed set"),
            Rejected::NotALabel(_)
        ));

        let pick = spec_for("pick_candidates");
        let candidates = vec!["src/a.rs".to_owned(), "src/b.rs".to_owned()];
        assert!(gate(&pick, "payload", "src/a.rs", &candidates, &[]).is_ok());
        assert!(matches!(
            gate(&pick, "payload", "src/invented.rs", &candidates, &[]).expect_err("invented"),
            Rejected::InventedId(_)
        ));
        assert!(matches!(
            gate(
                &pick,
                "payload",
                "src/a.rs because it matters",
                &candidates,
                &[]
            )
            .expect_err("unrelated instruction"),
            Rejected::InventedId(_)
        ));
        assert_eq!(
            gate(&pick, "payload", "", &candidates, &[]).expect_err("empty ids"),
            Rejected::NoEvidence
        );
        let short_id = vec!["a".to_owned()];
        assert!(gate(&pick, "payload", "a", &short_id, &[]).is_ok());

        let ask = spec_for("cite_spans");
        let payload = "the function returns None when the file is missing";
        assert!(gate(&ask, payload, "`returns None when the file is missing`", &[], &[]).is_ok());
        assert!(matches!(
            gate(&ask, payload, "`a sentence that never appeared`", &[], &[]).expect_err("not quoted"),
            Rejected::NotQuoted(_)
        ));
        assert_eq!(
            gate(&ask, payload, "the answer has no citation", &[], &[]).expect_err("no span"),
            Rejected::NoEvidence
        );
        assert!(gate(&ask, "id", "`id`", &[], &[]).is_ok());
        assert!(matches!(
            gate(&ask, "id", "`no`", &[], &[]).expect_err("invented short span"),
            Rejected::NotQuoted(_)
        ));

        let extract = spec_for("extract_schema");
        assert_eq!(
            gate(&extract, "payload", r#"{"id": 1}"#, &[], &[])
                .expect_err("unknown schema"),
            Rejected::NoContract
        );
        assert_eq!(
            gate(&extract, "payload", "id: 1", &[], &[]).expect_err("unknown schema"),
            Rejected::NoContract
        );
    }

    #[test]
    fn structured_json_guards_require_the_declared_fields_and_types() {
        let extract = spec_for("extract_schema");
        assert_eq!(
            gate(&extract, "payload", "{}", &[], &[]).expect_err("empty object"),
            Rejected::NoContract
        );
        assert_eq!(
            gate(&extract, "payload", "[]", &[], &[]).expect_err("array"),
            Rejected::NoContract
        );

        let shape = spec_for("json_shape");
        assert!(gate(&shape, "payload", r#"{"keys":["user_id"]}"#, &[], &[]).is_ok());
        assert!(gate(&shape, "payload", r#"{"keys":[]}"#, &[], &[]).is_err());
        assert!(gate(&shape, "payload", r#"{"keys":"user_id"}"#, &[], &[]).is_err());

        let table = spec_for("table_extract");
        assert!(gate(&table, "payload", r#"{"rows":[{"name":"id"}]}"#, &[], &[]).is_ok());
        assert!(gate(&table, "payload", r#"{"rows":["id"]}"#, &[], &[]).is_err());

        let logs = spec_for("log_records");
        assert!(gate(
            &logs,
            "payload",
            r#"{"records":[{"ts":"2026-09-21T00:00:00Z","level":"error","msg":"failed","file":"src/lib.rs"}]}"#,
            &[],
            &[]
        )
        .is_ok());
        assert!(gate(
            &logs,
            "payload",
            r#"{"records":[{"ts":"now","level":"error","msg":"failed"}]}"#,
            &[],
            &[]
        )
        .is_err());

        let vulnerabilities = spec_for("vuln_scan_digest");
        assert!(gate(
            &vulnerabilities,
            "payload",
            r#"{"vulnerabilities":[{"package":"serde","severity":"high","fixedIn":null}]}"#,
            &[],
            &[]
        )
        .is_ok());
        assert!(gate(
            &vulnerabilities,
            "payload",
            r#"{"vulnerabilities":[{"package":"serde","severity":2,"fixedIn":null}]}"#,
            &[],
            &[]
        )
        .is_err());
    }

    #[test]
    fn the_task_carries_the_instruction_the_question_and_the_payload() {
        let spec = spec_for("distill_command_output");
        let task = task_for(&spec, "PAYLOADTEXT", "what failed?");
        assert_eq!(task.id, "distill_command_output");
        let rendered = task.render();
        assert!(rendered.contains("Extractive compress of noisy stdout"));
        assert!(rendered.contains("QUESTION: what failed?"));
        assert!(rendered.contains("PAYLOADTEXT"));

        // Without a question the instruction stands alone.
        let plain = task_for(&spec, "PAYLOADTEXT", "  ").render();
        assert!(!plain.contains("QUESTION:"));

        // A compression's answer ceiling allows the payload's own literals.
        let big = task_for(&spec, &"x".repeat(4_000), "");
        assert!(big.max_answer_chars >= 512);
    }

    /// The mapping names only tasks the registry answers for. A table that drifts
    /// from the registry would hand the cheap lane a task it cannot build.
    #[test]
    fn every_task_the_mapping_names_is_registered() {
        use crate::jev::reduce::PayloadClass as P;
        let commands = [
            "git log --oneline",
            "git status --short",
            "git blame src/main.rs",
            "gh pr view 12",
            "gh run view 999",
            "kubectl get pods",
            "terraform plan",
            "lldb ./app",
            "strace -f ./app",
            "vmmap 1234",
            "otool -L ./app",
            "curl -s https://example.org",
            "lsof -i :8080",
            "lighthouse https://example.org",
            "npm audit --json",
            "glab mr list",
            "sqlite3 app.db .schema",
            "pip freeze",
            "git merge origin/main conflict",
            "clang -fsanitize=address",
            "playwright test",
            "pytest -q",
            "cargo test --lib",
            "xcodebuild -scheme App",
            "cargo build --release",
            "vite build",
            "docker build .",
            "brew install jq",
            "an unknown command that names nothing",
        ];
        let classes = [
            P::TestReport,
            P::BuildLog,
            P::Stack,
            P::Json,
            P::Lockfile,
            P::Listing,
            P::Diff,
            P::CommandOutput,
            P::Prose,
            P::Notebook,
            P::Html,
            P::Unknown,
        ];
        for command in commands {
            for class in classes {
                if let Some(id) = task_for_payload(command, class) {
                    assert!(
                        spec(id).is_some(),
                        "`{id}` is named by the mapping for {command:?}/{class:?} and is not registered"
                    );
                }
            }
        }
    }

    /// The command decides before the shape does, and a shape with no reader is
    /// left to the session model.
    #[test]
    fn the_command_decides_before_the_shape_does() {
        use crate::jev::reduce::PayloadClass as P;
        // One class, four commands, four different readers.
        assert_eq!(
            task_for_payload("cargo test --lib", P::BuildLog),
            Some("test_verdict")
        );
        assert_eq!(
            task_for_payload("kubectl get pods", P::BuildLog),
            Some("kubectl_digest")
        );
        assert_eq!(
            task_for_payload("cargo build --release", P::BuildLog),
            Some("compiler_diagnostics_extract")
        );
        assert_eq!(
            task_for_payload("vite build", P::BuildLog),
            Some("bundler_build_digest")
        );
        // Nothing named in the command: the shape decides.
        assert_eq!(task_for_payload("", P::TestReport), Some("test_verdict"));
        assert_eq!(task_for_payload("", P::Lockfile), Some("dependency_graph_digest"));
        assert_eq!(task_for_payload("", P::Stack), Some("map_error_to_files"));
        assert_eq!(task_for_payload("", P::Listing), Some("tree_listing_digest"));
        // Shapes no registered task reads keep the bytes with the session model.
        assert_eq!(task_for_payload("", P::Unknown), None);
        assert_eq!(task_for_payload("", P::Html), None);
        assert_eq!(task_for_payload("", P::Notebook), None);
    }

    /// The id the mapping returns runs through the shipped path with the shipped
    /// guard: a checked answer comes back, an unchecked one returns nothing and
    /// the caller keeps today's bytes.
    #[tokio::test]
    async fn the_mapped_id_runs_through_the_shipped_path_and_keeps_its_guard() {
        use crate::jev::cheap::test_support::client_answering;
        use crate::jev::reduce::PayloadClass as P;

        let payload = "error[E0308]: mismatched types\n  --> src/client.rs:868:5\nwarning: unused var\n";
        let id = task_for_payload("some command", P::CommandOutput)
            .expect("a command output has a reader");
        assert_eq!(id, "distill_command_output");

        let (_stub, client) = client_answering("error[E0308] at src/client.rs:868:5").await;
        let outcome = run(&client, id, payload, "what failed?")
            .await
            .expect("the shipped path returns an outcome");
        assert_eq!(outcome.id, "distill_command_output");
        assert_eq!(outcome.text, "error[E0308] at src/client.rs:868:5");

        // An answer that drops the location fails the literal gate: nothing is
        // returned, so the live path leaves the payload alone.
        let (_stub, client) = client_answering("a build error happened somewhere").await;
        assert!(
            run(&client, id, payload, "what failed?").await.is_none(),
            "a rejected answer must not reach the caller"
        );
    }

    /// The shipped path, end to end, against a stub endpoint: registry row →
    /// prepared payload → **one** request → gate → the text the caller uses.
    #[tokio::test]
    async fn a_registry_task_runs_the_shipped_path_and_is_gated() {
        use crate::jev::cheap::test_support::{client_answering, make_stub, chat_reply};
        use crate::jev::cheap::{CheapClient, CheapConfig};

        let payload = "error[E0308]: mismatched types\n  --> src/client.rs:868:5\nwarning: unused var\n";

        // A good answer: shorter, and it keeps the error word and the location.
        let (stub, client) = client_answering("error[E0308] at src/client.rs:868:5").await;
        let outcome = run(&client, "distill_command_output", payload, "what failed?")
            .await
            .expect("the shipped path returns an outcome");
        assert_eq!(outcome.id, "distill_command_output");
        assert_eq!(outcome.text, "error[E0308] at src/client.rs:868:5");
        assert_eq!(outcome.answer.model, "qwen/qwen3.7-flash");
        let bodies = stub.bodies();
        assert_eq!(bodies.len(), 1, "one task is one request");
        let sent: serde_json::Value = serde_json::from_str(&bodies[0]).expect("json");
        assert!(sent["messages"][1]["content"]
            .as_str()
            .expect("user text")
            .contains("QUESTION: what failed?"));
        assert_eq!(sent["reasoning"]["enabled"], false, "a closed task does not think");

        // An answer that drops the location is refused, and nothing is returned.
        let (_stub, client) = client_answering("a build error happened somewhere").await;
        assert!(run(&client, "distill_command_output", payload, "what failed?")
            .await
            .is_none());

        // NONE is an answer, and it is not an outcome either.
        let (_stub, client) = client_answering("NONE").await;
        assert!(run(&client, "distill_command_output", payload, "").await.is_none());

        // An unknown id is not a task.
        let (_stub, client) = client_answering("whatever").await;
        assert!(run(&client, "no_such_task", payload, "").await.is_none());

        // A failed call leaves the caller on today's bytes.
        let failing = make_stub((429, chat_reply("slow down", (1, 1)), false));
        let client = crate::jev::cheap::test_support::client_for(&failing, |_| {}).await;
        assert!(run(&client, "distill_command_output", payload, "").await.is_none());

        // And with no credential the lane is silent.
        let offline = CheapClient::new(CheapConfig::default()).expect("builds");
        if !offline.credential_present() {
            assert!(run(&offline, "distill_command_output", payload, "").await.is_none());
        }
    }

    /// Best-of-three: a refused first candidate costs another sample, a good one
    /// costs nothing, and the winner is always the smallest that kept its
    /// literals.
    #[tokio::test]
    async fn a_compression_can_ask_three_times_and_keeps_the_best() {
        use crate::jev::cheap::test_support::{client_answering, client_for, make_stub, chat_reply};

        let payload = "error[E0308]: mismatched types\n  --> src/client.rs:868:5\nwarning: unused\n";

        // The first candidate drops the location, the second keeps everything but
        // is longer than the third: the third wins.
        let stub = make_stub((200, chat_reply("a build error happened", (10, 5)), false));
        let client = client_for(&stub, |_| {}).await;
        let first = run_best_of(&client, "distill_command_output", payload, "", BEST_OF).await;
        assert!(first.is_none(), "a candidate that lost a literal is not the best");

        // One acceptable candidate short-circuits the rest: exactly one request.
        let (stub, client) = client_answering("error[E0308] at src/client.rs:868:5").await;
        let outcome = run_best_of(&client, "distill_command_output", payload, "", BEST_OF)
            .await
            .expect("the first acceptable candidate wins");
        assert_eq!(outcome.text, "error[E0308] at src/client.rs:868:5");
        assert_eq!(stub.bodies().len(), 1, "one accepted answer, one request");

        // A non-compression task is not sampled: it answers once, as always.
        let (stub, client) = client_answering("FAIL").await;
        let outcome = run_best_of(&client, "test_verdict", "test result: FAILED", "", BEST_OF)
            .await
            .expect("labelled");
        assert_eq!(outcome.text, "FAIL");
        assert_eq!(stub.bodies().len(), 1);
    }

    /// A classify task is gated by its closed labels, and the answer that wins is
    /// the label — not a sentence around it.
    #[tokio::test]
    async fn a_classify_task_returns_a_label_the_caller_can_branch_on() {
        use crate::jev::cheap::test_support::client_answering;

        let payload = "running 3 tests\ntest a ... ok\ntest c ... FAILED\ntest result: FAILED. 2 passed";
        let (_stub, client) = client_answering("FAIL").await;
        let outcome = run(&client, "test_verdict", payload, "").await.expect("labelled");
        assert_eq!(outcome.text, "FAIL");

        let (_stub, client) = client_answering("It depends on the runner").await;
        assert!(
            run(&client, "test_verdict", payload, "").await.is_none(),
            "a sentence is not a label"
        );
    }

    #[test]
    fn the_compression_preparation_runs_the_deterministic_pass_first() {
        let spec = spec_for("distill_command_output");
        let noisy = format!("{}\u{1b}[2K\n", "   Fresh (0.1s)\n".repeat(40));
        let prepared = prepare(&spec, &noisy).expect("prepared");
        assert!(prepared.len() < noisy.len(), "the crusher already shrank it");
        // An empty payload is not a task.
        assert!(prepare(&spec, "   ").is_none());
    }
}
