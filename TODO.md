# TODO.md — token saver: o que roda, o que falta e o que não cabe

Inventário das técnicas de economia de tokens deste harness. Cruza o catálogo de
`plan-new-llm` (188 entradas: 57 determinísticas, 99 safe-generate, 13 candidates
e 19 forbidden) com o mapeamento que já existe em [`list.md`](list.md) (129 "no
harness", 59 deferidas) e com o inventário do app macOS em `remote-code`.

Status possíveis:

| status | significa |
| --- | --- |
| `live` | roda hoje no caminho vivo do tool result, com o default do harness |
| `live·gated` | ligado no caminho vivo, mas o portão de literais ou o portão de documento decide se aplica |
| `off` | codificado e testado, com a chave desligada no default, e o motivo registrado |
| `unwired` | codificado e testado, sem nenhum chamador no caminho vivo |
| `deferred` | fora de escopo aqui, com o motivo |
| `forbidden` | proibido pelo próprio catálogo; é regra, não item de trabalho |

---

## 1. No caminho vivo hoje

Tudo abaixo entra pelo mesmo ponto, `jev_post_process_tool_result`
(`crates/codegen/distill-shell/src/session/acp_session_impl/jev_tool_result.rs`),
que agora chama a pipeline única em
`crates/codegen/distill-shell/src/jev_lanes.rs::reduce_payload`. Uma chamada só,
então é ela que o teste dirige e é ela que a sessão roda.

| técnica | id no catálogo | status | observação |
| --- | --- | --- | --- |
| Reuso de leitura | `retrieve` (lado leitura) | `live` | só bytes idênticos, e nunca um arquivo que mudou |
| Strip de ANSI | `ansi_escape_strip` | `live` | primeiro passo fixo da cadeia |
| Colapso de frames de progresso | `progress_bar_crusher` | `live` | segundo passo fixo |
| Colapso de repetição (log/listagem/comando) | `log_crusher` | `live` | `reduce_redundancy`, preserva todo literal por construção |
| Crush de diff | `diff_crusher` | `live` | mantém cabeçalhos, hunks e linhas mudadas |
| Colapso de padding de colunas | `padded_table_compact` | `live` | **ligado neste trabalho**; é o ganho novo mais claro |
| Compressão de relatório de teste | `test_crusher` | `live·gated` | **ligado neste trabalho**; recusa quando a linha descartada carrega literal |
| Compressão de stack trace | `stack_crusher` | `live·gated` | **ligado neste trabalho**; recusa um dump com endereços, porque endereço é literal |
| Redução de lockfile | `lockfile_crusher` | `live·gated` | **ligado neste trabalho**; na prática recusa (versões e checksums são literais) e cai para o colapso de repetição |
| Crush de JSON | `json_crusher` | `live·gated` | **ligado neste trabalho**; o portão de documento barra, porque JSON é o próprio documento |
| Crush de HTML | `html_crusher` | `live·gated` | **ligado neste trabalho**; mesmo caso do JSON |
| Strip de output de notebook | `notebook_crusher` | `live·gated` | **ligado neste trabalho**; notebook é JSON, então cai no mesmo portão |
| Elisão por importância (cabeça, cauda, meio) | `stdout_budget_elide` | `live` | lossy: grava o original antes e nomeia o arquivo no corpo |
| Store byte-exato do original | `retrieve` | `live` | `jev_store::store_payload`, lido de volta byte a byte |
| Poda de família de ferramentas por turno | `p1_tool_family` | `live` | não é crusher: muda o que é anunciado, não o payload |
| Janela de leitura em vez de arquivo inteiro | `p2_read_shortlist` | `live` | idem |
| Recorte da compactação | `p3_compaction_recorte` | `live` | idem |
| Retenção de output grande | `d2_big_output_retention` | `live` | idem |
| Re-injeção pós-compactação | `d3_post_compaction` | `live` | idem |
| Roteamento hard/light/cheap | `b2_light_model`, `b2_local_model`, `b2_micro_effort` | `live` | economiza **dinheiro**, não tokens: muda quem responde |

## 2. Codificado e sem chamador

| técnica | id no catálogo | status | motivo |
| --- | --- | --- | --- |
| Esqueleto de código (assinaturas, sem corpos) | `source_skeleton` | `unwired` | lossy de verdade: precisa do original gravado antes; hoje não há estágio que grave para ele |
| Crush de SVG | `svg_crusher` | `unwired` | idem: o path-data completo fica atrás de um handle que ninguém lê |
| Aviso de asset gerado | `generated_asset_notice` | `unwired` | idem: troca o arquivo por um aviso, o que é perda sem store |
| Blobs embutidos (base64/data-URI/hexdump) | `embedded_blob_crusher` | `unwired` | idem, e o blob é literal para o portão |
| Vista mascarada de segredo | `secret_redact_view` | `unwired` | é transformação de segurança, não de economia; mascara valor por decisão, não por ganho |
| Codec de span compacto | `compact_span` | `unwired` | depende do dialeto de wire, que este harness não tem |
| Codec TOON | `toon_codec` | `unwired` | duplica `json_crusher` (`toon_encode` só delega), e o portão de documento barra JSON |
| Stats do store | `stats` | `unwired` | função pura sobre entradas em memória; não há índice de store a consultar |
| Busca no store | `search_store` | `unwired` | idem |
| Query determinística em handle | `handle_query_eval` | `unwired` | idem |
| Estimativa de tokens por provedor | `handle_token_estimate` | `unwired` | `estimate_tokens` existe; falta um portão que decida por custo |
| Mapa de repo por orçamento | `repo_map_budget` | `unwired` | precisa de índice de símbolos que o harness não constrói |
| Ack curto de escrita | `write_ack_verify` | `unwired` | risco de tirar do modelo o eco que ele confere; medir antes |
| Diff contra baseline de teste | `test_baseline_diff` | `unwired` | precisa de baseline persistido por sessão |
| Alias reversível de identificadores | `identifier_alias_codec` | `unwired` | precisa de tabela de expansão atrás de handle |
| Validação de schema em handle | `schema_validate_eval` | `unwired` | sem chamador que peça validação |
| Aviso de payload duplicado | `duplicate_notice` | `unwired` | o reuso de leitura cobre o mesmo caso para bytes idênticos |
| Detector de token volátil | `volatile_tokens` | `unwired` | é diagnóstico de cache do provedor, não muda bytes |
| Auto-citação de `file:line` | `error_site_autoquote` | `unwired` | `error_site_refs` existe e é puro; falta o passo que anexa as linhas |
| 91 das 93 tarefas registradas | família `e_cheap_task` | `unwired` | a registry em `jev/tasks.rs` tem 93 tarefas; o caminho vivo chamava só `test_verdict` e `distill_command_output`, e ambos atrás de lane desligada por default |

## 3. Portável do app macOS

Ordem sugerida: do mais barato para o mais caro.

| técnica do app | o que falta aqui | status |
| --- | --- | --- |
| Mapa `command + classe → tarefa` | escolher a tarefa de utilidade pelo comando e pela classe em vez de por id fixo | `deferred` (próximo lote) |
| Skip-set do que não comprime | memorizar payload provado não compressível e não reclassificar | `deferred` |
| Dedup de blocos no mesmo prompt | trocar bloco idêntico por ponteiro `⟪igual ao bloco #N⟫` | `deferred` |
| Ponteiros de dedup entre turnos | além do reuso byte-idêntico: teto por prompt, nunca o output mais recente | `deferred` |
| Compactador de payload persistido | valores grandes em eventos gravados viram cabeça + handle + cauda | `deferred` |
| Roteador lossy com handle obrigatório | o app só roteia stack/test/source/diff com handle durável; aqui falta o estágio que grava e então aplica | `deferred` |
| Gate de fidelidade | rejeitar compressão degenerada (`NONE`/`PASS`) além do gate de literais | `deferred` |
| Circuit breaker por taxa de retrieve | hoje `e_breaker` conta falha de lane, não taxa de retrieve nem custo | `deferred` |
| Dialeto de wire (`RCW1`) | reencodar o prompt só se ficar ≤45% dos tokens | `deferred` |

## 4. Fora de escopo

| grupo | entradas | motivo |
| --- | --- | --- |
| Host-bound | `graphify_query`, `path_wrappers`, `screenshot_ocr`, `doc_text_extract`, `stale_tool_result_evict`, `workspace_change_notice`, `audio_transcribe_local`, `video_keyframe_ocr`, `screenshot_diff_digest`, `unchanged_reread_stub`, `predicate_watch`, `lsp_query`, `ast_grep_query`, `batch_codemod_apply`, `stale_tool_args_evict`, `spreadsheet_extract` | dependem do app macOS como host: PATH wrappers, FSEvents, OCR, ASR, Vision, LSP, tree-sitter, scrollback, anexos |
| Precisa de modelo local | `encoder_rank`, `semantic_cache_lookup`, `workspace_semantic_search`, `session_transcript_search`, `repeated_failure_notice`, `history_recall_search`, `nli_gate`, `nli_claim`, `semantic_dedup`, `correction_exemplar_store`, `metric_delta` | dependem de embeddings ou NLI que este harness não carrega |
| Restrito ao 35B | `impacted_test_pick`, `cross_handle_contradiction_scan`, `plan_candidate`, `review_comment_draft`, `refactor_suggestion_list`, `test_plan_draft`, `root_cause_hypothesis_draft` | o catálogo restringe ao modelo grande; não é lane barata |
| UI apenas | `thread_title_generate`, `provider_tier_suggest` | rótulo de UI, nunca entra no contexto pago |
| Ledger de economia | `token_saver_savings_totals`, relatório, popover | não pedido neste trabalho; sem medição, nenhum número de economia pode ser alegado |
| Subcomando `distill compress` | wrapper `\| distill` do app | fora do escopo declarado; o harness é o próprio agente e comprime em processo |
| Hooks de provedor | `PostToolUse` com `updatedToolOutput` | fora do escopo declarado; rotas que já reescrevem por hook não são substituídas |

## 5. Regras duras

Estas são proibições do catálogo, aplicadas como regra e não como item de
trabalho:

| regra | onde está aplicada aqui |
| --- | --- |
| Nunca auto-comprimir família exact-output (`rg`, `grep`, `sed`, `awk`, `cut`, `tr`, `cat`, `head`, `tail`, `diff`, `jq`, `yq`, …) nem `git grep\|show\|cat-file\|blame` | `crushers::is_exact_output`, no portão de entrada da pipeline |
| Nunca comprimir a mensagem do usuário | a pipeline só recebe resultado de ferramenta; o único chamador é o pós-processamento de tool result |
| Nunca comprimir corpo de skill nem superfície de permissão | `is_exact_output` cobre caminho `/skills/` e `SKILL.md`; permissão não passa por este caminho |
| Nunca devolver valor de segredo | `secret_presence` / `pii_presence` marcam sem ecoar valor |
| Nunca contar estimativa como economia medida | nada aqui reporta economia; não existe ledger |
| Falha é passthrough, não perda | toda lane devolve os bytes de hoje quando não tem ganho |

## 6. Defaults

`JevFlags::harness_default()` em `crates/codegen/distill-workspace/src/jev/flags.rs`
é a lista autoritativa.

Ligadas: `e_crushers`, `e_importance`, `e_read_reuse`, `e_breaker`,
`d2_big_output_retention`, `d3_post_compaction`, `p1_tool_family`,
`p2_read_shortlist`, `p3_compaction_recorte`, `p6_skill_suggestion`, `b2_light_model`,
`b2_local_model`, `b2_micro_effort` e as demais `a*`, `b1`, `b3`, `b6`, `c1`–`c5`, `c7`.

Desligadas, com o motivo:

| chave | motivo para continuar desligada |
| --- | --- |
| `e_retention` | lane que pergunta chunk a chunk; sem medição de custo por chamada, o gasto pode passar o ganho |
| `e_cheap_compress` | idem: uma chamada paga por payload |
| `e_cheap_task` | é a lane das 93 tarefas; ligar exige o mapa comando+classe → tarefa (seção 3, primeiro item) |
| `e_cheap_agent` | a lane de subagente barato não está ligada; a decisão registra `defer` em vez de fingir |
| `e_lane_choice` | a bateria que decide main-vs-cheap por micro-ação ainda não tem portão de custo |
| `e_prompt_blocks` | recorte do prompt-base; precisa de whitelist de blocos obrigatórios |
| `b2_model_tier` | alavanca de dinheiro; espera o portão dela |
| `c6_injection_screen` | espera medição do próprio custo |
