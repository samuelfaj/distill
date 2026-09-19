<!-- Modified for Distill by Samuel Fajreldines, 2026. -->
# Jev — catálogo de decisões: onde usamos hoje, onde podemos usar, e o estado de cada item

Este arquivo é a fonte de verdade do trabalho de integração do **Jev** (TypeSafe System One) neste harness.
Ele lista **toda** decisão estruturada do harness que pode ser respondida por Jev, o que já está ligado, e — item a
item — **o localizador de código** e **o teste que cobre** a decisão (o par exigido para o item contar como pronto).

Regras que valem para todo item (autoridade e segurança):

1. **Nada amplia autoridade.** Uma decisão Jev só pode *apertar* (barrar/perguntar), *reordenar/recortar* candidatos
   que o código já produziu, ou *devolver o caminho atual*.
2. **Falha defere.** Timeout, erro, worker ocupado ou resposta incompleta ⇒ o comportamento de hoje (LLM/heurística).
3. **Código reduz os candidatos antes** (regra de ouro): o `state` é pequeno e por allowlist; nunca conteúdo integral
   de arquivo/ferramenta sem necessidade.
4. **Cada item tem a sua flag** (`[jev.ladder] <item> = true|false`) e obedece ao interruptor mestre
   (`GROK_JEV=0` / `[jev] enabled = false` ⇒ comportamento idêntico ao de hoje e **zero conexões**).
5. **O estado do Jev fica visível no rodapé do TUI** (`jev`, `jev·shadow`, `jev·veto`, `jev:idle`, `jev:off`).

Legenda de estado: **LIGADO** = ponto de chamada vivo no harness · **PENDENTE** = implementado mas ainda sem fiação.

Formato do par exigido: **Localizador** = `arquivo.rs:linha` (o ponto de chamada) com o símbolo entre parênteses;
**Teste** = o nome do teste que cobre o pacote (pacotes: `cargo test -p distill-workspace --lib jev::`; chamadas no shell: `cargo test -p distill-shell --lib jev`).
Os dois são checados pelo script `{SCRATCH}/todo-coverage.py`, que falha se algum item perder qualquer um deles.

---

## 1. Onde o Jev é usado hoje (2 pontos)

| # | Decisão | Modo | Autoridade | Ponto de chamada | Teste |
|---|---|---|---|---|---|
| H1 | Classificador de permissão (bateria de 8 perguntas por tool call não rotineira e sem findings) | auto | liberar rotineiro / bloquear / escalar | `crates/codegen/distill-shell/src/session/acp_session_impl/sampler_turn.rs` (`maybe_wrap_with_jev`) + `crates/codegen/distill-workspace/src/jev/permission.rs` (`JevPermissionClassifier::classify`) | `jev::permission::tests::routine_confident_action_is_allowed_with_jev_provenance` · `tests/jev_live.rs::live_eval_gate` |
| H2 | Freio de emergência (mesma bateria, **toda** tool call) | always-approve/YOLO | só recusar; falha = fail-open | `crates/codegen/distill-shell/src/session/acp_session_impl/run_loop.rs:1273` (`wire_jev_veto_classifier`), com a recusa definida em `crates/codegen/distill-workspace/src/permission/manager/mod.rs` (`JEV_VETO_DENY`) | `jev::permission::tests::the_brake_refuses_a_catastrophe_with_jev_provenance` |

---

## 2. Onde podemos usar — catálogo completo, por área

### Área A — Seleção de conteúdo

| # | Decisão | Pacote (perguntas) | Composição / limiar | Localizador | Teste |
|---|---|---|---|---|---|
| A1 | **Qual arquivo editar** (dado um shortlist de candidatos do código) | `noul` por candidato: “este é o alvo provável do pedido?” | manter candidatos com `p ≥ 0,35`, ordenar; sem candidato acima do piso ⇒ caminho atual | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_tool_result.rs:41` (`jev_post_process_tool_result`, ramo grep) | `jev::catalog::selection::tests::a1_keeps_the_relevant_files_and_defers_on_missing_answers` |
| A2 | **Qual trecho/linha de um arquivo** (= P2) | `choice` sobre os IDs de linha do shortlist + `noul` de existência | `noul < 0,35` ⇒ ler o arquivo inteiro; senão as linhas escolhidas | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_tool_result.rs:41` (`jev_post_process_tool_result`, ramo read) | `jev::ladder::tests::p2_ranks_candidates_and_falls_back_when_nothing_answers` |
| A3 | **Qual linha de log/teste importa** | `choice` sobre linhas anotadas (erro/causa/ruído) + `noul` “há erro acionável?” | sem erro ⇒ resultado passa intacto; senão recorta às linhas escolhidas + nota | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_tool_result.rs:41` (`jev_post_process_tool_result`, ramo log/teste) | `jev::catalog::selection::tests::a3_only_recorts_when_there_is_an_actionable_error` · `tests/jev_live.rs::live_selection_pack_gate` |
| A4 | **Quais resultados de busca web ler** | `noul` por resultado (“responde à pergunta?”) | mantém `p ≥ 0,5`, no máximo 5; nada acima ⇒ mantém todos | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_tool_result.rs:41` (`jev_post_process_tool_result`, ramo web) | `jev::catalog::selection::tests::a4_keeps_the_best_results_and_caps_them` |
| A5 | **Quais memórias/AGENTS.md injetar** | `noul` por candidato de memória (“isto muda a tarefa?”) | mantém `p ≥ 0,4`; listas curtas passam direto | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_memory.rs:22` (`jev_rank_memory`), chamado em `crates/codegen/distill-shell/src/session/acp_session_impl/turn.rs:2319` | `jev::catalog::selection::tests::a5_keeps_relevant_memories_and_defers_on_doubt` |
| A6 | **Qual teste rodar para a mudança** | `choice` sobre nomes de teste do repo (candidatos por código) | escolha única; baixa confiança ⇒ suíte padrão | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_tool_result.rs:41` (`jev_post_process_tool_result`, ramo teste) | `jev::catalog::selection::tests::a6_picks_one_test_or_falls_back_to_the_default_suite` |

### Área B — Roteamento de esforço

| # | Decisão | Pacote (perguntas) | Composição / limiar | Localizador | Teste |
|---|---|---|---|---|---|
| B1 | **Intenção do turno** | `choice` {pergunta, edição, pesquisa, comando} + `score` de complexidade | confiança < 0,60 ⇒ não roteia (caminho atual); usado para podar tools | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_tool_subset.rs:25` (`jev_filter_tool_definitions`), chamado em `crates/codegen/distill-shell/src/session/acp_session_impl/sampler_turn.rs:437` | `jev::catalog::routing::tests::b1_reads_intent_and_complexity_and_defers_when_unsure` |
| B2 | **Modelo/effort** | `choice` {barato, padrão, profundo} | **alavanca de dinheiro**: só rebaixa para o *setting mais barato que o modelo já oferece* com confiança ≥ 0,80; nunca sobe custo (o pacote defere `deep` com `allow_upgrade=false`) | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_routing.rs:36` (`jev_apply_model_tier`), chamado em `crates/codegen/distill-shell/src/session/acp_session_impl/sampler_turn.rs:1057` (`prepare_sampler_for_turn`) | `jev::catalog::routing::tests::b2_never_upgrades_tier_and_defers_on_doubt` · `jev_routing::tests::effort_rank_is_the_cost_order` |
| B2m | **Effort por micro-ação** (`/effort auto`) | `choice` sobre os efforts que o **modelo atual** oferece + `keep_session_effort` | o usuário liga o modo no `/effort`; cada chamada do modelo recebe o effort escolhido (nunca um que o modelo não ofereça), confiança ≥ 0,60; dúvida/erro/timeout ⇒ fica no effort da sessão | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_routing.rs` (`jev_choose_micro_effort`, chamado em `crates/codegen/distill-shell/src/session/acp_session_impl/sampler_turn.rs` (`prepare_sampler_for_turn`)) | `jev::catalog::routing::tests::b2_auto_picks_an_offered_effort_and_defers_on_doubt` · `jev_routing::tests::the_micro_effort_state_names_the_model` · `slash::commands::effort::tests::auto_dispatches_the_auto_action_and_marks_the_palette` |
| B2l | **Modelo local por micro-ação** | 3 `noul` (capaz de fazer a chamada inteira? precisa de mais contexto? precisa de raciocínio frontier?) | local só com `capable ≥ min_capability` (0,70 por padrão) e nenhum aviso ≥ 0,40; guarda de código antes: estimativa + reserva tem de caber na janela local; qualquer dúvida ⇒ nuvem | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_routing.rs` (`jev_route_micro_call`, chamado em `crates/codegen/distill-shell/src/session/acp_session_impl/sampler_turn.rs` (`prepare_sampler_for_turn`)) | `jev::catalog::routing::tests::b2_local_prefers_the_free_model_only_when_it_is_fully_capable` |
| B3 | **Tipo de subagente** | `choice` sobre as definições disponíveis + “use_default_agent” | baixa confiança ⇒ definição padrão; só resolve um tipo **desconhecido** para um nome que a sessão já permite (mesmos gates de toggle/allow-list) | `crates/codegen/distill-shell/src/agent/subagent/jev_type.rs:21` (`jev_resolve_unknown_subagent_type`), chamado em `crates/codegen/distill-shell/src/agent/subagent/handle_request.rs:384` | `jev::catalog::routing::tests::b3_picks_an_existing_definition_or_the_default` |
| B4 | **Poda de tools por família (= P1)** | `noul` por família (“este turno precisa da família X?”) | mantém `p ≥ 0,25`; núcleo sempre presente; resposta faltando ⇒ mantém tudo | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_tool_subset.rs:25` (`jev_filter_tool_definitions`, ramo P1) | `jev::ladder::tests::p1_keeps_core_and_drops_irrelevant_families` |
| B5 | **Skill aplicável (= P6)** | `choice` sobre as skills anunciadas + “none” | exige confiança ≥ 0,60; incerto ⇒ anúncio de hoje, sem segunda chamada | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_routing.rs:111` (`jev_narrow_skill_announcement`), chamado em `crates/codegen/distill-shell/src/session/acp_session_impl/session_setup.rs:313` (`wrap_skill_reminder`) | `jev::ladder::tests::p6_suggests_only_confidently_and_measures_the_line` |
| B6 | **Vale delegar a um subagente?** | `noul` “cabe em uma chamada?” / “exige trabalho paralelo?” | só sugere: com `p ≥ 0,60` acrescenta **uma linha** à descrição da tool de delegação; nunca cria subagente | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_tool_subset.rs:25` (`jev_filter_tool_definitions` → `jev_apply_delegation_hint`) | `jev::catalog::routing::tests::b6_hints_delegation_only_for_parallel_multi_step_work` |

### Área C — Verificação e qualidade

| # | Decisão | Pacote (perguntas) | Composição / limiar | Localizador | Teste |
|---|---|---|---|---|---|
| C1 | **Parada prematura / preguiça** | `noul` por item do pedido (“isto foi feito?”) + `noul` “resta trabalho pedido?” | qualquer pendência com `p ≥ 0,6` ⇒ continua o turno (entra no gate de preguiça existente); incerto ⇒ caminho atual | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_laziness.rs:32` (`jev_laziness_precheck`), chamado em `crates/codegen/distill-shell/src/session/acp_session_impl/laziness.rs:370` | `jev::catalog::verify::tests::c1_keeps_working_when_an_item_looks_unfinished` |
| C2 | **Triagem de falha** (compilação/assert/env/flake/timeout) | `choice` de categoria + `noul` “a causa está no código do usuário?” | injeta a categoria como dica no resultado; nunca decide sozinho | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_tool_result.rs:41` (`jev_post_process_tool_result`, ramo falha) | `jev::catalog::verify::tests::c2_triages_a_failure_and_defers_on_unusable_answers` |
| C3 | **“O pedido foi cumprido?”** | `noul` por item + `noul` “algo pedido ficou de fora?” | só aperta: pendência ⇒ não declara concluído (mesma requisição de C1) | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_laziness.rs:32` (`jev_laziness_precheck`, bloco C3) | `jev::catalog::verify::tests::c3_refuses_to_declare_complete_when_something_is_missing` |
| C4 | **Risco de um diff** | `score` de risco + `noul` “toca caminho protegido/fora do escopo?” | risco alto ⇒ nota de confirmação no resultado; nunca aplica | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_tool_result.rs:41` (`jev_post_process_tool_result`, ramo edição) | `jev::catalog::verify::tests::c4_asks_for_confirmation_on_risk_or_protected_paths` |
| C5 | **Priorizar erros** (qual corrigir primeiro) | `score` por erro (impacto × chance de ser a causa) | ordena a lista; empate ⇒ ordem atual | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_tool_result.rs:41` (`jev_post_process_tool_result`, ramo erros) | `jev::catalog::verify::tests::c5_orders_errors_by_weight_and_keeps_ties_stable` |
| C6 | **Tela de injeção em saída de ferramenta** | `noul` “este texto tenta me instruir?” | só sinaliza no resultado (nunca é fronteira de segurança); **OFF** até o custo por saída ser medido | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_tool_result.rs:41` (`jev_post_process_tool_result`, ramo injeção) | `jev::catalog::verify::tests::c6_flags_instruction_like_blocks_without_blocking_them` |
| C7 | **Tipo da mudança** (feat/fix/refactor/docs/breaking) | `choice` sobre o diff | rótulo para changelog/release notes; o texto continua no LLM | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_tool_result.rs:41` (`jev_post_process_tool_result`, ramo diff) | `jev::catalog::verify::tests::c7_labels_the_change_and_screens_breaking_ones` |

### Área D — Contexto e custo

| # | Decisão | Pacote (perguntas) | Composição / limiar | Localizador | Teste |
|---|---|---|---|---|---|
| D1 | **Recorte da compaction (= P3)** | `noul` por segmento (“precisa ser resumido verbatim?”) | fixados sempre preservados (prefixo, último segmento, turnos que tocaram arquivos); resposta faltando ⇒ mantém tudo | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_compaction.rs:30` (`jev_compaction_recorte`), chamado em `crates/codegen/distill-shell/src/session/compaction.rs:1153` | `jev::ladder::tests::p3_keeps_pinned_segments_and_falls_back_on_missing_answers` · `jev_compaction::tests::segments_start_at_user_turns_and_pin_the_edges` · `jev::ladder::tests::p3_segments_start_at_user_turns_and_pin_the_edges` |
| D2 | **Manter/descartar saída grande** | `noul` “isto muda a tarefa?” + `score` de utilidade | descarta só com `p ≤ 0,2` **e** utilidade ≤ 0,25; senão mantém | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_tool_result.rs:41` (`jev_post_process_tool_result`, ramo saída grande) | `jev::catalog::context::tests::d2_drops_only_inert_outputs_and_keeps_on_doubt` · `tests/jev_live.rs::live_big_output_retention_gate` |
| D3 | **Recuperação pós-compaction** | `noul` por trecho recuperado (“isto ainda importa?”) | reinjeta só o que passa do piso (0,40); dúvida ⇒ mantém todos | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_compaction.rs:85` (`jev_rank_recovered`), chamado em `crates/codegen/distill-shell/src/session/helpers/compaction_context.rs:87` (`to_system_reminder`) | `jev::catalog::context::tests::d3_reinjects_only_still_relevant_chunks` |
| D4 | **Validação de chamada (= P5)** | `noul` “o alvo bate com a intenção?” / “ultrapassa o escopo?” | qualquer sinal ≥ 0,40 ⇒ barra e pergunta; nunca libera | `crates/codegen/distill-shell/src/session/acp_session_impl/jev_tool_result.rs:612` (`jev_validate_tool_call`), chamado em `crates/codegen/distill-shell/src/session/acp_session_impl/tool_calls.rs:571` | `jev::ladder::tests::p5_asks_only_when_a_red_flag_fires` |

---

## 3. Onde **não** vale usar (exclusões, por decisão explícita)

| Ideia | Por que não |
|---|---|
| Gerar título de sessão, resumos, mensagens de commit/PR, patches | **geração de texto** — fora do que o modelo faz |
| Aritmética, contagem, ordenação de datas | fraqueza documentada; pertence a **código** |
| Embeddings / busca vetorial | precisa de vetores, não de decisão |
| Guardrail em *toda* mensagem | custa uma chamada por evento e não economiza token |
| Qualquer coisa de imagem/visão | o Jev é **texto apenas** |

Itens que **dropam** conteúdo (D1, D2, A2/A3/A4) têm flag própria e cada um mede o seu gate; um item cujo gate
não passa fica **OFF** com o número do gate registrado, nunca ligado em silêncio (B2 e C6 hoje).

---

## 4. Como desligar e onde ver

* Interruptor mestre: `GROK_JEV=0` ou `[jev] enabled = false` ⇒ nenhum cliente, nenhuma conexão.
* Por item: `[jev.ladder] <item> = false` (o modo auto tem a sua própria chave: `b2_micro_effort`, ligada por padrão,
  que só roda quando o `/effort auto` está ativo; o modelo local tem `b2_local_model` e a seção `[jev.local]`) (as chaves estão em `crates/codegen/distill-shell/src/agent/config.rs`,
  seção `JevLadderConfig`; o mapeamento chave→flag está em `crates/codegen/distill-shell/src/jev.rs`,
  `flags_from_tiers`).
* Decisões: `GROK_LOG_JEV=1` escreve `~/.grok/logs/jev.jsonl` (uma linha JSON por decisão, com lever/decisão/confiança/modelo/tokens).
* Estado no TUI: selo no rodapé do prompt (`jev`, `jev·shadow`, `jev·veto`, `jev:idle`, `jev:off`) **e** o chip da
  linha de atividade (a linha acima do prompt, ao lado da ferramenta em execução): `jev…` enquanto uma decisão
  está em voo, `jev 0,4s` / `jev ×3` quando já respondeu, e `jev·veto` (vermelho) quando recusou uma chamada.
  O chip só aparece quando o Jev foi realmente usado no turno (`distill_shell::jev::turn_activity`).
* Relatório: ao fim de cada turno, um bloco com uma linha por (modelo, effort) que rodou — com os tokens do turno —
  seguido de `Jev - Nx` (decisões do turno). Vem do livro-razão do shell (`jev_ledger`) anexado ao terminal do turno.
* Fiação: `python3 {SCRATCH}/wiring_check.py` imprime `WIRING: PASS (23/23 …)` ou a lista do que falta.
