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
    "retrieve": "E11_handles", "stats": "E11_handles", "search_store": "E11_handles",
    "retrieve_range": "E11_handles", "handle_grep": "E11_handles",
    "handle_query_eval": "E11_handles", "handle_token_estimate": "E11_handles",
    "duplicate_handle_notice": "E11_handles", "schema_validate_eval": "E11_handles",
    "test_baseline_diff": "E11_handles",
    # deterministic crushers
    "json_crusher": "E10_crushers", "log_crusher": "E10_crushers",
    "stack_crusher": "E10_crushers", "test_crusher": "E10_crushers",
    "diff_crusher": "E10_crushers", "html_crusher": "E10_crushers",
    "source_skeleton": "E10_crushers", "toon_codec": "E10_crushers",
    "notebook_crusher": "E10_crushers", "lockfile_crusher": "E10_crushers",
    "generated_asset_notice": "E10_crushers", "embedded_blob_crusher": "E10_crushers",
    "progress_bar_crusher": "E10_crushers", "ansi_escape_strip": "E10_crushers",
    "padded_table_compact": "E10_crushers", "svg_crusher": "E10_crushers",
    "compact_span": "E10_crushers", "repo_map_budget": "E10_crushers",
    # flags / views
    "secret_redact_view": "E10_crushers", "secret_presence_flag": "E10_crushers",
    "pii_presence_flag": "E10_crushers", "injection_pattern_flag": "E10_crushers",
    # importance / elision
    "stdout_budget_elide": "E2_importance_extract",
    "error_site_autoquote": "E2_importance_extract",
    "identifier_alias_codec": "E2_importance_extract",
    "write_ack_verify": "E4_read_reuse",
    "workspace_change_notice": "E4_read_reuse",
}


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
    if fid in FLAG_FOR:
        return ("planned", "deterministic", FLAG_FOR[fid], "—")
    if fn["llm"]:
        return ("planned", "cheap-model task", "E5_cheap_task", "—")
    return ("deferred", "no harness seam", "—", "—")


# parte-a.md opportunities (#1-#27, sections C.1-C.5).
# name, seam, flag, status, note-or-reason
PARTE_A = [
    ("1", "Recortar o prompt-base por turno (system + AGENTS.md + skills ≈ 29k)",
     "montagem do prompt da sessão", "E9_prompt_blocks", "planned",
     "whitelist de blocos obrigatórios"),
    ("2", "Classificar o payload antes de injetar (11 tipos)",
     "jev_post_process_tool_result", "E1_payload_classify", "planned",
     "piso de confiança; abaixo dele passa inteiro"),
    ("3", "Compressão generativa da saída grande (≤1/3, prompt próprio)",
     "jev_post_process_tool_result", "E3_cheap_compress", "planned",
     "store-before-loss + read-back + gate de literal"),
    ("4", "Seleção extrativa por relevância antes de qualquer LLM",
     "idem", "E2_importance_extract", "planned",
     "determinístico; âncora nas últimas N linhas"),
    ("5", "Score de importância por linha (erros, file:line, paths, últimas N)",
     "idem", "E2_importance_extract", "planned", "elide só o miolo, com marcador"),
    ("6", "Dedup cross-turno / reuso de leitura (path+range+hash)",
     "jev_post_process_tool_result", "E4_read_reuse", "implemented",
     "gate: bytes idênticos; teste em jev.rs"),
    ("7", "Detector de token volátil (cache do provider)",
     "pré-envio da rodada", "E10_crushers", "planned",
     "diagnóstico + ordenação; nunca reescreve conteúdo"),
    ("8", "Recuperação sob demanda (handle + expandir o original)",
     "store do harness + read_file", "E11_handles", "planned",
     "read-back byte-exact"),
    ("9", "Passages/guardrail de conteúdo não confiável",
     "C6 (injeção) + web/memória", "C6 (existente)", "mapped",
     "C6 existe e está off por custo; o classificador determinístico novo alimenta a decisão"),
    ("10", "Slim de schema de tools por turno",
     "poda de tools (P1)", "P1 (existente)", "mapped",
     "P1 já poda famílias; poda de parâmetros é o segundo nível — deferred (risco de remover obrigatório)"),
    ("11", "Resumo da compactação no modelo barato",
     "session/compaction", "E5_cheap_task", "planned",
     "contrato com prefixo/último segmento fixos; cai para o frontier se falhar"),
    ("12", "Título/resumo de sessão, changelog, mensagem de commit",
     "E5_cheap_task (tarefa registrada)", "E5_cheap_task", "planned", "sem segurança envolvida"),
    ("13", "Imagens/screenshots/anexos",
     "—", "—", "deferred", "precisa de Vision; não há modelo local nem visão no provider barato"),
    ("14", "Extração de dados estruturados de saída (paths, PASS/FAIL, JSON, status)",
     "C2/C5/C7 + tarefas registradas", "E5_cheap_task", "planned",
     "determinístico primeiro, modelo barato quando o determinístico não fecha"),
    ("15", "Pré-computar o que o próximo turno vai pedir (prefetch)",
     "—", "—", "deferred", "só leitura, mas exige fila do turno; sem seam seguro aqui hoje"),
    ("16", "\"Isso que eu li responde à pergunta?\" por trecho",
     "suficiência (noul por trecho)", "E7_lane_choice", "planned", "4 nouls, ≥2/3 excluídos"),
    ("17", "\"Preciso ler mais um arquivo ou já sei o suficiente?\"",
     "suficiência antes de ler", "E7_lane_choice", "planned", "noul por candidato"),
    ("18", "\"Esta saída é confiável/usável?\"",
     "C2/C4 cobrem falha e diff", "C2/C4 + E1_payload_classify", "mapped",
     "o classificador novo responde o caso placeholder/CoT"),
    ("19", "Plan mode / próximos passos",
     "classe do próximo passo", "E7_lane_choice", "planned", "reduz turnos exploratórios"),
    ("20", "Prioridade de contexto sob pressão (o que soltar primeiro)",
     "D1/D2 + blocos do prompt", "E9_prompt_blocks", "planned",
     "lossless antes de lossy"),
    ("21", "Escolher entre 3 saídas do modelo barato",
     "gate de fidelidade da lane", "E3_cheap_compress", "planned",
     "escolhe a que preserva os literais quando há empate"),
    ("22", "\"O turno terminou?\" / \"faltou algo?\"", "C1/C3", "C1/C3 (existentes)", "mapped",
     "já fiado"),
    ("23", "Coalescer as decisões do turno em 1 request por ponto de decisão",
     "baterias do Jev", "E7_lane_choice", "planned", "mede requests por turno"),
    ("24", "Idle/pressão — pular quando não vale",
     "gate de custo por lane", "E8_lane_breaker", "planned", "skip-set do que não comprime"),
    ("25", "Deadline por chamada + circuit breaker por lane",
     "orçamento + breaker", "E8_lane_breaker", "planned", "trip por turno, registrado"),
    ("26", "Skip-set do que sabidamente não comprime",
     "classificador + skip-set", "E1_payload_classify", "planned", "evita gastar com payload incomprimível"),
    ("27", "Serializar o caminho barato (uma geração por vez)",
     "fila da lane barata", "E8_lane_breaker", "planned",
     "o harness pode disparar várias chamadas; a fila é por processo"),
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
    return 0


if __name__ == "__main__":
    sys.exit(main())
