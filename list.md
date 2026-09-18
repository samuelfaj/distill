# list.md — inventário de capacidades do harness (Jev + modelo barato)

Fonte: `parte-a.md` (o que o app macOS fazia com o modelo local para poupar
tokens) e o catálogo `plan-new-llm/01-catalogo.html` (188 funções, via
`function-catalog.json`). Este arquivo é o inventário **completo**: nenhuma
capacidade dos dois documentos fica de fora, e uma linha só diz `implemented`
quando o flag e o teste nomeados nela existem no repo.

## Como ler

| status | significa |
| --- | --- |
| `implemented` | código + flag + teste existem neste repo (a linha nomeia os dois) |
| `planned` | a lane existe neste plano e ainda está sendo construída |
| `mapped` | uma alavanca que já existia no harness faz esse trabalho (nomeada) |
| `deferred` | não é construível aqui, com o motivo permitido |

Motivos permitidos para `deferred`: `forbidden by the catalogue`,
`35B-only`, `host-bound (macOS app)`, `needs a local model (embeddings/NLI)`,
`host/paid authority`, `no harness seam`.

Catálogo: **188** funções — 0 implemented, 130 planned,
58 deferred. `parte-a.md`: **27** oportunidades —
1 implemented, 20 planned, 4 mapped, 2 deferred.

## 1. Catálogo — o que entra no harness

| id | o que faz | seam no harness | flag | teste |
| --- | --- | --- | --- | --- |
| `retrieve` | Byte-exact original behind [[rc:handle]] | pós-processo de tool result | `E11_handles` | — |
| `stats` | Ledger stats, no payloads | pós-processo de tool result | `E11_handles` | — |
| `search_store` | CCR/reversible-store search by query | pós-processo de tool result | `E11_handles` | — |
| `json_crusher` | Deterministic JSON/TOON crush | pós-processo de tool result | `E10_crushers` | — |
| `log_crusher` | Collapse repeated log lines | pós-processo de tool result | `E10_crushers` | — |
| `stack_crusher` | Keep frames, drop dumps | pós-processo de tool result | `E10_crushers` | — |
| `test_crusher` | Keep fail names and assertion text | pós-processo de tool result | `E10_crushers` | — |
| `diff_crusher` | Keep file headers and hunks, drop noise | pós-processo de tool result | `E10_crushers` | — |
| `html_crusher` | Strip chrome, keep text | pós-processo de tool result | `E10_crushers` | — |
| `source_skeleton` | Signatures/line map without bodies | pós-processo de tool result | `E10_crushers` | — |
| `toon_codec` | Uniform JSON arrays as TOON | pós-processo de tool result | `E10_crushers` | — |
| `notebook_crusher` | Strip .ipynb outputs/base64 images; keep code+markdown cells | pós-processo de tool result | `E10_crushers` | — |
| `lockfile_crusher` | package-lock/yarn.lock/Cargo.lock/Package.resolved → top-level deps + counts, never full graph | pós-processo de tool result | `E10_crushers` | — |
| `generated_asset_notice` | Binary/minified/generated mega-file → typed notice (size, kind, hash) + handle instead of bytes | pós-processo de tool result | `E10_crushers` | — |
| `embedded_blob_crusher` | base64/data-URI/hexdump islands inside text → typed placeholder + sub-handle | pós-processo de tool result | `E10_crushers` | — |
| `progress_bar_crusher` | Collapse carriage-return progress/spinner frames (npm, pip, docker pull) to final state per bar | pós-processo de tool result | `E10_crushers` | — |
| `secret_redact_view` | Masked view of secret-bearing blob: keys kept, values masked; deterministic masker, fail-closed | pós-processo de tool result | `E10_crushers` | — |
| `repo_map_budget` | Ranked repo map of signatures fitted to a token budget (aider-style) for session boot | pós-processo de tool result | `E10_crushers` | — |
| `retrieve_range` | Byte-exact slice of a handle by line/byte range instead of the whole blob | pós-processo de tool result | `E11_handles` | — |
| `handle_grep` | Exact grep inside handle(s): verbatim match lines + line numbers | pós-processo de tool result | `E11_handles` | — |
| `handle_query_eval` | Run a paid-supplied deterministic query (regex/jq/xpath/line-range) against a handle → counts + sample spans; no LLM involved | pós-processo de tool result | `E11_handles` | — |
| `handle_token_estimate` | Per-provider token estimate + head/tail preview of a handle, so paid can decide retrieve-or-not | pós-processo de tool result | `E11_handles` | — |
| `identifier_alias_codec` | Reversible aliasing of long UUIDs/hashes/paths to short tokens; expansion table behind a handle | pós-processo de tool result | `E2_importance_extract` | — |
| `ansi_escape_strip` | Strip ANSI color/cursor escape codes that inflate tokenization | pós-processo de tool result | `E10_crushers` | — |
| `padded_table_compact` | Collapse alignment whitespace in columnar CLI output; CSV/TOON re-emit | pós-processo de tool result | `E10_crushers` | — |
| `workspace_change_notice` | FSEvents-based list of files changed since a given turn + per-handle staleness check; paid re-reads only those | pós-processo de tool result | `E4_read_reuse` | — |
| `write_ack_verify` | Successful write/edit tool results → terse ack {lines, hash, applied hunks} instead of full echo; kills the verify re-read | pós-processo de tool result | `E4_read_reuse` | — |
| `stdout_budget_elide` | Generic byte budget on any stdout when no specific crusher matches: head+tail verbatim, middle elided to [[rc:handle]] | pós-processo de tool result | `E2_importance_extract` | — |
| `svg_crusher` | Strip SVG path-coordinate blobs → structure, text, viewBox; full behind handle | pós-processo de tool result | `E10_crushers` | — |
| `error_site_autoquote` | Parse file:line refs in failing output; host appends the referenced source lines (±N) verbatim so paid skips the follow-up read | pós-processo de tool result | `E2_importance_extract` | — |
| `schema_validate_eval` | Validate handle content against a paid-supplied JSON Schema/grammar → error list with paths; no LLM involved | pós-processo de tool result | `E11_handles` | — |
| `test_baseline_diff` | Compare current test failures against a stored baseline handle → only NEW and newly-fixed failures; pre-existing flakes stop burning paid attention | pós-processo de tool result | `E11_handles` | — |
| `distill_command_output` | Extractive compress of noisy stdout with question | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `distill_long_text` | Long prose/log extractive compress | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `watch_summary` | Existing watchSummary mode | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `ask_handle` | Extractive QA over a recovery-scope handle with obligatory spans | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `extract_schema` | Fill closed JSON schema from handle | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `pick_candidates` | Top-k ids from path/symbol list for a question | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `classify_closed` | Closed enum: pass/fail, file role, error class | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `cluster_lines` | Group similar failures; 1 example per group | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `outline_structure` | Headings/functions with line ranges | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `delta_handles` | What changed between two handles | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `cite_spans` | Quotes that must be substrings of original | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `entity_list` | Files, tests, error tokens mentioned in blob | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `test_verdict` | PASS/FAIL + failing names from test stdout | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `command_intent` | Existing CommandOutputIntent classifier | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `filter_line_numbers` | Line numbers matching an NL predicate | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `map_error_to_files` | Stack/error → likely paths | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `json_shape` | Infer keys of a JSON blob without values if secret-like | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `log_records` | Log lines → {ts,level,msg,file} | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `git_theme_summary` | git log/status noisy summary | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `ci_job_failures` | CI log → failed job names | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `lint_group` | Group linter hits by rule | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `coverage_summary` | Coverage % and uncovered files | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `docker_error_tail` | Last error in long docker/build log | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `terraform_plan_digest` | Add/change/destroy counts + names | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `kubectl_digest` | Wide kubectl get output | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `gh_run_digest` | gh run view / api noise | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `linear_issue_digest` | Extract title/AC/labels from issue JSON, no invention | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `mcp_schema_trim` | Filter huge MCP tool catalogs to matching names | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `patch_explain` | Describe a unified diff; never apply | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `chunk_select` | Which line ranges of a file answer a question | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `file_role_classify` | test/impl/config/generated/skill | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `json_path_select` | Which JSON paths match a question | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `table_extract` | Markdown/HTML tables → JSON rows | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `metric_extract` | Numbers from bench output | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `playwright_trace_digest` | Playwright log → failed spec + error | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `xcodebuild_error_extract` | xcodebuild/swift test errors | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `subagent_brief_compact` | Compress explore subagent dump before parent context | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `multi_handle_ask` | One question over N handles | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `constraint_extract` | Acceptance criteria from a ticket blob | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `import_summary` | Import/include list from a file | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `log_timeline` | Timestamped event list | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `graphql_error_extract` | GraphQL errors array digest | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `har_summary` | HAR/network log: status and URLs only | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `sql_explain_digest` | EXPLAIN/analyze output | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `profiler_hotspots` | Top stacks from profiler text | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `flake_classify` | flake vs consistent fail (classify only) | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `boilerplate_strip` | Drop license headers/generated banners | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `graph_ask` | Prose wrapper over graphify query result | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `skill_name_pick` | Pick relevant skill NAMES only; never distill SKILL.md bodies | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `env_key_list` | Env key names, never values | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `duplicate_handle_notice` | This blob already stored as handle X | pós-processo de tool result | `E11_handles` | — |
| `secret_presence_flag` | Regex/heuristic: blob looks secret-bearing; do not echo values | pós-processo de tool result | `E10_crushers` | — |
| `wire_encode` | Existing DistillMode.wireEncode | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `compact_span` | Existing compact span codec | pós-processo de tool result | `E10_crushers` | — |
| `tree_listing_digest` | Huge find/ls -R listing → subtree summary relevant to question | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `mcp_result_digest` | Compress verbose MCP tool results (question-aware) where the provider hook supports rewrite | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `grep_hits_rank` | Rank/group hits of an already-stored grep dump handle; voluntary paid call only — never auto-distills rg/grep | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `context_pack` | Question → ranked reading list of handles, paths+ranges and skeletons; no prose claims | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `dependency_graph_digest` | npm ls / pip freeze / SwiftPM resolve output → direct deps, versions, conflicts | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `crash_report_digest` | macOS .ips/crash log → exception type, faulting thread, top frames, relevant binary images | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `blame_digest` | git blame output → per-range author/commit/date relevant to question | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `merge_conflict_digest` | Conflicted files + per-conflict ours/theirs summary; conflict markers quoted verbatim | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `pr_thread_digest` | gh pr review/comment JSON → unresolved threads {file, line, ask} | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `chat_thread_digest` | Slack/comment thread JSON → participants, decisions, open questions with spans | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `help_flags_extract` | --help/man output → flags and subcommands relevant to question | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `openapi_digest` | OpenAPI/GraphQL SDL spec → endpoints/types matching question | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `db_schema_digest` | sqlite .schema / SHOW CREATE dump → relevant tables, columns, indexes | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `tabular_digest` | Big CSV/TSV → columns, row count, question-relevant sample rows | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `compiler_diagnostics_extract` | Any compiler/typechecker output (tsc, cargo, go, javac, swiftc) → {file, line, severity, message} | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `bundler_build_digest` | webpack/vite/next build output → errors, warnings, emitted sizes | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `snapshot_diff_digest` | Snapshot-test failure diffs → minimal changed-subtree summary | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `pkg_install_digest` | brew/apt/npm install and upgrade logs → installed versions + warnings/errors | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `http_response_digest` | curl/httpie output → status, key headers, body digest relevant to question | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `process_snapshot_digest` | ps/lsof/netstat snapshots → entries matching question (orphans, sockets, ports) | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `sanitizer_report_digest` | ASan/TSan/UBSan report → leak/race class + key frames | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `syscall_trace_digest` | strace/dtruss/fs_usage trace → files/sockets touched + error syscalls relevant to question | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `debugger_output_digest` | lldb/gdb session output → relevant frames, variables, breakpoint hits | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `memory_report_digest` | vmmap/leaks/footprint output → top regions and allocations relevant to question | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `binary_inspect_digest` | nm/otool/objdump/strings output → symbols and sections matching question | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `lighthouse_digest` | Lighthouse/axe JSON → scores + top violations grouped | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `vuln_scan_digest` | npm audit/trivy/grype JSON → {package, severity, fixedIn} grouped | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `gitlab_api_digest` | glab/GitLab API JSON (MRs, pipelines, discussions) → unresolved threads + failed jobs | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `email_thread_digest` | Email chains (.eml/mbox/M365 JSON) → participants, latest ask, decisions; quote-chain deduped | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `meeting_transcript_digest` | Meeting transcript → decisions, action items, owners, with spans | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `changelog_range_extract` | CHANGELOG/release notes → entries between two versions | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `subagent_outcome_classify` | Subagent output → done/partial/failed/off-task with evidence spans | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `pii_presence_flag` | Heuristic PII detector (emails, names, document numbers) → flag + masked view; keeps customer data out of paid context | pós-processo de tool result | `E10_crushers` | — |
| `multi_log_timeline_merge` | Merge timestamped events across N log handles into one ordered timeline with source tags | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `ui_tree_digest` | Accessibility/DOM tree dumps (browser or computer-use) → elements matching question {role, label, coords} | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `injection_pattern_flag` | Heuristic prompt-injection detector on fetched/untrusted content → flag + quarantine notice; content stays behind handle until paid opts in | pós-processo de tool result | `E10_crushers` | — |
| `commit_message_draft` | Draft commit message from diff handle | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `pr_description_draft` | Draft PR body from diff+tests | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `i18n_key_diff` | Missing translation keys | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `search_query_suggest` | Suggest rg/graphify queries for a question; paid chooses and executes | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `release_notes_draft` | Draft release notes from diff/log handles | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `i18n_translation_draft` | Draft translations for missing resource keys | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `issue_triage_draft` | Suggest labels/duplicates/severity for a new issue from handles | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |
| `sql_query_draft` | NL → SQL draft against a schema digest (live-DB triage) | tarefa registrada (`jev/tasks.rs`) | `E5_cheap_task` | — |

## 2. Catálogo — deferidos, com o motivo

| id | o que faz | motivo |
| --- | --- | --- |
| `encoder_rank` | Embedding rank of chunks vs question | needs a local model (embeddings/NLI) |
| `graphify_query` | Existing graphify CLI query/path/explain | host-bound (macOS app) |
| `path_wrappers` | Host PATH wrap of noisy binaries | host-bound (macOS app) |
| `nli_gate` | Existing BERT NLI reject-only gate | needs a local model (embeddings/NLI) |
| `screenshot_ocr` | On-device Vision OCR of pasted image → text + boxes instead of vision tokens; secret pre-gate applies to OCR text | host-bound (macOS app) |
| `doc_text_extract` | pdftotext/textutil text layer of PDF/DOCX/RTF → handle; secret pre-gate applies | host-bound (macOS app) |
| `semantic_cache_lookup` | Embedding lookup: similar question/blob already answered at handle X; notice-only, never substitutes bytes | needs a local model (embeddings/NLI) |
| `workspace_semantic_search` | Embedding search over host-indexed workspace chunks → ranked paths+ranges; host feeds chunks, sidecar never open(2) | needs a local model (embeddings/NLI) |
| `stale_tool_result_evict` | Host-owned resume/replay context only: age/size policy swaps old tool results for [[rc:handle]] stubs; never rewrites a live provider session | host-bound (macOS app) |
| `session_transcript_search` | Keyword/embedding search over the host-persisted session transcript → turn refs + spans; survives compaction | needs a local model (embeddings/NLI) |
| `repeated_failure_notice` | Embedding similarity: this error ≈ previous attempts at handles X,Y; notice-only loop detector | needs a local model (embeddings/NLI) |
| `audio_transcribe_local` | On-device ASR of audio (meetings, voice notes) → transcript + timestamps; secret/PII pre-gate applies | host-bound (macOS app) |
| `video_keyframe_ocr` | Keyframes + OCR + scene timestamps from a video (demo-video validation) instead of paid vision | host-bound (macOS app) |
| `screenshot_diff_digest` | Pixel-diff two images → changed-region boxes, % changed, OCR of changed regions | host-bound (macOS app) |
| `unchanged_reread_stub` | Re-read of a file unchanged since handle X → stub + delta ranges, via provider rewrite hook only (Claude PostToolUse); Grok/Codex native reads stay untouched | host-bound (macOS app) |
| `predicate_watch` | Paid-initiated watch: host re-runs the same already-approved command until regex predicate matches or timeout; paid receives one final result instead of N polls | host-bound (macOS app) |
| `attachment_handle_intake` | User file attachments become handles + typed preview (kind, size, pages); paid pulls ranges/digests on demand instead of full inline. User prompt TEXT stays verbatim | host-bound (macOS app) |
| `lsp_query` | sourcekit-lsp/tsserver queries: definition, references, hover types, document symbols → exact locations + signatures instead of grep+read storms | host-bound (macOS app) |
| `ast_grep_query` | Tree-sitter/ast-grep structural pattern queries over host-fed workspace files → match locations | host-bound (macOS app) |
| `history_recall_search` | Embedding search across past session stores/threads in the same workspace scope → prior fixes/decisions + handles; notice-only | needs a local model (embeddings/NLI) |
| `terminal_scrollback_intake` | Ghostty scrollback the user attaches → scoped handle + crushers; paid gets digest + handle, not raw scrollback | host-bound (macOS app) |
| `batch_codemod_apply` | Paid-authored deterministic rewrite rule (regex/ast-grep), host-executed across N files → per-file diff digest; local LLM never authors the rule; saves paid OUTPUT tokens on mass renames/import updates | host-bound (macOS app) |
| `stale_tool_args_evict` | Host-owned resume/replay context: old write/edit tool ARGUMENTS collapse to {path, hash, handle} stubs — the content is on disk and in store; never mid live provider session | host-bound (macOS app) |
| `spreadsheet_extract` | xlsx/numbers workbook → sheet list + tabular digest per sheet via host converter (paid can't read the binary at all); secret/PII gates apply | host-bound (macOS app) |
| `correction_exemplar_store` | Store paid's correction of a local answer as a scoped few-shot exemplar for future local calls on similar blobs; exemplars never cross workspace scope | needs a local model (embeddings/NLI) |
| `nli_claim` | entail/contradict/unknown of claim vs handle | needs a local model (embeddings/NLI) |
| `semantic_dedup` | Near-duplicate handles/chunks | needs a local model (embeddings/NLI) |
| `metric_delta` | Two metric handles → per-metric delta table; numbers must be substrings of both sources | needs a local model (embeddings/NLI) |
| `impacted_test_pick` | Diff handle + test inventory → likely-impacted test ids; never gates CI | 35B-only |
| `thread_title_generate` | Host-UI labels (thread titles, tab names) from first turns; UI-only, never injected into paid context | host/paid authority |
| `pbxproj_digest` | Xcode .pbxproj → targets, phases, build settings relevant to question | host-bound (macOS app) |
| `codesign_notarize_digest` | codesign/notarytool/stapler output → identity, entitlements, errors | host-bound (macOS app) |
| `cross_handle_contradiction_scan` | Find contradicting claims across N handles; both sides quoted as obligatory spans | 35B-only |
| `provider_tier_suggest` | Suggest the cheapest adequate paid tier/model for a prompt (model-Auto input); UI-surfaced only | host/paid authority |
| `plan_candidate` | 5-step candidate plan, labeled non-authoritative | 35B-only |
| `review_comment_draft` | Draft review notes with required citations | 35B-only |
| `refactor_suggestion_list` | List possible refactors; do not apply | 35B-only |
| `test_plan_draft` | Suggested test names from a change | 35B-only |
| `root_cause_hypothesis_draft` | Ranked failure hypotheses from an error digest, labeled non-authoritative | 35B-only |
| `local_agent_with_tools` | Do not give Ornith shell/FS/web tools | forbidden by the catalogue |
| `apply_patch` | Local model must not apply or emit authoritative patches | forbidden by the catalogue |
| `generate_code_to_ship` | No shipping code from 9B/35B | forbidden by the catalogue |
| `distill_skill_body` | SKILL.md and skill tool output stay verbatim | forbidden by the catalogue |
| `distill_exact_rg` | Never auto-distill rg/grep/cat and listed exact-output tools | forbidden by the catalogue |
| `rewrite_system_prompt` | Local must not mutate provider safety/system prompts | forbidden by the catalogue |
| `extract_secret_values` | Never return token/password/key values | forbidden by the catalogue |
| `authority_merge` | Local is never merge/review authority | forbidden by the catalogue |
| `forward_thinking_trace` | Never put Ornith <think>/reasoning_content into paid context | forbidden by the catalogue |
| `web_or_computer_use` | No network/computer-use from local specialist | forbidden by the catalogue |
| `lie_tests_passed` | Must not claim PASS if original lacks it; NLI/faithfulness reject | forbidden by the catalogue |
| `impersonate_agent_reply` | Local must never author user-facing agent chat replies; host-UI labels only | forbidden by the catalogue |
| `auto_run_suggested_commands` | Commands/queries suggested by local text must never auto-execute | forbidden by the catalogue |
| `exfiltrate_store_content` | Sidecar/store payloads never leave the machine; ledger and telemetry carry counts only | forbidden by the catalogue |
| `distill_permission_surface` | Permission prompts, approval dialogs and safety warnings are never distilled or rewritten | forbidden by the catalogue |
| `distill_user_prompt` | User-authored messages are never locally compressed or rewritten before the paid model | forbidden by the catalogue |
| `fabricate_handles` | Never mint or answer for handle IDs that don't exist in scope | forbidden by the catalogue |
| `silent_model_downgrade` | Host/local must never silently change the user-selected paid model or reasoning effort | forbidden by the catalogue |
| `overstate_savings_ledger` | Ledger must never count estimates/caps as measured savings, nor skipped items as saved | forbidden by the catalogue |

## 3. `parte-a.md` — as oportunidades por micro-ação

| # | micro-ação | seam | flag | status | guarda / motivo |
| --- | --- | --- | --- | --- | --- |
| 1 | Recortar o prompt-base por turno (system + AGENTS.md + skills ≈ 29k) | montagem do prompt da sessão | `E9_prompt_blocks` | planned | whitelist de blocos obrigatórios |
| 2 | Classificar o payload antes de injetar (11 tipos) | jev_post_process_tool_result | `E1_payload_classify` | planned | piso de confiança; abaixo dele passa inteiro |
| 3 | Compressão generativa da saída grande (≤1/3, prompt próprio) | jev_post_process_tool_result | `E3_cheap_compress` | planned | store-before-loss + read-back + gate de literal |
| 4 | Seleção extrativa por relevância antes de qualquer LLM | idem | `E2_importance_extract` | planned | determinístico; âncora nas últimas N linhas |
| 5 | Score de importância por linha (erros, file:line, paths, últimas N) | idem | `E2_importance_extract` | planned | elide só o miolo, com marcador |
| 6 | Dedup cross-turno / reuso de leitura (path+range+hash) | jev_post_process_tool_result | `E4_read_reuse` | implemented | gate: bytes idênticos; teste em jev.rs |
| 7 | Detector de token volátil (cache do provider) | pré-envio da rodada | `E10_crushers` | planned | diagnóstico + ordenação; nunca reescreve conteúdo |
| 8 | Recuperação sob demanda (handle + expandir o original) | store do harness + read_file | `E11_handles` | planned | read-back byte-exact |
| 9 | Passages/guardrail de conteúdo não confiável | C6 (injeção) + web/memória | `C6 (existente)` | mapped | C6 existe e está off por custo; o classificador determinístico novo alimenta a decisão |
| 10 | Slim de schema de tools por turno | poda de tools (P1) | `P1 (existente)` | mapped | P1 já poda famílias; poda de parâmetros é o segundo nível — deferred (risco de remover obrigatório) |
| 11 | Resumo da compactação no modelo barato | session/compaction | `E5_cheap_task` | planned | contrato com prefixo/último segmento fixos; cai para o frontier se falhar |
| 12 | Título/resumo de sessão, changelog, mensagem de commit | E5_cheap_task (tarefa registrada) | `E5_cheap_task` | planned | sem segurança envolvida |
| 13 | Imagens/screenshots/anexos | — | `—` | deferred | precisa de Vision; não há modelo local nem visão no provider barato |
| 14 | Extração de dados estruturados de saída (paths, PASS/FAIL, JSON, status) | C2/C5/C7 + tarefas registradas | `E5_cheap_task` | planned | determinístico primeiro, modelo barato quando o determinístico não fecha |
| 15 | Pré-computar o que o próximo turno vai pedir (prefetch) | — | `—` | deferred | só leitura, mas exige fila do turno; sem seam seguro aqui hoje |
| 16 | "Isso que eu li responde à pergunta?" por trecho | suficiência (noul por trecho) | `E7_lane_choice` | planned | 4 nouls, ≥2/3 excluídos |
| 17 | "Preciso ler mais um arquivo ou já sei o suficiente?" | suficiência antes de ler | `E7_lane_choice` | planned | noul por candidato |
| 18 | "Esta saída é confiável/usável?" | C2/C4 cobrem falha e diff | `C2/C4 + E1_payload_classify` | mapped | o classificador novo responde o caso placeholder/CoT |
| 19 | Plan mode / próximos passos | classe do próximo passo | `E7_lane_choice` | planned | reduz turnos exploratórios |
| 20 | Prioridade de contexto sob pressão (o que soltar primeiro) | D1/D2 + blocos do prompt | `E9_prompt_blocks` | planned | lossless antes de lossy |
| 21 | Escolher entre 3 saídas do modelo barato | gate de fidelidade da lane | `E3_cheap_compress` | planned | escolhe a que preserva os literais quando há empate |
| 22 | "O turno terminou?" / "faltou algo?" | C1/C3 | `C1/C3 (existentes)` | mapped | já fiado |
| 23 | Coalescer as decisões do turno em 1 request por ponto de decisão | baterias do Jev | `E7_lane_choice` | planned | mede requests por turno |
| 24 | Idle/pressão — pular quando não vale | gate de custo por lane | `E8_lane_breaker` | planned | skip-set do que não comprime |
| 25 | Deadline por chamada + circuit breaker por lane | orçamento + breaker | `E8_lane_breaker` | planned | trip por turno, registrado |
| 26 | Skip-set do que sabidamente não comprime | classificador + skip-set | `E1_payload_classify` | planned | evita gastar com payload incomprimível |
| 27 | Serializar o caminho barato (uma geração por vez) | fila da lane barata | `E8_lane_breaker` | planned | o harness pode disparar várias chamadas; a fila é por processo |
