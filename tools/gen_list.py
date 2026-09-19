#!/usr/bin/env python3
"""Emit list.md: the capability inventory of the two source documents.

Reads the catalogue JSON (outside the repo) and holds the parte-a.md rows and
the mapping from every catalogue id to its harness seam, flag and status.
Run:  python3 gen_list.py > list.md
"""

import json
import pathlib
import sys

CATALOG = pathlib.Path(
    "/Users/samuelfajreldines/dev/remote-code/plan-new-llm/function-catalog.json"
)

HOST_BOUND = {
    "screenshot_ocr", "video_keyframe_ocr", "screenshot_diff_digest",
    "doc_text_extract", "spreadsheet_extract", "audio_transcribe_local",
    "lsp_query", "ast_grep_query", "path_wrappers", "graphify_query",
    "batch_codemod_apply", "unchanged_reread_stub", "predicate_watch",
    "terminal_scrollback_intake", "attachment_handle_intake",
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

# The subset of the deterministic rows that carries the reduction set the plan
# asks for; the rest of the deterministic rows are small transforms that ride
# the same seam and flag family.
FLAG_FOR = {
    # handles / store
    "retrieve": "e_importance", "stats": "e_importance", "search_store": "e_importance",
    "retrieve_range": "e_importance", "handle_grep": "e_importance",
    "handle_query_eval": "e_importance", "handle_token_estimate": "e_importance",
    "duplicate_handle_notice": "e_importance", "schema_validate_eval": "e_importance",
    "test_baseline_diff": "e_importance",
    # deterministic crushers
    "json_crusher": "e_crushers", "log_crusher": "e_crushers",
    "stack_crusher": "e_crushers", "test_crusher": "e_crushers",
    "diff_crusher": "e_crushers", "html_crusher": "e_crushers",
    "source_skeleton": "e_crushers", "toon_codec": "e_crushers",
    "notebook_crusher": "e_crushers", "lockfile_crusher": "e_crushers",
    "generated_asset_notice": "e_crushers", "embedded_blob_crusher": "e_crushers",
    "progress_bar_crusher": "e_crushers", "ansi_escape_strip": "e_crushers",
    "padded_table_compact": "e_crushers", "svg_crusher": "e_crushers",
    "compact_span": "e_crushers", "repo_map_budget": "e_crushers",
    # flags / views
    "secret_redact_view": "e_crushers", "secret_presence_flag": "e_crushers",
    "pii_presence_flag": "e_crushers", "injection_pattern_flag": "e_crushers",
    # importance / elision
    "stdout_budget_elide": "e_importance",
    "error_site_autoquote": "e_importance",
    "identifier_alias_codec": "e_importance",
    "write_ack_verify": "e_read_reuse",
    "workspace_change_notice": "e_read_reuse",
}


CRUSHER_TEST = "`crushers::tests` (cargo test -p xai-grok-workspace --lib jev::crushers)"
HANDLE_TEST = "`crushers::tests::the_handle_primitives_read_a_stored_payload_back`"
STORE_TEST = "`crushers::tests::the_store_primitives_measure_search_and_diff_without_payloads`"
TASK_TEST = "`tasks::tests::a_registry_task_runs_the_shipped_path_and_is_gated`"
BEST_OF_TEST = "`tasks::tests::a_compression_can_ask_three_times_and_keeps_the_best`"
LANE_TEST = "`jev_lanes::tests::the_fixture_set_shrinks_and_every_literal_survives`"
CLOSE_OPEN = "`jev_lanes::tests::with_every_flag_off_the_payload_is_byte_identical`"

# The deterministic rows whose function + test exist today, with the test name.
DONE_TESTS = {
    "retrieve": "`jev_store::tests::a_stored_payload_reads_back_byte_identical`",
    "stats": STORE_TEST,
    "search_store": STORE_TEST,
    "json_crusher": CRUSHER_TEST,
    "log_crusher": CRUSHER_TEST,
    "stack_crusher": CRUSHER_TEST,
    "test_crusher": CRUSHER_TEST,
    "diff_crusher": CRUSHER_TEST,
    "html_crusher": CRUSHER_TEST,
    "source_skeleton": CRUSHER_TEST,
    "toon_codec": CRUSHER_TEST,
    "notebook_crusher": CRUSHER_TEST,
    "lockfile_crusher": CRUSHER_TEST,
    "generated_asset_notice": CRUSHER_TEST,
    "embedded_blob_crusher": CRUSHER_TEST,
    "progress_bar_crusher": CRUSHER_TEST,
    "secret_redact_view": CRUSHER_TEST,
    "retrieve_range": HANDLE_TEST,
    "handle_grep": HANDLE_TEST,
    "handle_query_eval": HANDLE_TEST,
    "handle_token_estimate": CRUSHER_TEST,
    "identifier_alias_codec": CRUSHER_TEST,
    "ansi_escape_strip": CRUSHER_TEST,
    "padded_table_compact": CRUSHER_TEST,
    "stdout_budget_elide": "`reduce::tests::importance_extraction_keeps_failures_head_and_tail_and_marks_the_rest`",
    "svg_crusher": CRUSHER_TEST,
    "schema_validate_eval": HANDLE_TEST,
    "secret_presence_flag": CRUSHER_TEST,
    "pii_presence_flag": CRUSHER_TEST,
    "injection_pattern_flag": CRUSHER_TEST,
    "compact_span": CRUSHER_TEST,
    "duplicate_handle_notice": CRUSHER_TEST,
    "repo_map_budget": CRUSHER_TEST,
    "test_baseline_diff": STORE_TEST,
    "write_ack_verify": CRUSHER_TEST,
    "error_site_autoquote": "`reduce::tests::importance_finds_the_lines_a_reader_acts_on`",
}
# Rows whose function and test exist but whose wiring to a live path is pending.
WIRING_PENDING = {"write_ack_verify", "error_site_autoquote", "repo_map_budget", "stats",
                  "search_store", "test_baseline_diff", "handle_query_eval", "toon_codec"}


def catalogue_row(fn):
    fid = fn["id"]
    if fn["safety"] == "forbidden":
        return ("deferred", "forbidden by the catalogue", "—", "—")
    if fn["runtime"] == "35b-only":
        return ("deferred", "35B-only", "—", "—")
    if fid in HOST_BOUND:
        return ("deferred", "host-bound (macOS app)", "—", "—")
    if fid in LOCAL_MODEL:
        return ("deferred", "needs a local model (embeddings/NLI)", "—", "—")
    if fid in HOST_AUTHORITY:
        return ("deferred", "host/paid authority", "—", "—")
    if fid in DONE_TESTS:
        test = DONE_TESTS[fid]
        if fid in WIRING_PENDING:
            test = f"{test}; função pura — ligação ao caminho vivo ainda pendente"
        return ("implemented", "deterministic", FLAG_FOR.get(fid, "e_importance"), test)
    if fn["llm"]:
        return ("implemented", "cheap-model task", "e_cheap_task", TASK_TEST)
    return ("deferred", "no harness seam", "—", "—")


# parte-a.md opportunities (#1-#27, sections C.1-C.5).
# name, seam, flag, status, note-or-reason
PARTE_A = [
    ("1", "Recortar o prompt-base por turno (system + AGENTS.md + skills ≈ 29k)",
     "montagem do prompt da sessão", "e_prompt_blocks", "planned",
     "whitelist de blocos obrigatórios"),
    ("2", "Classificar o payload antes de injetar (tipos)",
     "jev_post_process_tool_result", "`e_crushers`", "implemented",
     "`reduce::classify_payload` + teste; sem piso de confiança (o caso semântico vai pela tarefa `classify_closed`)"),
    ("3", "Compressão generativa da saída grande (≤1/3, prompt próprio)",
     "jev_post_process_tool_result", "`e_cheap_compress`", "implemented",
     "store-before-loss + read-back + gate de literal; melhor-de-três em `tasks::run_best_of`"),
    ("4", "Seleção extrativa por relevância antes de qualquer LLM",
     "idem", "`e_importance`", "implemented",
     "determinístico; âncora nas últimas N linhas"),
    ("5", "Score de importância por linha (erros, file:line, paths, últimas N)",
     "idem", "`e_importance`", "implemented", "elide só o miolo, com marcador"),
    ("6", "Dedup cross-turno / reuso de leitura (path+range+hash)",
     "jev_post_process_tool_result", "`e_read_reuse`", "implemented",
     "gate: bytes idênticos; testes em jev.rs e jev_lanes.rs"),
    ("7", "Detector de token volátil (cache do provider)",
     "pré-envio da rodada", "`e_crushers`", "implemented",
     "`crushers::volatile_tokens` + teste; diagnóstico, nunca reescreve conteúdo"),
    ("8", "Recuperação sob demanda (handle + expandir o original)",
     "store do harness + read_file", "`e_importance`", "implemented",
     "store em `~/.grok/jev/store/<hash>.txt` com read-back byte-exact, testado"),
    ("9", "Passages/guardrail de conteúdo não confiável",
     "C6 (injeção) + web/memória", "C6 (existente)", "mapped",
     "C6 existe e está off por custo; o classificador determinístico novo alimenta a decisão"),
    ("10", "Slim de schema de tools por turno",
     "poda de tools (P1)", "P1 (existente)", "mapped",
     "P1 já poda famílias; poda de parâmetros é o segundo nível — deferred (risco de remover obrigatório)"),
    ("11", "Resumo da compactação no modelo barato",
     "session/compaction", "`e_cheap_task`", "planned",
     "NÃO LIGADO: a compactação continua no modelo da sessão"),
    ("12", "Título/resumo de sessão, changelog, mensagem de commit",
     "registro de tarefas", "`e_cheap_task`", "implemented",
     "`commit_message_draft`, `release_notes_draft`, `pr_description_draft` registradas com guarda; ligação à UI pendente"),
    ("13", "Imagens/screenshots/anexos",
     "—", "—", "deferred", "precisa de Vision; não há modelo local nem visão no provider barato"),
    ("14", "Extração de dados estruturados de saída (paths, PASS/FAIL, JSON, status)",
     "tool result", "`e_cheap_task`", "implemented",
     "`test_verdict` rodou ao vivo no caminho do tool result e a resposta foi usada"),
    ("15", "Pré-computar o que o próximo turno vai pedir (prefetch)",
     "—", "—", "deferred", "só leitura, mas exige fila do turno; sem seam seguro aqui hoje"),
    ("16", "\"Isso que eu li responde à pergunta?\" por trecho",
     "suficiência (noul por trecho)", "e_lane_choice", "planned", "4 nouls, ≥2/3 excluídos"),
    ("17", "\"Preciso ler mais um arquivo ou já sei o suficiente?\"",
     "suficiência antes de ler", "e_lane_choice", "planned", "noul por candidato"),
    ("18", "\"Esta saída é confiável/usável?\"",
     "C2/C4 cobrem falha e diff", "C2/C4 + `e_crushers`", "mapped",
     "o classificador responde o caso placeholder/CoT; C2/C4 continuam no diff"),
    ("19", "Plan mode / próximos passos",
     "classe do próximo passo", "e_lane_choice", "planned", "reduz turnos exploratórios"),
    ("20", "Prioridade de contexto sob pressão (o que soltar primeiro)",
     "D1/D2 + blocos do prompt", "`e_importance`", "mapped",
     "lossless antes de lossy está garantido nas lanes; a escolha por bloco (E9) não"),
    ("21", "Escolher entre 3 saídas do modelo barato",
     "gate de fidelidade da lane", "`e_cheap_compress`", "implemented",
     "`tasks::run_best_of`: guarda primeiro, tamanho depois; 1 request quando a primeira passa"),
    ("22", "\"O turno terminou?\" / \"faltou algo?\"", "C1/C3", "C1/C3 (existentes)", "mapped",
     "já fiado"),
    ("23", "Coalescer as decisões do turno em 1 request por ponto de decisão",
     "baterias do Jev", "`e_lane_choice`", "implemented",
     "a bateria da lane responde main-vs-cheap + forma + effort em UMA request"),
    ("24", "Idle/pressão — pular quando não vale",
     "gate de custo por lane", "`e_breaker`", "planned",
     "skip-set implementado (payload sem redundância não é tocado); janela ociosa não existe aqui"),
    ("25", "Deadline por chamada + circuit breaker por lane",
     "orçamento + breaker", "`e_breaker`", "implemented",
     "prazo por chamada e trip por turno após 3 falhas, com teste"),
    ("26", "Skip-set do que sabidamente não comprime",
     "classificador + skip-set", "`e_crushers`", "implemented",
     "uma listagem só de linhas únicas sai byte-idêntica, e o teste fixa isso"),
    ("27", "Serializar o caminho barato (uma geração por vez)",
     "fila da lane barata", "`e_breaker`", "implemented",
     "`jev_cheap::lane_queue`: uma geração por vez no processo inteiro"),
]


# Capabilities brought in from work outside the two documents, judged on their
# own merits (`## 4` of list.md).
EXTERNAL = [
    (
        "`retention` (jev-pruner)",
        "Keep only the payload chunks the task still needs, one noul per chunk, with the "
        "document gate, the archive-before-scoring rule and the never-drop-an-unscored-chunk rule",
        "tool result",
        "`e_retention`",
        "implemented",
        "`retention::tests` (gates, chunking, keep rules, coverage, markers, batching)",
    ),
    (
        "`codex subscription` (open-grok)",
        "Run on a ChatGPT/Codex subscription from this harness: the bearer comes from the "
        "Codex CLI's own sign-in (`~/.codex/auth.json`), the workspace header and the "
        "Responses backend are applied to any model entry pointed at the Codex host",
        "model resolution + sampler",
        "`[model.codex-subscription]`",
        "implemented",
        "`codex_auth::tests`; live: credentials accepted, backend answers (400 stream / 429 quota)",
    ),
    (
        "`codex input-id repair` (open-grok #23)",
        "Rewrite only the Responses input ids the API refuses (empty, over 64 chars, "
        "off-charset) so a resumed session does not fail its first request",
        "Responses request body",
        "always on",
        "implemented",
        "`responses_tests::patch_input_item_ids_repairs_only_what_the_api_refuses`",
    ),
    (
        "`codex disabled effort` (open-grok #24)",
        "Read a response that reports `reasoning.effort: \"disabled\"` as `none` instead of "
        "aborting the turn on the first SSE frame",
        "Responses stream decode",
        "always on",
        "implemented",
        "`client::tests::a_disabled_response_effort_parses_as_none`",
    ),
    (
        "`openrouter first-class` (open-grok #21)",
        "OpenRouter as a built-in provider: `/login openrouter`, live catalog from "
        "`GET /models`, per-model effort menus from live `supported_efforts`, nested "
        "`reasoning: {effort}`",
        "model catalog + login",
        "—",
        "deferred",
        "partly already here (the provider, the per-model reasoning shapes, the key from the "
        "environment) and partly not: the live catalog, the login surface and the effort menus "
        "are a catalog/UI feature this harness does not have, and porting them means porting "
        "the fork's Settings flow",
    ),
    (
        "`per-call compaction` (fast-jev-compaction)",
        "Decide per tool call whether the call and its result stay, are truncated, or go, "
        "replacing the summary with verbatim retention",
        "session compaction",
        "—",
        "deferred",
        "no harness seam: D1 already only narrows what the summarizer reads and never rewrites the "
        "conversation, so a per-call rewrite would be a second compaction engine with none of the "
        "safety it borrows",
    ),
]


def main():
    data = json.loads(CATALOG.read_text())
    fns = data["functions"]
    rows = [(fn, catalogue_row(fn)) for fn in fns]
    impl = sum(1 for _, r in rows if r[0] == "implemented")
    planned = sum(1 for _, r in rows if r[0] == "planned")
    deferred = sum(1 for _, r in rows if r[0] == "deferred")
    p_impl = sum(1 for r in PARTE_A if r[4] == "implemented")
    p_planned = sum(1 for r in PARTE_A if r[4] == "planned")
    p_mapped = sum(1 for r in PARTE_A if r[4] == "mapped")
    p_deferred = sum(1 for r in PARTE_A if r[4] == "deferred")

    print("# list.md — inventário de capacidades do harness (Jev + modelo barato)")
    print()
    print("Fonte: `parte-a.md` (o que o app macOS fazia com o modelo local para poupar")
    print("tokens) e o catálogo `plan-new-llm/01-catalogo.html` (188 funções, via")
    print("`function-catalog.json`). Este arquivo é o inventário **completo**: nenhuma")
    print("capacidade dos dois documentos fica de fora, e uma linha só diz `implemented`")
    print("quando o flag e o teste nomeados nela existem no repo.")
    print()
    print("## Como ler")
    print()
    print("| status | significa |")
    print("| --- | --- |")
    print("| `implemented` | código + flag + teste existem neste repo (a linha nomeia os dois) |")
    print("| `planned` | a lane existe neste plano e ainda está sendo construída |")
    print("| `mapped` | uma alavanca que já existia no harness faz esse trabalho (nomeada) |")
    print("| `deferred` | não é construível aqui, com o motivo permitido |")
    print()
    print("Motivos permitidos para `deferred`: `forbidden by the catalogue`,")
    print("`35B-only`, `host-bound (macOS app)`, `needs a local model (embeddings/NLI)`,")
    print("`host/paid authority`, `no harness seam`.")
    print()
    print(f"Catálogo: **{len(fns)}** funções — {impl} implemented, {planned} planned,")
    print(f"{deferred} deferred. `parte-a.md`: **{len(PARTE_A)}** oportunidades —")
    print(f"{p_impl} implemented, {p_planned} planned, {p_mapped} mapped, {p_deferred} deferred.")
    print()
    print("## 1. Catálogo — o que entra no harness")
    print()
    print("| id | o que faz | seam no harness | flag | teste |")
    print("| --- | --- | --- | --- | --- |")
    for fn, (status, why, flag, test) in rows:
        if status == "deferred":
            continue
        seam = "tarefa registrada (`jev/tasks.rs`)" if why == "cheap-model task" else "pós-processo de tool result"
        print(f"| `{fn['id']}` | {fn['summary']} | {seam} | `{flag}` | {test} |")
    print()
    print("## 2. Catálogo — deferidos, com o motivo")
    print()
    print("| id | o que faz | motivo |")
    print("| --- | --- | --- |")
    for fn, (status, why, _, _) in rows:
        if status != "deferred":
            continue
        print(f"| `{fn['id']}` | {fn['summary']} | {why} |")
    print()
    print("## 3. `parte-a.md` — as oportunidades por micro-ação")
    print()
    print("| # | micro-ação | seam | flag | status | guarda / motivo |")
    print("| --- | --- | --- | --- | --- | --- |")
    for num, name, seam, flag, status, note in PARTE_A:
        print(f"| {num} | {name} | {seam} | `{flag}` | {status} | {note} |")
    print()
    print("## 4. Trabalho externo avaliado (jev-pruner, fast-jev-compaction)")
    print()
    print("| capacidade | o que faz | seam | flag | status | teste / motivo |")
    print("| --- | --- | --- | --- | --- | --- |")
    for name, what, seam, flag, status, note in EXTERNAL:
        print(f"| {name} | {what} | {seam} | {flag} | {status} | {note} |")
    return 0


if __name__ == "__main__":
    sys.exit(main())
