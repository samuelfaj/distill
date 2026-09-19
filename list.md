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

Catálogo: **188** funções — 129 implemented, 0 planned,
59 deferred. `parte-a.md`: **27** oportunidades —
14 implemented, 6 planned, 5 mapped, 2 deferred.

## 1. Catálogo — o que entra no harness

| id | o que faz | seam no harness | flag | teste |
| --- | --- | --- | --- | --- |
| `retrieve` | Byte-exact original behind [[rc:handle]] | pós-processo de tool result | `e_importance` | `jev_store::tests::a_stored_payload_reads_back_byte_identical` |
| `stats` | Ledger stats, no payloads | pós-processo de tool result | `e_importance` | `crushers::tests::the_store_primitives_measure_search_and_diff_without_payloads`; função pura — ligação ao caminho vivo ainda pendente |
| `search_store` | CCR/reversible-store search by query | pós-processo de tool result | `e_importance` | `crushers::tests::the_store_primitives_measure_search_and_diff_without_payloads`; função pura — ligação ao caminho vivo ainda pendente |
| `json_crusher` | Deterministic JSON/TOON crush | pós-processo de tool result | `e_crushers` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers) |
| `log_crusher` | Collapse repeated log lines | pós-processo de tool result | `e_crushers` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers) |
| `stack_crusher` | Keep frames, drop dumps | pós-processo de tool result | `e_crushers` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers) |
| `test_crusher` | Keep fail names and assertion text | pós-processo de tool result | `e_crushers` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers) |
| `diff_crusher` | Keep file headers and hunks, drop noise | pós-processo de tool result | `e_crushers` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers) |
| `html_crusher` | Strip chrome, keep text | pós-processo de tool result | `e_crushers` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers) |
| `source_skeleton` | Signatures/line map without bodies | pós-processo de tool result | `e_crushers` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers) |
| `toon_codec` | Uniform JSON arrays as TOON | pós-processo de tool result | `e_crushers` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers); função pura — ligação ao caminho vivo ainda pendente |
| `notebook_crusher` | Strip .ipynb outputs/base64 images; keep code+markdown cells | pós-processo de tool result | `e_crushers` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers) |
| `lockfile_crusher` | package-lock/yarn.lock/Cargo.lock/Package.resolved → top-level deps + counts, never full graph | pós-processo de tool result | `e_crushers` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers) |
| `generated_asset_notice` | Binary/minified/generated mega-file → typed notice (size, kind, hash) + handle instead of bytes | pós-processo de tool result | `e_crushers` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers) |
| `embedded_blob_crusher` | base64/data-URI/hexdump islands inside text → typed placeholder + sub-handle | pós-processo de tool result | `e_crushers` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers) |
| `progress_bar_crusher` | Collapse carriage-return progress/spinner frames (npm, pip, docker pull) to final state per bar | pós-processo de tool result | `e_crushers` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers) |
| `secret_redact_view` | Masked view of secret-bearing blob: keys kept, values masked; deterministic masker, fail-closed | pós-processo de tool result | `e_crushers` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers) |
| `repo_map_budget` | Ranked repo map of signatures fitted to a token budget (aider-style) for session boot | pós-processo de tool result | `e_crushers` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers); função pura — ligação ao caminho vivo ainda pendente |
| `retrieve_range` | Byte-exact slice of a handle by line/byte range instead of the whole blob | pós-processo de tool result | `e_importance` | `crushers::tests::the_handle_primitives_read_a_stored_payload_back` |
| `handle_grep` | Exact grep inside handle(s): verbatim match lines + line numbers | pós-processo de tool result | `e_importance` | `crushers::tests::the_handle_primitives_read_a_stored_payload_back` |
| `handle_query_eval` | Run a paid-supplied deterministic query (regex/jq/xpath/line-range) against a handle → counts + sample spans; no LLM involved | pós-processo de tool result | `e_importance` | `crushers::tests::the_handle_primitives_read_a_stored_payload_back`; função pura — ligação ao caminho vivo ainda pendente |
| `handle_token_estimate` | Per-provider token estimate + head/tail preview of a handle, so paid can decide retrieve-or-not | pós-processo de tool result | `e_importance` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers) |
| `identifier_alias_codec` | Reversible aliasing of long UUIDs/hashes/paths to short tokens; expansion table behind a handle | pós-processo de tool result | `e_importance` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers) |
| `ansi_escape_strip` | Strip ANSI color/cursor escape codes that inflate tokenization | pós-processo de tool result | `e_crushers` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers) |
| `padded_table_compact` | Collapse alignment whitespace in columnar CLI output; CSV/TOON re-emit | pós-processo de tool result | `e_crushers` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers) |
| `write_ack_verify` | Successful write/edit tool results → terse ack {lines, hash, applied hunks} instead of full echo; kills the verify re-read | pós-processo de tool result | `e_read_reuse` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers); função pura — ligação ao caminho vivo ainda pendente |
| `stdout_budget_elide` | Generic byte budget on any stdout when no specific crusher matches: head+tail verbatim, middle elided to [[rc:handle]] | pós-processo de tool result | `e_importance` | `reduce::tests::importance_extraction_keeps_failures_head_and_tail_and_marks_the_rest` |
| `svg_crusher` | Strip SVG path-coordinate blobs → structure, text, viewBox; full behind handle | pós-processo de tool result | `e_crushers` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers) |
| `error_site_autoquote` | Parse file:line refs in failing output; host appends the referenced source lines (±N) verbatim so paid skips the follow-up read | pós-processo de tool result | `e_importance` | `reduce::tests::importance_finds_the_lines_a_reader_acts_on`; função pura — ligação ao caminho vivo ainda pendente |
| `schema_validate_eval` | Validate handle content against a paid-supplied JSON Schema/grammar → error list with paths; no LLM involved | pós-processo de tool result | `e_importance` | `crushers::tests::the_handle_primitives_read_a_stored_payload_back` |
| `test_baseline_diff` | Compare current test failures against a stored baseline handle → only NEW and newly-fixed failures; pre-existing flakes stop burning paid attention | pós-processo de tool result | `e_importance` | `crushers::tests::the_store_primitives_measure_search_and_diff_without_payloads`; função pura — ligação ao caminho vivo ainda pendente |
| `distill_command_output` | Extractive compress of noisy stdout with question | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `distill_long_text` | Long prose/log extractive compress | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `watch_summary` | Existing watchSummary mode | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `ask_handle` | Extractive QA over a recovery-scope handle with obligatory spans | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `extract_schema` | Fill closed JSON schema from handle | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `pick_candidates` | Top-k ids from path/symbol list for a question | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `classify_closed` | Closed enum: pass/fail, file role, error class | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `cluster_lines` | Group similar failures; 1 example per group | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `outline_structure` | Headings/functions with line ranges | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `delta_handles` | What changed between two handles | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `cite_spans` | Quotes that must be substrings of original | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `entity_list` | Files, tests, error tokens mentioned in blob | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `test_verdict` | PASS/FAIL + failing names from test stdout | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `command_intent` | Existing CommandOutputIntent classifier | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `filter_line_numbers` | Line numbers matching an NL predicate | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `map_error_to_files` | Stack/error → likely paths | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `json_shape` | Infer keys of a JSON blob without values if secret-like | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `log_records` | Log lines → {ts,level,msg,file} | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `git_theme_summary` | git log/status noisy summary | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `ci_job_failures` | CI log → failed job names | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `lint_group` | Group linter hits by rule | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `coverage_summary` | Coverage % and uncovered files | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `docker_error_tail` | Last error in long docker/build log | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `terraform_plan_digest` | Add/change/destroy counts + names | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `kubectl_digest` | Wide kubectl get output | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `gh_run_digest` | gh run view / api noise | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `linear_issue_digest` | Extract title/AC/labels from issue JSON, no invention | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `mcp_schema_trim` | Filter huge MCP tool catalogs to matching names | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `patch_explain` | Describe a unified diff; never apply | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `chunk_select` | Which line ranges of a file answer a question | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `file_role_classify` | test/impl/config/generated/skill | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `json_path_select` | Which JSON paths match a question | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `table_extract` | Markdown/HTML tables → JSON rows | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `metric_extract` | Numbers from bench output | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `playwright_trace_digest` | Playwright log → failed spec + error | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `xcodebuild_error_extract` | xcodebuild/swift test errors | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `subagent_brief_compact` | Compress explore subagent dump before parent context | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `multi_handle_ask` | One question over N handles | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `constraint_extract` | Acceptance criteria from a ticket blob | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `import_summary` | Import/include list from a file | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `log_timeline` | Timestamped event list | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `graphql_error_extract` | GraphQL errors array digest | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `har_summary` | HAR/network log: status and URLs only | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `sql_explain_digest` | EXPLAIN/analyze output | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `profiler_hotspots` | Top stacks from profiler text | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `flake_classify` | flake vs consistent fail (classify only) | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `boilerplate_strip` | Drop license headers/generated banners | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `graph_ask` | Prose wrapper over graphify query result | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `skill_name_pick` | Pick relevant skill NAMES only; never distill SKILL.md bodies | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `env_key_list` | Env key names, never values | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `duplicate_handle_notice` | This blob already stored as handle X | pós-processo de tool result | `e_importance` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers) |
| `secret_presence_flag` | Regex/heuristic: blob looks secret-bearing; do not echo values | pós-processo de tool result | `e_crushers` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers) |
| `wire_encode` | Existing DistillMode.wireEncode | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `compact_span` | Existing compact span codec | pós-processo de tool result | `e_crushers` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers) |
| `tree_listing_digest` | Huge find/ls -R listing → subtree summary relevant to question | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `mcp_result_digest` | Compress verbose MCP tool results (question-aware) where the provider hook supports rewrite | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `grep_hits_rank` | Rank/group hits of an already-stored grep dump handle; voluntary paid call only — never auto-distills rg/grep | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `context_pack` | Question → ranked reading list of handles, paths+ranges and skeletons; no prose claims | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `dependency_graph_digest` | npm ls / pip freeze / SwiftPM resolve output → direct deps, versions, conflicts | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `crash_report_digest` | macOS .ips/crash log → exception type, faulting thread, top frames, relevant binary images | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `blame_digest` | git blame output → per-range author/commit/date relevant to question | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `merge_conflict_digest` | Conflicted files + per-conflict ours/theirs summary; conflict markers quoted verbatim | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `pr_thread_digest` | gh pr review/comment JSON → unresolved threads {file, line, ask} | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `chat_thread_digest` | Slack/comment thread JSON → participants, decisions, open questions with spans | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `help_flags_extract` | --help/man output → flags and subcommands relevant to question | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `openapi_digest` | OpenAPI/GraphQL SDL spec → endpoints/types matching question | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `db_schema_digest` | sqlite .schema / SHOW CREATE dump → relevant tables, columns, indexes | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `tabular_digest` | Big CSV/TSV → columns, row count, question-relevant sample rows | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `compiler_diagnostics_extract` | Any compiler/typechecker output (tsc, cargo, go, javac, swiftc) → {file, line, severity, message} | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `bundler_build_digest` | webpack/vite/next build output → errors, warnings, emitted sizes | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `snapshot_diff_digest` | Snapshot-test failure diffs → minimal changed-subtree summary | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `pkg_install_digest` | brew/apt/npm install and upgrade logs → installed versions + warnings/errors | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `http_response_digest` | curl/httpie output → status, key headers, body digest relevant to question | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `process_snapshot_digest` | ps/lsof/netstat snapshots → entries matching question (orphans, sockets, ports) | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `sanitizer_report_digest` | ASan/TSan/UBSan report → leak/race class + key frames | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `syscall_trace_digest` | strace/dtruss/fs_usage trace → files/sockets touched + error syscalls relevant to question | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `debugger_output_digest` | lldb/gdb session output → relevant frames, variables, breakpoint hits | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `memory_report_digest` | vmmap/leaks/footprint output → top regions and allocations relevant to question | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `binary_inspect_digest` | nm/otool/objdump/strings output → symbols and sections matching question | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `lighthouse_digest` | Lighthouse/axe JSON → scores + top violations grouped | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `vuln_scan_digest` | npm audit/trivy/grype JSON → {package, severity, fixedIn} grouped | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `gitlab_api_digest` | glab/GitLab API JSON (MRs, pipelines, discussions) → unresolved threads + failed jobs | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `email_thread_digest` | Email chains (.eml/mbox/M365 JSON) → participants, latest ask, decisions; quote-chain deduped | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `meeting_transcript_digest` | Meeting transcript → decisions, action items, owners, with spans | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `changelog_range_extract` | CHANGELOG/release notes → entries between two versions | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `subagent_outcome_classify` | Subagent output → done/partial/failed/off-task with evidence spans | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `pii_presence_flag` | Heuristic PII detector (emails, names, document numbers) → flag + masked view; keeps customer data out of paid context | pós-processo de tool result | `e_crushers` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers) |
| `multi_log_timeline_merge` | Merge timestamped events across N log handles into one ordered timeline with source tags | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `ui_tree_digest` | Accessibility/DOM tree dumps (browser or computer-use) → elements matching question {role, label, coords} | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `injection_pattern_flag` | Heuristic prompt-injection detector on fetched/untrusted content → flag + quarantine notice; content stays behind handle until paid opts in | pós-processo de tool result | `e_crushers` | `crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers) |
| `commit_message_draft` | Draft commit message from diff handle | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `pr_description_draft` | Draft PR body from diff+tests | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `i18n_key_diff` | Missing translation keys | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `search_query_suggest` | Suggest rg/graphify queries for a question; paid chooses and executes | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `release_notes_draft` | Draft release notes from diff/log handles | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `i18n_translation_draft` | Draft translations for missing resource keys | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `issue_triage_draft` | Suggest labels/duplicates/severity for a new issue from handles | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |
| `sql_query_draft` | NL → SQL draft against a schema digest (live-DB triage) | tarefa registrada (`jev/tasks.rs`) | `e_cheap_task` | `tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated` |

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
| `workspace_change_notice` | FSEvents-based list of files changed since a given turn + per-handle staleness check; paid re-reads only those | no harness seam |
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
| 1 | Recortar o prompt-base por turno (system + AGENTS.md + skills ≈ 29k) | montagem do prompt da sessão | `e_prompt_blocks` | planned | whitelist de blocos obrigatórios |
| 2 | Classificar o payload antes de injetar (tipos) | jev_post_process_tool_result | ``e_crushers`` | implemented | `reduce::classify_payload` + teste; sem piso de confiança (o caso semântico vai pela tarefa `classify_closed`) |
| 3 | Compressão generativa da saída grande (≤1/3, prompt próprio) | jev_post_process_tool_result | ``e_cheap_compress`` | implemented | store-before-loss + read-back + gate de literal; melhor-de-três em `tasks::run_best_of` |
| 4 | Seleção extrativa por relevância antes de qualquer LLM | idem | ``e_importance`` | implemented | determinístico; âncora nas últimas N linhas |
| 5 | Score de importância por linha (erros, file:line, paths, últimas N) | idem | ``e_importance`` | implemented | elide só o miolo, com marcador |
| 6 | Dedup cross-turno / reuso de leitura (path+range+hash) | jev_post_process_tool_result | ``e_read_reuse`` | implemented | gate: bytes idênticos; testes em jev.rs e jev_lanes.rs |
| 7 | Detector de token volátil (cache do provider) | pré-envio da rodada | ``e_crushers`` | implemented | `crushers::volatile_tokens` + teste; diagnóstico, nunca reescreve conteúdo |
| 8 | Recuperação sob demanda (handle + expandir o original) | store do harness + read_file | ``e_importance`` | implemented | store em `~/.grok/jev/store/<hash>.txt` com read-back byte-exact, testado |
| 9 | Passages/guardrail de conteúdo não confiável | C6 (injeção) + web/memória | `C6 (existente)` | mapped | C6 existe e está off por custo; o classificador determinístico novo alimenta a decisão |
| 10 | Slim de schema de tools por turno | poda de tools (P1) | `P1 (existente)` | mapped | P1 já poda famílias; poda de parâmetros é o segundo nível — deferred (risco de remover obrigatório) |
| 11 | Resumo da compactação no modelo barato | session/compaction | ``e_cheap_task`` | planned | NÃO LIGADO: a compactação continua no modelo da sessão |
| 12 | Título/resumo de sessão, changelog, mensagem de commit | registro de tarefas | ``e_cheap_task`` | implemented | `commit_message_draft`, `release_notes_draft`, `pr_description_draft` registradas com guarda; ligação à UI pendente |
| 13 | Imagens/screenshots/anexos | — | `—` | deferred | precisa de Vision; não há modelo local nem visão no provider barato |
| 14 | Extração de dados estruturados de saída (paths, PASS/FAIL, JSON, status) | tool result | ``e_cheap_task`` | implemented | `test_verdict` rodou ao vivo no caminho do tool result e a resposta foi usada |
| 15 | Pré-computar o que o próximo turno vai pedir (prefetch) | — | `—` | deferred | só leitura, mas exige fila do turno; sem seam seguro aqui hoje |
| 16 | "Isso que eu li responde à pergunta?" por trecho | suficiência (noul por trecho) | `e_lane_choice` | planned | 4 nouls, ≥2/3 excluídos |
| 17 | "Preciso ler mais um arquivo ou já sei o suficiente?" | suficiência antes de ler | `e_lane_choice` | planned | noul por candidato |
| 18 | "Esta saída é confiável/usável?" | C2/C4 cobrem falha e diff | `C2/C4 + `e_crushers`` | mapped | o classificador responde o caso placeholder/CoT; C2/C4 continuam no diff |
| 19 | Plan mode / próximos passos | classe do próximo passo | `e_lane_choice` | planned | reduz turnos exploratórios |
| 20 | Prioridade de contexto sob pressão (o que soltar primeiro) | D1/D2 + blocos do prompt | ``e_importance`` | mapped | lossless antes de lossy está garantido nas lanes; a escolha por bloco (E9) não |
| 21 | Escolher entre 3 saídas do modelo barato | gate de fidelidade da lane | ``e_cheap_compress`` | implemented | `tasks::run_best_of`: guarda primeiro, tamanho depois; 1 request quando a primeira passa |
| 22 | "O turno terminou?" / "faltou algo?" | C1/C3 | `C1/C3 (existentes)` | mapped | já fiado |
| 23 | Coalescer as decisões do turno em 1 request por ponto de decisão | baterias do Jev | ``e_lane_choice`` | implemented | a bateria da lane responde main-vs-cheap + forma + effort em UMA request |
| 24 | Idle/pressão — pular quando não vale | gate de custo por lane | ``e_breaker`` | planned | skip-set implementado (payload sem redundância não é tocado); janela ociosa não existe aqui |
| 25 | Deadline por chamada + circuit breaker por lane | orçamento + breaker | ``e_breaker`` | implemented | prazo por chamada e trip por turno após 3 falhas, com teste |
| 26 | Skip-set do que sabidamente não comprime | classificador + skip-set | ``e_crushers`` | implemented | uma listagem só de linhas únicas sai byte-idêntica, e o teste fixa isso |
| 27 | Serializar o caminho barato (uma geração por vez) | fila da lane barata | ``e_breaker`` | implemented | `jev_cheap::lane_queue`: uma geração por vez no processo inteiro |

## 4. Trabalho externo avaliado (jev-pruner, fast-jev-compaction)

| capacidade | o que faz | seam | flag | status | teste / motivo |
| --- | --- | --- | --- | --- | --- |
| `retention` (jev-pruner) | Keep only the payload chunks the task still needs, one noul per chunk, with the document gate, the archive-before-scoring rule and the never-drop-an-unscored-chunk rule | tool result | `e_retention` | implemented | `retention::tests` (gates, chunking, keep rules, coverage, markers, batching) |
| `codex subscription` (open-grok) | Run on a ChatGPT/Codex subscription from this harness: the bearer comes from the Codex CLI's own sign-in (`~/.codex/auth.json`), the workspace header and the Responses backend are applied to any model entry pointed at the Codex host | model resolution + sampler | `[model.codex-subscription]` | implemented | `codex_auth::tests`; live: credentials accepted, backend answers (400 stream / 429 quota) |
| `codex input-id repair` (open-grok #23) | Rewrite only the Responses input ids the API refuses (empty, over 64 chars, off-charset) so a resumed session does not fail its first request | Responses request body | always on | implemented | `responses_tests::patch_input_item_ids_repairs_only_what_the_api_refuses` |
| `codex disabled effort` (open-grok #24) | Read a response that reports `reasoning.effort: "disabled"` as `none` instead of aborting the turn on the first SSE frame | Responses stream decode | always on | implemented | `client::tests::a_disabled_response_effort_parses_as_none` |
| `openrouter first-class` (open-grok #21) | OpenRouter as a built-in provider: `/login openrouter`, live catalog from `GET /models`, per-model effort menus from live `supported_efforts`, nested `reasoning: {effort}` | model catalog + login | — | deferred | partly already here (the provider, the per-model reasoning shapes, the key from the environment) and partly not: the live catalog, the login surface and the effort menus are a catalog/UI feature this harness does not have, and porting them means porting the fork's Settings flow |
| `per-call compaction` (fast-jev-compaction) | Decide per tool call whether the call and its result stay, are truncated, or go, replacing the summary with verbatim retention | session compaction | — | deferred | no harness seam: D1 already only narrows what the summarizer reads and never rewrites the conversation, so a per-call rewrite would be a second compaction engine with none of the safety it borrows |

## 5. Superfícies de provedor (pedido direto do dono, 2026-09-18)

O harness roteia para mais de um backend, então o que o provedor anuncia e o
que o dono escolhe precisam estar visíveis onde ele entra: na tela de boas-vindas.

| capacidade | o que faz | seam | flag | status | teste / motivo |
| --- | --- | --- | --- | --- | --- |
| banner do provedor fora | O banner de anúncio que o backend envia não aparece: é marketing de um provedor só, e este harness roteia para vários; a env vence, e sem env a seção decide (ausente = fora) | `acp_handler::settings` (filtro de anúncios) | `[announcements] enabled` / `REMOTE_CODE_ANNOUNCEMENTS=1` | implemented | `announcements_enabled` |
| linhas de login no menu | Tela de boas-vindas ganha `Log in with Grok`, `Log in with Codex`, `Log in with OpenRouter` e `Cheap lane model`; Grok dispara o fluxo real, os outros três abrem o aviso com o estado vivo e o próximo passo exato | menu de boas-vindas + `dispatch_menu_action` | — | implemented | `menu_action_provider_rows_route_to_login_and_notices`, `menu_action_indices_with_import_and_changelog` |
| estado dos provedores | `codex_status` / `openrouter_status` / `cheap_lane_status` leem a mesma fonte que as lanes usam (o `~/.codex/auth.json` do CLI, `OPENROUTER_API_KEY`, `[jev.local]`), sem nunca ecoar token | `slash/commands/provider_status` | — | implemented | `the_codex_status_says_which_file_it_reads_and_what_to_do`, `the_openrouter_status_reports_the_key_without_ever_showing_one`, `the_cheap_lane_status_names_the_model_the_candidates_and_the_knob` |
| escolher o modelo da lane barata | `/cheap-model <entry>` grava `[jev.local].model` no `~/.grok/config.toml` pelo mesmo read-modify-write atômico das outras settings; entry fora das entradas OpenRouter configuradas é recusado com a lista; `/cheap-model` mostra, `/cheap-model clear` limpa | comando slash → `Action::SetCheapModel` → `Effect::PersistSetting("cheap_model")` → `set_jev_local_model` | `[jev.local] model` | implemented | `set_jev_local_model_writes_the_pick_and_leaves_the_lane_alone`, `clearing_the_jev_local_model_reads_back_as_no_cheap_lane`, `merging_the_jev_slice_writes_the_cheap_model_and_preserves_the_lane`, `cheap_model::tests` (bare, clear, unknown entry, verbatim entry) |
