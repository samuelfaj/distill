
# TODO — entrega completa do Distill

## 0. Objetivo

Implementar todos os requisitos deste documento, revisar o resultado e
comprovar o funcionamento. Cada checkpoint aprovado pelo controlador deve ser
commitado e enviado ao remoto durante a execução. Somente após a verificação
funcional final, a revisão independente e um gate explícito de publicação,
executar as ações irreversíveis de distribuição:

1. Preparar e confirmar o conjunto final revisado.
2. Criar a tag/versão e a GitHub Release com `gh release`.
3. Instalar/verificar os artefatos publicados e confirmar a publicação.

Esta fase usa o pipeline atual de `/sam-orchestrate`; referências históricas a
`/sam-goal` não substituem esse pipeline.

### Estado conhecido

O checkpoint de depuração/teste anterior foi o commit
`c3d02c1646eaacbb71c5660041bba58583e0c15e`, com o binário local
`distill 2.0.0 (c3d02c1646ea) [alpha]` e SHA-256
`f73c33e30533f3202d1eecc52da7c6d2e88a61f218da3b63e820b8b49a9ad94b`.
A versão publicada separadamente é a tag `v2.0.0`, source SHA
`b9d8f23ea845ef7fadc5f22a089e61f23d0a9175`, em
`https://github.com/samfaj/distill/releases/tag/v2.0.0`. Estes documentos são
o checkpoint pós-publicação ainda não commitado/pushado; não atribuir a eles
um SHA remoto até o controlador concluir esse checkpoint.

`test.txt` continua desconhecido, preservado e excluído. O catálogo Jev antigo
continua arquivado apenas como histórico; as decisões H1/H2/P5 e os badges
`jev·veto` estão aposentados e não podem ser reativados ou contados como prova.

### Evidência final disponível

- `controller-shell-final-isolated-tests.log`: 405 PASS, 0 FAIL, 23.12 s,
  com `GROK_HOME` isolado, stack de 16 MiB e uma thread.
- `controller-resolution-final.log`: 94 PASS, incluindo precedência,
  criação/fork/retomada e definições/configuração de resolução. O ciclo de
  vida, wake, deduplicação, compaction, modelos, esforços e integração dos
  subagents está no `controller-shell-final-isolated-tests.log` (405 PASS).
- `controller-footer-pager-tests.log`: 431 PASS, 0 FAIL, 0.79 s;
  `workspace-final-tests.log`: 139 PASS; `tools-tests.log`: 9 PASS;
  `controller-installer-final.log`: 1 PASS.
- A publicação `v2.0.0` foi verificada no workflow `35492066534`: os quatro
  builds nativos e o job `publish` passaram (5/5 jobs). A verificação da
  release confirmou 9/9 assets, quatro binários com arquitetura/SHA-256 e os
  metadados `install.sh`, `LICENSE`, `NOTICE`, `SHA256SUMS` e
  `THIRD-PARTY-NOTICES`.
- O instalador oficial terminou com exit 0. O binário macOS ARM instalado
  corresponde a `distill-macos-aarch64`, SHA-256
  `eeb0950a07df737c36fcdac5cdc9183012c7adde3cb11df08921a2d6024d13fc`.
  Os testes pós-instalação passaram: onboarding 2/0 em 35.95 s, status 2/0
  em 11.04 s, modelo 1/0 em 5.44 s e tools/yolo 2/0 em 10.34 s (7 casos
  únicos).
- O smoke live de ferramenta no artefato instalado passou em 17.67 s, exit 0,
  com o marcador `DISTILL_YOLO_PUBLISHED_OK` e `run_terminal_command` real.
  Isso não prova login OAuth dos três provedores. As tentativas anteriores
  com HTTP 429 permanecem como histórico; não são o resultado atual do smoke
  instalado.
- PTY no binário atual: onboarding 2 PASS/0 FAIL em 36.04 s, status 2/0 em
  11.56 s, troca de modelo 1/0 em 5.54 s e child navigation 1/0 em 5.35 s.
  São seis casos reais do app/harness com endpoints mock isolados, não OAuth
  real. Os frames registram normal/narrow/short 8x80, Step 3 com `Esc`,
  conclusão persistida (`onboarding_completed=true`, `test-model`),
  restart/reopen/back e URL/falha do navegador com `Finish`.
- O complemento positivo do worker no binário atual também passou: compilação
  do fixture em 1.13 s e `controller-positive-worker-pty.log` com 2 PASS/0
  FAIL em 36.19 s. Os artefatos `normal-step2-model-selected.screen.txt`,
  `normal-step3.screen.txt`, `normal-complete.screen.txt` e
  `restart-completed.screen.txt` mostram `default-model` escolhido como
  worker, `Worker model saved`, `Worker model: default-model` e a
  persistência após conclusão/restart.
- `controller-worker-command-tests.log` passou 19/19 testes em 0.10 s,
  incluindo parsing/ação real de `WorkerModelCommand`, aliases, esforços e
  `clear`; isso é prova de regressão do comando, não OAuth live.
- Os testes de child capturam requests reais do sampler e criação/persistência/
  wake real; os testes de unidade não são reduzidos a valores de helper.
- O navegador bloqueou a inspeção de HTML local pela política de URL. Não houve
  workaround, follow externo, alteração de credenciais ou reivindicação de
  screenshot colorido. Login OAuth real dos três provedores não foi verificado;
  o smoke live instalado é uma prova separada de ferramenta/conta já
  conectada.

### Histórico resolvido, não pendente

- As duas asserções contraditórias de autenticação foram corrigidas e o teste
  isolado de shell agora passa; a chave armazenada no ambiente do host não é
  mais tratada como prova.
- O grupo pacer falhou apenas no modo não isolado/agregado e agora passa no
  grupo isolado; nenhum limite de produção foi alterado.
- O stack de 16 MiB é uma configuração do harness do teste isolado, não uma
  exigência de produção. O binário antigo e os cinco achados anteriores foram
  substituídos pela clearance final e pelos seis PTYs atuais.

### Inventário de mecanismos existentes

Jev permanece uma camada de otimização, não uma autoridade de permissão. O
comportamento usa mecanismos existentes e separados: seleção/subconjunto de
ferramentas, crushers, retenção, deduplicação, read reuse, compressão,
compaction e reservas de contexto. A paridade principal/child é coberta para
criação normal, tipos especializados, fork, resume, workflow e filhos
aninhados, mantendo invariantes de permissão; não foi inventada uma flag
genérica de “token saver”.

### Limitações de publicação

O intento de provider live anterior registrado em `live-yolo-path.txt` e
`live-yolo-qwen-path.txt` terminou em HTTP 429 antes das chamadas de ferramenta;
isso é histórico e uma limitação externa separada, não uma recusa nativa de
permissão. O smoke live atual do artefato instalado passou, mas não verifica
login OAuth dos três provedores. Tag v2, GitHub Release, assets remotos,
instalação e smoke pós-instalação já foram verificados; permanece pendente o
checkpoint final de documentação (commit/push/revisão do controlador).

### Regras de execução

- [x] Ler as instruções atuais do repositório e verificar os arquivos sob escopo
  antes de editar; esta atualização permaneceu limitada aos documentos
  autorizados e ao relatório temporário.
- [x] Seguir o registro de `/sam-orchestrate`: `orchestration.json` contém o
  DAG, ownership, writable paths, gates e evidências V-E1/V-E2/V-E3/V-R1.
- [x] Contar unidades independentes e aplicar a regra de delegação: o DAG
  registra exatamente os três execution owners E1/E2/E3, além do controller e
  do reviewer; não foi criado um quarto executor.
- [x] Quando houve delegação, os owners, dependências, escopos e no-go paths
  foram registrados sem escritores concorrentes; os três execution proofs e a
  revisão independente têm evidência PASS nos relatórios controller.
- [x] O coordenador reexecutou a prova do worker antes de aceitar o resultado:
  fixture compile 1.13 s e positive-worker PTY 2/0 em 36.19 s.
- [x] Reutilizar mecanismos existentes, fazer alterações cirúrgicas e não
  adicionar dependências; a prova inclui os testes e harnesses já existentes.
- [x] Preservar alterações não relacionadas; nenhum arquivo de source/teste ou
  trabalho de outro worker foi editado nesta atualização.
- [x] Não declarar sucesso sem evidência nem reduzir silenciosamente o escopo;
  o relatório separa mock/OAuth, gate funcional e publicação.
- [x] Registrar bloqueios, testes não executados e limitações explicitamente;
  ver `worker-3-postrelease-report.md` e o checkpoint documental ainda aberto.

---

## 1. Onboarding do Distill

### Objetivo

Criar um onboarding simples, dinâmico e visualmente bem resolvido para a interface de terminal do Distill.

Requisitos obrigatórios:

- Todo o conteúdo da interface deve estar em inglês.
- Deve aparecer no primeiro uso interativo.
- Deve poder ser aberto novamente por `/onboarding`.
- Deve conter exatamente as quatro etapas descritas abaixo.
- Aplicar os princípios de UI/UX solicitados com `/ui-ux-pro-max`, adaptados ao terminal Rust/Ratatui e às convenções existentes.

### 1.1. Entrada, navegação e persistência

- [x] Criar um fluxo próprio de onboarding. Evidência final: `controller-onboarding-current-pty.log` percorreu as quatro etapas no app real; `controller-pager-focused-retry.log` separa `/onboarding` de `/tutorial`.
- [x] Separar `/onboarding` do tutorial atual, pois hoje ele é um alias de `/tutorial`. Evidência direta: `controller-pager-focused-retry.log`, `dispatches_open_onboarding_without_aliasing_tutorial` passou.
- [x] Preservar `/tutorial` e `/tour`. Evidência: `controller-pager-focused-retry.log` e `controller-footer-pager-tests.log` passaram os testes de dispatch, documentação, navegação e fechamento do tutorial.
- [x] Persistir a conclusão para evitar repetição automática após finalizar. Evidência: `onboarding_four_steps_persist_and_restart_normal_and_narrow` e os testes de persistência/status do mesmo log.
- [x] Permitir reabrir o fluxo por `/onboarding` mesmo depois de concluído. Evidência: frame `restart-completed.screen.txt` e `reopened_completed_onboarding_closes_at_app_dispatch_boundary`.
- [x] Mostrar progresso, por exemplo: `Step 1 of 4`. Evidência direta: `controller-pager-focused-retry.log`, `four_steps_have_stable_titles_and_progress` passou.
- [x] Oferecer navegação por teclado, retorno e saída claros. Evidência: frames atuais mostram `Enter`, `← back`, `Esc` e `s skip`; o PTY exerceu back/close/reopen.
- [x] Preservar as opções de mouse já suportadas pelos componentes existentes. Evidência direta: `controller-pager-focused-retry.log`, `large_catalog_scrolls_selected_row_and_derives_wrapped_mouse_hitbox` passou.
- [x] Adaptar texto e controles a terminais estreitos e baixos. Evidência direta:
  `controller-pager-focused-retry.log`, `short_terminal_keeps_resize_instruction_visible`,
  teste de hitbox quebrada e os artefatos current-binary normal/narrow/8x80
  (`short-resize-guard.screen.txt`, `short-resized-step1.screen.txt` e
  `short-resized-step2.screen.txt`).
- [x] Definir e testar o comportamento de fechar, reabrir e retomar. Evidência: artefatos atuais de back, `restart-completed`, `reopen` e o teste normal/narrow de persistência/restart.
- [x] Não marcar conclusão quando a gravação do estado falhar. Evidência direta: `controller-pager-focused-retry.log`, `completion_error_does_not_mark_the_flow_done` e `onboarding_completion_waits_for_persistence_and_keeps_failed_flow_open` passaram.
- [x] Preservar fluxos de prompt inicial, retomada/fork, login obrigatório, agentes externos e execução não interativa. Evidência: `controller-shell-final-isolated-tests.log` e `controller-resolution-final.log` cobrem resume/fork/child; o pager final cobre auth/tutorial e prompt status.
- [x] Documentar quando a abertura automática precisa ser adiada, sem marcar o onboarding como concluído. Evidência: `auto_onboarding_waits_for_provider_catalog_and_excludes_external_startup`.
- [x] Não abrir um modal invisível em modo mínimo: inspeção direta de
  `crates/codegen/distill-pager/src/app/dispatch/status.rs:719-723` confirma
  toast com `/fullscreen` e `/onboarding`, retornando antes de abrir o modal.
  Não houve PTY específico de modo mínimo.

### 1.2. Etapa 1 — OpenRouter e economia

Apresentar o benefício de maneira animada, curta e compreensível.

- [x] Explicar como e por que OpenRouter pode ajudar a economizar no Distill. Evidência: frame `short-resized-step1.screen.txt`.
- [x] Explicar a escolha de modelos e o roteamento de tarefas adequadas para modelos mais econômicos, conforme a implementação real. Evidência: frame `short-resized-step1.screen.txt`.
- [x] Distinguir:
  - Redução de custo.
  - Redução de tokens totais.
  - Redução de tokens processados pelo modelo mais caro.
- [x] Não prometer percentuais não comprovados. Evidência: o texto atual não contém percentual e separa custo/tokens.
- [x] Não afirmar que OpenRouter, sozinho, reduz a quantidade de tokens. Evidência: `short-resized-step1.screen.txt` declara explicitamente essa limitação.
- [x] Oferecer uma ação clara para continuar. Evidência: o frame mostra `Continue`.

Sugestão de título: `Make your AI budget go further`.

### 1.3. Etapa 2 — Login e seleção de `/model`

- [x] Oferecer as opções: `short-resized-step2.screen.txt` mostra Grok, Codex (ChatGPT) e OpenRouter.
  - Grok.
  - Codex (ChatGPT).
  - OpenRouter.
- [x] Reutilizar autenticação existente e contas já conectadas. Evidência direta: `controller-pager-focused-retry.log`, `onboarding_grok_login_uses_provider_connection_state` e os testes de cancelamento/retorno de autenticação passaram; OAuth real permanece não verificado.
- [x] Permitir selecionar o modelo principal pela funcionalidade existente de `/model`. Evidência direta: `controller-pager-focused-retry.log`, os testes `onboarding_model_dispatch_*`, o PTY de modelo atual (1/0) e o artefato `normal-step2-model-selected.screen.txt`.
- [x] Manter a seleção dentro da experiência guiada, sem exigir que o usuário descubra os comandos sozinho. Evidência: Step 2 atual inclui login, catálogo/modelo e Continue.
- [x] Mostrar carregamento, sucesso, erro e retry. Evidência: testes de auth/status do `controller-pager-focused-retry.log` e `controller-footer-pager-tests.log`.
- [x] Voltar à etapa correta após cancelar login ou picker. Evidência direta: `controller-pager-focused-retry.log`, `grok_onboarding_auth_returns_to_connect_after_success_error_or_cancel` passou.
- [x] Confirmar que o catálogo fica atualizado após autenticar. Evidência: `onboarding_selection_status_tracks_persisted_results` e `onboarding_chat_session_reports_runtime_model_without_waiting_for_save`.
- [x] Confirmar que a seleção foi efetivamente persistida. Evidência direta:
  `onboarding_selection_status_tracks_persisted_results` e os artefatos
  current-binary `normal-complete.screen.txt`/`restart-completed.screen.txt`,
  que preservam a configuração após conclusão e restart.
- [x] Não armazenar credenciais no estado do onboarding. Evidência: clearance de implementação e testes de auth isolados; nenhum credential real foi usado no PTY.
- [x] Não desconectar outros provedores. Evidência: `provider_login_starts_once_and_leaves_grok_auth_unchanged` e os testes de cancelamento/relogin.

Sugestão de título: `Connect your AI`.

### 1.4. Etapa 3 — Seleção opcional de `/worker-model`

- [x] Explicar brevemente o papel do worker. Evidência: `normal-step3.screen.txt`.
- [x] Permitir selecionar um worker pela funcionalidade existente. Evidência:
  `controller-positive-worker-pty.log` (2 PASS/0 FAIL, 36.19 s) e os artefatos
  `normal-step3.screen.txt`, `normal-complete.screen.txt` e
  `restart-completed.screen.txt` mostram seleção compatível de
  `default-model`, `Worker model saved` e persistência após restart. O caso
  separado de catálogo sem outro worker continua coberto pelo frame antigo;
  não são resultados contraditórios.
- [x] Oferecer `Skip` claramente. Evidência: `normal-step3.screen.txt` mostra `Skip and keep the current worker`.
- [x] Pular deve preservar um worker previamente configurado. Evidência: texto do frame e clearance de implementação; sem worker compatível no catálogo desta execução.
- [x] Respeitar compatibilidade de conexão, backend e credenciais. Evidência: texto do Step 3 e invariantes do clearance.
- [x] Preservar guardas de contexto e reserva de resposta. Evidência: routing/resolution logs e clearance de implementação.
- [x] Explicar o modo de esforço necessário ao roteamento automático. Evidência: `normal-step3.screen.txt` declara que effort deve ser `auto`.
- [x] Não deixar um worker aparentemente ativo que nunca possa ser selecionado sem informar o motivo. Evidência: o frame informa explicitamente ausência de worker compatível e oferece Skip.

Sugestão de título: `Choose a worker model`.

### 1.5. Etapa 4 — Convite para seguir no X

- [x] Convidar o usuário, em inglês, para seguir `@samfajreldines`. Evidência: `normal-step4.screen.txt`.
- [x] Oferecer ação para abrir exatamente:
  `https://x.com/samfajreldines/`
- [x] Abrir no navegador do usuário pelo mecanismo existente. Evidência: o PTY exerceu o opener hook e registrou a URL; a inspeção HTML local foi bloqueada pela política, sem workaround.
- [x] Oferecer `Skip`, concluindo sem abrir o navegador. Evidência: `normal-step4.screen.txt`.
- [x] Tratar falha de abertura com URL acessível e opção de finalizar. Evidência: caso `onboarding_browser_write_failure_keeps_finish_available` e frame de browser failure.
- [x] Não seguir automaticamente. Evidência: texto atual do Step 4.
- [x] Não afirmar que o follow foi verificado. Evidência: texto atual declara que a tela não comprova follow.

Sugestão de título: `Stay in the loop`.

### Aceite do onboarding

- [x] Percorrer as quatro etapas no app real. Evidência: `onboarding_four_steps_persist_and_restart_normal_and_narrow`.
- [x] Testar skips, retorno, fechamento, erros e cancelamentos. Evidência: frames atuais, browser failure, back/reopen e auth cancellation tests.
- [x] Reiniciar e confirmar a persistência da conclusão e das configurações. Evidência: frame normal concluído com `onboarding_completed=true`/`test-model` e restart PTY.
- [x] Reabrir usando `/onboarding`. Evidência: frame/reopen artifact e dispatch test.
- [x] Confirmar ausência de regressões no tutorial, login e seleção de modelos. Evidência: 431 pager tests, 405 shell tests e o current model PTY.

---

## 2. Roteamento reasoning/worker

### Objetivo

Garantir que o Distill selecione corretamente entre reasoning e worker em cada chamada.

“Alternar corretamente” significa escolher conforme a tarefa e as políticas, não fazer round-robin obrigatório.

### Implementação e verificação

- [x] Rastrear o caminho: decisão → configuração final → requisição enviada. Evidência: `controlled_routes_are_captured_on_the_wire`, `retry_sends_updated_final_model_and_effort` e source clearance.
- [x] Provar uma sequência reasoning → worker → reasoning usando decisões
  controladas. Evidência: `controlled_routes_are_captured_on_the_wire` em
  `crates/codegen/distill-shell/src/session/acp_session_tests/turn/rate_limit_backoff_tests.rs:338-464`,
  que captura reasoning/high → worker/low → reasoning/high no wire e passou
  no shell final 405/0.
- [x] Confirmar o modelo enviado de verdade, não apenas o modelo configurado. Evidência: wire-capture tests no routing log e 405-test shell log.
- [x] Preservar comportamento seguro quando:
  - Não há worker.
  - O worker é incompatível.
  - A conversa mais a reserva de resposta não cabe.
  - A decisão é incerta ou incompleta.
  - Há timeout ou falha da camada de decisão.
  - O roteamento está desabilitado.
- [x] Testar esforço automático e manual. Evidência direta: `controller-routing-idle-tests.log` cobre effort explícito e as guardas de auto-routing.
- [x] Testar modelos com nenhuma ou apenas uma opção de esforço. Evidência direta: `controller-routing-idle-tests.log`, `zero_or_single_effort_menus_*` passou.
- [x] Corrigir bloqueios indevidos à seleção do worker sem ignorar escolhas explícitas do usuário. Evidência: source clearance e `explicit_child_model_with_auto_effort_stays_pinned`.
- [x] Preservar as verificações de endpoint, backend e credenciais. Evidência: routing/resolution tests e clearance.
- [x] Preservar a conversa e as restrições do modelo selecionado. Evidência: source clearance e testes de contexto/fallback.
- [x] Verificar interação com modelos locais/utility existentes. Evidência: `rejected_local_route_resubmits_base_model_with_fresh_attribution` e `main_session_429_is_owned_by_the_sampler_never_the_pacer`.
- [x] Verificar retries e caminhos de recuperação sem persistir indevidamente a seleção anterior. Evidência final: `controller-routing-idle-tests.log` (13/0), `retry_sends_updated_final_model_and_effort`, `rejected_local_route_resubmits_base_model_with_fresh_attribution` e clearance C1–C5.

### Pontos anteriormente observados — resolvidos no snapshot final

Arquivo:
`crates/codegen/distill-shell/src/session/acp_session_impl/jev_routing.rs`

- A escolha de modelo/esforço, os menus zero/único e as guardas de compatibilidade
  foram revalidados no routing log (13/0) e na clearance C1–C5.
- As guardas de compatibilidade e contexto permanecem intactas; nenhuma
  alternância artificial foi introduzida.

### Aceite

- [x] Testes capturam os modelos realmente enviados em cada chamada. Evidência final: `controlled_routes_are_captured_on_the_wire` e `retry_sends_updated_final_model_and_effort`, mais os 405 testes isolados.
- [x] Casos elegíveis usam worker conforme a decisão. Evidência: chooser/explicit-child wire tests e clearance.
- [x] Casos que exigem reasoning usam reasoning. Evidência: route attribution/effort tests e source clearance.
- [x] Casos inválidos/incertos mantêm o comportamento seguro. Evidência: zero/single menus, rejected-local retry, budget exhaustion e fallback tests.
- [x] Não há troca de modelo apenas para produzir uma aparência de alternância. Evidência: explicit pin/auto tests e wire capture.

---

## 3. Mostrar o modelo realmente usado no status

### Objetivo

A linha de atividade deve identificar o modelo efetivo da chamada.

Problema mostrado pelo usuário:

`Writing file… 5.5s · jev ×64 · medium`

O esforço aparece, mas o modelo não.

### Requisitos

- [x] Mostrar identidade explícita tanto do reasoning quanto do worker. Evidência: `controller-status-current-pty.log` (2/0) e `model_switch_label_tests`.
- [x] Mostrar o esforço efetivamente aplicado quando existir. Evidência: routing ledger/effort tests e status source clearance.
- [x] Usar a configuração final enviada ao sampler como fonte de verdade. Evidência: final sampler/source clearance e wire tests.
- [x] Registrar depois de todos os ajustes de modelo/esforço, incluindo tier, effort floor e overrides. Evidência: `the_effort_floor_keeps_the_highest_and_resets_with_the_turn`, explicit child override tests.
- [x] Não usar apenas: o status final é correlacionado com a configuração enviada, não somente com default salvo, decisão provisória, chamada anterior ou chamada auxiliar. Evidência: source clearance, ledger/model-switch tests and current status PTY.
  - Modelo padrão salvo.
  - Decisão provisória.
  - Modelo da chamada anterior.
  - Modelo de uma chamada auxiliar.
- [x] Durante execução de ferramentas, mostrar o modelo que originou aquela ação. Evidência: status source clearance e current status PTY.
- [x] Não afirmar que uma inferência está ativa enquanto o modelo estiver ocioso. Evidência: `waiting_for_model_label_shows_before_first_token` e current status PTY.
- [x] Atualizar corretamente em retry, fallback, cancelamento, próxima chamada e próximo turno. Evidência: model-switch/ledger and retry tests in the 405/13 logs.
- [x] Não deixar chamadas de classificação/utility sobrescreverem o indicador da chamada principal. Evidência: status source clearance and auxiliary attribution tests.
- [x] Identificar corretamente outro modelo quando ele realmente executar a chamada. Evidência: wire capture and child status tests.
- [x] Preservar atividade, tempo, contador Jev e controles. Evidência: current status PTY and footer pager tests.
- [x] Manter legibilidade em terminais estreitos. Evidência: `controller-status-current-pty.log` and onboarding 8x80 artifacts.
- [x] Conferir coerência entre indicador, requisição enviada e relatório de uso por modelo/esforço. Evidência: ledger tests, wire capture and current status PTY.

### Lacunas anteriormente observadas — resolvidas no snapshot final

As divergências de modelo/esforço, atribuição após tier/effort floor e
isolamento entre sessões foram cobertas pela clearance de implementação, pelos
405 testes isolados de shell, pelos 13 testes wire/routing e pelos dois PTYs de
status atuais. Não há correção de fonte pendente nesta seção.

### Aceite

- [x] Testes cobrem o caminho integrado da decisão até o indicador. Evidência: 405 shell tests, 13 routing tests, 2 current status PTYs.
- [x] Modelo e esforço exibidos correspondem à requisição final. Evidência: wire/ledger/status tests and current PTY.
- [x] Não há nome obsoleto após troca, fallback ou cancelamento. Evidência: model switch/fallback attribution tests.
- [x] O nome do reasoning aparece mesmo sem override de worker. Evidência: zero-turn/model label tests.
- [x] Sessões concorrentes não contaminam o indicador umas das outras. Evidência:
  session-scoped ledger/state e child-isolation tests no shell 405, além da
  clearance de fonte. `GROK_HOME` isolado comprova isolamento de configuração
  e credenciais do ambiente de teste, não substitui a prova de estado entre
  sessões.

---

## 4. Subagents seguem as políticas do agente principal

### Objetivo

Garantir que todos os subagents usem as mesmas regras aplicáveis do agente principal, incluindo Jev, token saver e roteamento de modelos.

A imagem enviada destaca:
- O nome de modelo na lista de subagents.
- O indicador Jev no rodapé.
- A configuração reasoning/worker do principal.

Essas superfícies precisam representar corretamente a execução, não apenas configurações iniciais.

### Herança e execução

- [x] Inventariar os mecanismos reais de economia de tokens existentes. Evidência: source clearance e inventário final acima — tool subset, crushers, retention, dedup, read reuse, compression, compaction e context reserves.
- [x] Comparar principal e filhos quanto a:
  - Instruções e regras aplicáveis.
  - Flags e decisões Jev.
  - Esforço automático/manual.
  - Modelos reasoning, worker e utility.
  - Seleção/recorte de conteúdo.
  - Deduplicação.
  - Compressão/compaction.
  - Limites e reservas de contexto.
  - Guardas de segurança.
- [x] Não inventar uma configuração genérica de “token saver” se o projeto usa mecanismos separados. Evidência: source clearance e 405 isolated shell tests.
- [x] Reutilizar o pipeline compartilhado em vez de duplicar políticas por tipo de subagent. Evidência: source clearance dos caminhos compartilhados.
- [x] Corrigir a causa compartilhada quando houver diferença indevida. Evidência: combined implementation clearance PASS.
- [x] Validar o worker contra o modelo efetivo e o contexto do filho. Evidência: explicit child model/effort, fallback and context tests.
- [x] Respeitar modelos/esforços explicitamente solicitados. Evidência: `explicit_child_model_and_effort_survive_all_routing_passes` e precedence tests.
- [x] Definir e testar precedência entre herança e overrides. Evidência: model/effort precedence and resume tests in the 405 log.
- [x] Não desativar silenciosamente outras políticas ao aplicar um override. Evidência: permission/auto-mode and fallback tests.

### Caminhos obrigatórios

- [x] Criação normal. Evidência: resolution 94 and shell 405 tests.
- [x] Tipos especializados. Evidência: role/persona/model resolution tests.
- [x] Fork de contexto. Evidência: `forked_initial_context_*` tests.
- [x] Retomada de subagent. Evidência: `bootstrap_in_place_resume_reads_existing_transcript`, resume identity/source tests.
- [x] Subagents iniciados por workflows. Evidência: source clearance and workflow lifecycle tests in the 405 log.
- [x] Filhos aninhados, quando permitidos. Evidência: child policy/permission invariants in source clearance and isolated shell integration tests.

### Segurança e restrições

- [x] Preservar restrições próprias de cada tipo. Evidência: child tool policy and permission tests.
- [x] Um `explore` continua somente leitura. Evidência: child tool policy clearance.
- [x] Herança não pode ampliar permissões, ferramentas ou profundidade autorizada. Evidência: isolated permission tests and clearance.
- [x] Regras do principal não podem ser silenciosamente perdidas durante criação ou retomada. Evidência: inherited parent policy/resume tests.

### Estado e interface

- [x] Uma chamada do filho não altera configuração ou indicador de outro filho ou do principal. Evidência: session-scoped ledger, child isolation and current child PTY.
- [x] A lista/detalhe dos subagents mostra o modelo realmente ativo. Evidência: `controller-child-current-pty.log` and status/child source clearance.
- [x] Distinguir modelo configurado e modelo ativo quando necessário. Evidência: child configured-vs-active source tests.
- [x] O rodapé corresponde à sessão exibida. Evidência: current status and child PTYs plus footer pager tests.
- [x] Testar concorrência, fallback, retomada, conclusão e cancelamento sem estado obsoleto. Evidência: 405 shell lifecycle tests and resolution 94.

### Pontos de partida

- `crates/codegen/distill-shell/src/agent/subagent/handle_request.rs`
- `crates/codegen/distill-shell/src/agent/subagent/mod.rs`
- `crates/codegen/distill-subagent-resolution/`
- Caminhos compartilhados de criação e execução de sessões.
- Atualização da lista/status de subagents no pager, a localizar.

### Aceite

- [x] Testes comparativos pai/filho comprovam políticas efetivas. Evidência: source clearance, 405 shell tests and 94 resolution tests.
- [x] Testes inspecionam modelo enviado e comportamento de economia de tokens. Evidência: wire sampler requests, ledger/token tests and child routing tests.
- [x] Testes cobrem criação e retomada. Evidência: 405/94 logs and current child PTY.
- [x] Testes comprovam isolamento entre sessões. Evidência: session-scoped
  state tests, child-isolation tests, source clearance e child PTY. O
  `GROK_HOME` isolado é apenas controle de configuração/credenciais do
  ambiente de teste.
- [x] Compartilhar a função de criação de sessões não é usado como única prova de paridade. Evidência: independent resolution, shell integration and PTY evidence.

---
## 5. Migração de branding e contratos para Distill

### Objetivo

Localizar referências ao nome antigo e substituir pelo nome correto: **Distill**.

### Busca e alterações

- [x] Buscar sem distinção de maiúsculas/minúsculas, inclusive em arquivos ocultos. Evidência: `controller-branding-final-audit.txt` e auditoria final de aliases/URLs:
  - `Remote-Code`
  - `Remote Code`
  - `remote_code`
  - Formas compactas/capitalizadas relevantes.
- [x] Incluir código, documentação, scripts, arquivos ocultos de configuração, schemas e artefatos gerados pertinentes. Evidência: branding audit e `tools-tests.log` (9/0).
- [x] Atualizar textos e superfícies de produto. Evidência: audit final e current binary surfaces.
- [x] Corrigir links, verificando o destino real antes de substituí-los. Evidência: README alvo verificado como `https://github.com/samfaj/distill/blob/main/README.md`.
- [x] Atualizar identificadores pertinentes de maneira consistente. Evidência: namespace/schema tests and audit.
- [x] Avaliar contratos serializados e compatibilidade antes de renomear chaves. Evidência: legacy serde/env aliases in audit and shell tests.
- [x] Atualizar produtores, consumidores, schemas e testes em conjunto. Evidência: `tools-tests.log` 9/0 and shell 405/0.
- [x] Regenerar artefatos pelo processo existente quando aplicável. Evidência: `tool_meta_schema_is_up_to_date` passed.
- [x] Repetir a busca final e revisar cada ocorrência restante. Evidência: final audit classifies the nine intentional aliases/tests and deliberate external references.

### Compatibilidade

- [x] Não quebrar configurações ou dados antigos com substituição textual cega. Evidência: compatibility aliases plus 405 shell/config tests.
- [x] Se um identificador antigo precisar permanecer como alias de migração, registrar a justificativa e o teste. Evidência direta: `controller-branding-final-audit.txt` registra os aliases/tests legados restantes como intencionais.
- [x] Não esconder exceções de compatibilidade. Evidência: audit names remaining aliases/tests and the external telemetry contract.
- [x] Não manter o nome antigo nas superfícies atuais do produto. Evidência final: a auditoria lista apenas aliases de compatibilidade/testes e referências externas deliberadas; a revisão de branding foi encerrada no snapshot atual.

### Ocorrências iniciais encontradas

Esta lista não é exaustiva e deve ser revalidada.

| Local | Ocorrência |
|---|---|
| `crates/codegen/distill-pager/src/slash/commands/docs.rs` | URL antiga do repositório |
| `crates/codegen/distill-pager/docs/user-guide/04-slash-commands.md` | Mesma URL antiga |
| `crates/codegen/distill-pager/src/app/acp_handler/settings.rs` | `DISTILL_ANNOUNCEMENTS` + fallback legado testado |
| Testes de anúncios e `list.md` | `DISTILL_ANNOUNCEMENTS` |
| `crates/codegen/distill-shell/src/codex_auth.rs` | `DISTILL_CODEX_CLIENT_VERSION` + fallback legado testado |
| `crates/codegen/distill-tools/schema/tool_meta.schema.json` | namespaces canônicos `distill`, `distill_concise`, `distill_hashline` |
| `plan/plan.md` | `REMOTE_CODE_SUBAGENT_TELEMETRY_COMMAND` é contrato externo do bridge de telemetria e permanece sem renomear |

As referências antigas usadas como termos de busca, no arquivo histórico e no
bridge externo de telemetria são deliberadas. Elas não são superfícies atuais
do produto. O alias de env e os aliases serde permanecem apenas para ler
configurações/dados antigos; novas gravações e documentos usam Distill.

### Aceite

- [x] Nenhuma menção antiga permanece ativa sem justificativa. Evidência: final audit classifies all remaining aliases/tests or deliberate external references.
- [x] URLs corrigidas apontam para destinos verificados. Evidência: README target and docs URL audit.
- [x] Configurações e contratos continuam funcionando. Evidência: shell/config/auth 405/0 and schema/tool 9/0.
- [x] Aliases de compatibilidade, se necessários, estão documentados e testados. Evidência direta: `controller-branding-final-audit.txt` e os testes de ambiente/contratos dos logs de shell/tools.

---

## 6. Mapa inicial do código

Revalidar caminhos e símbolos na árvore atual. Não confiar em números de linha históricos.

### Onboarding, tutorial e comandos

- `crates/codegen/distill-pager/src/slash/commands/tutorial.rs`
- `crates/codegen/distill-pager/src/slash/commands/mod.rs`
- `crates/codegen/distill-pager/src/views/tutorial.rs`
- `crates/codegen/distill-pager/src/views/modal.rs`

### Estado, renderização e inicialização

- `crates/codegen/distill-pager/src/app/app_view.rs`
- `crates/codegen/distill-pager/src/app/actions.rs`
- `crates/codegen/distill-pager/src/app/dispatch/status.rs`
- `crates/codegen/distill-pager/src/app/event_loop.rs`
- `crates/codegen/distill-pager/src/app/session_startup.rs`
- `crates/codegen/distill-pager/src/app/dispatch/session/lifecycle.rs`

### Login e modelos

- `crates/codegen/distill-pager/src/app/dispatch/auth.rs`
- `crates/codegen/distill-pager/src/app/dispatch/task_result.rs`
- `crates/codegen/distill-pager/src/app/dispatch/settings/setters.rs`
- `crates/codegen/distill-pager/src/slash/commands/model.rs`
- `crates/codegen/distill-pager/src/slash/commands/worker_model.rs`

### Roteamento e indicador

- `crates/codegen/distill-shell/src/session/acp_session_impl/jev_routing.rs`
- `crates/codegen/distill-shell/src/session/acp_session_impl/sampler_turn.rs`
- `crates/codegen/distill-shell/src/jev.rs`
- `crates/codegen/distill-pager/src/views/turn_status.rs`

### Subagents

- `crates/codegen/distill-shell/src/agent/subagent/handle_request.rs`
- `crates/codegen/distill-shell/src/agent/subagent/mod.rs`
- `crates/codegen/distill-subagent-resolution/`

---

## 7. Revisão e verificação obrigatórias

### Testes automatizados

- [x] Escrever regressões que reproduzam os defeitos confirmados. Evidência: footer, onboarding, status, model and child PTY regressions plus focused unit tests.
- [x] Testar a intenção dos requisitos, não apenas valores de funções auxiliares. Evidência: six current-binary PTYs and wire/integration tests.
- [x] Cobrir o caminho integrado:
  decisão → configuração final → requisição → estado/evento → indicador.
- [x] Usar decisões controladas para testes determinísticos de roteamento. Evidência: 13 controlled routing tests.
- [x] Executar formatação, compilação e testes relevantes de `distill-shell`, `distill-pager` e demais crates afetados. Evidência: final controller compile/test logs and 405/431/139/9 suites.
- [x] Seguir os comandos e convenções atuais do repositório. Evidência: controller-owned package lanes and isolated PTY targets.
- [x] Reexecutar testes após qualquer correção posterior à revisão. Evidência: current footer/model/onboarding PTY rerun after terminal delta clearance.

### Verificação interativa

- [x] Executar o app real via terminal/PTY com configuração isolada. Evidência: six current-binary PTY cases with isolated harness endpoints.
- [x] Verificar terminal normal e estreito. Evidência: normal/narrow onboarding plus 8x80 resize guard.
- [x] Exercitar onboarding, navegação, persistência, erros e cancelamentos. Evidência: current onboarding artifacts and 2/0 log.
- [x] Verificar modelos e indicadores durante as transições. Evidência: current status 2/0 and model 1/0 logs.
- [x] Verificar subagents e concorrência. Evidência: current child 1/0 plus 405/94 lifecycle and isolation tests.
- [x] Testar abertura de URL e falha do navegador. Evidência: exact X URL/browser-error/Finish frame; browser policy block was preserved.
- [x] Diferenciar prova com mocks de prova OAuth real. Evidência direta: `controller-pty-inspection.md` identifica a PTY/mock isolada e declara OAuth real não verificado.
- [x] Se OAuth exigir interação humana, registrar o que foi e não foi verificado. Evidência direta: OAuth real permanece explicitamente não verificado; a política de URL local não foi contornada.
- [x] Não alterar credenciais reais nem seguir contas automaticamente. Evidência direta: a inspeção PTY registra ausência de credenciais reais, browser real e follow.

### Regressões

- [x] `/tutorial` e `/tour`. Evidência: focused/footer pager tutorial suites.
- [x] Regressões de autenticação dos três provedores nos caminhos mock/estado:
  `controller-pager-focused-retry.log`, `controller-shell-permissions-auth.log`
  e os testes de retorno/cancelamento passaram. Isso não prova login OAuth
  live; essa limitação permanece explícita acima.
- [x] `/model`. Evidência: current model PTY 1/0 and focused model dispatch tests.
- [x] `/worker-model`. Evidência distinta: `controller-worker-command-tests.log`
  (19 PASS/0 FAIL) inclui `worker_selection_resolves_display_names_and_validates_effort`,
  aliases, esforços e `clear`; o fixture de onboarding positivo também passa
  pela persistência real de `SetTierLight`/worker. Isso cobre a regressão de
  parsing/ação e o caminho guiado, mas não é uma declaração de OAuth live nem
  de um PTY digitando `/worker-model`.
- [x] Configuração e persistência de tiers. Evidência: onboarding persisted model frame plus settings/routing tests.
- [x] Inicialização, retomada e fork. Evidência: 405 shell and 94 resolution tests.
- [x] Relatório de uso por modelo/esforço. Evidência: ledger/model/effort tests in the 405 suite.
- [x] Criação, retomada e cancelamento de subagents. Evidência: 405/94 lifecycle tests and current child PTY.

### Interface web, caso seja afetada

- [x] Se alguma aplicação web for alterada, verificar no navegador os fluxos completos. N/A: nenhum produto web foi alterado.
- [x] Verificar páginas que compartilham estado ou componentes. N/A: nenhum produto web foi alterado.
- [x] Testar desktop/mobile quando houver mudanças visuais. N/A: nenhum produto web foi alterado.
- [x] Para a interface de terminal, screenshots no navegador não substituem interação via terminal. Evidência: actual PTY frames are the proof; local HTML browser inspection remained policy-blocked.

### Revisão final

- [ ] Revisar o diff completo.
- [ ] Procurar regressões além do caminho feliz.
- [ ] Remover apenas complexidade introduzida pelo próprio trabalho.
- [ ] Evitar refatorações alheias.
- [ ] Atualizar evidências de todos os gates.
- [ ] Executar validadores de gates, delegação e relatório da skill `/sam-orchestrate`.
- [ ] Não declarar COMPLETE com requisitos pendentes ou provas inexistentes.

---

## 8. Checkpoints, commit, push e GitHub Release

A publicação final foi concluída pelo controlador após a verificação funcional,
a revisão independente e o gate explícito. O source publicado é a tag
`v2.0.0`/SHA `b9d8f23ea845ef7fadc5f22a089e61f23d0a9175`; somente este handback
documental ainda aguarda commit/push/revisão final, sem atribuir-lhe o SHA
remoto antes disso.

### Preparação

- [x] Verificar status, branch e remoto. Evidência: source SHA publicado
  `b9d8f23ea845ef7fadc5f22a089e61f23d0a9175` e release readback.
- [x] Verificar tags/releases existentes e convenção de versionamento.
  Evidência: preflight registrou tag v2 ausente e latest release v1.5.2;
  `v2.0.0` foi criada sem reutilização.
- [x] Verificar autenticação do `gh`. Evidência: publication preflight com
  permissões de admin/maintain/push e gate aprovado pelo controller.
- [x] Ler o processo de release e os workflows atuais. Evidência:
  `.github/workflows/release.yml` e workflow `35492066534`, 5/5 jobs PASS.
- [x] Revisar todos os arquivos a incluir, inclusive não rastreados. Evidência: `test.txt` foi inspecionado quanto ao estado e permanece excluído/desconhecido.
- [x] Não publicar segredos, credenciais ou temporários. Evidência: PTY/harness isolado; nenhuma credencial real foi alterada.
- [x] Não incluir trabalho de origem desconhecida sem entender seu conteúdo. Evidência: `test.txt` excluído e preservado.
- [x] Não apagar trabalho do usuário para “limpar” a árvore. Evidência: nenhum arquivo desconhecido foi removido.
- [ ] Resolver explicitamente qualquer conflito entre segurança e o pedido de incluir “all”.

### Publicação

- [x] Confirmar evidências funcionais finais e revisão independente antes da
  publicação. Evidência: `final-prepublication-clearance.md` PASS,
  `controller-publication-preflight.json` aprovado e release readback.
- [x] Definir a versão segundo a convenção existente. Evidência: tag
  `v2.0.0` e source SHA `b9d8f23ea845ef7fadc5f22a089e61f23d0a9175`.
- [x] Atualizar arquivos de versão e notas necessários. Evidência: release
  copy final em `/tmp/distill-todo-orchestration/release-notes-v2.md`, com o
  cabeçalho sem `[alpha]`.
- [x] Não reutilizar uma tag publicada indevidamente. Evidência:
  preflight registrou v2 ausente antes da publicação; `v2.0.0` foi criada para
  o source SHA acima.
- [ ] Fazer `git add` do conjunto final revisado após a aprovação do controlador.
- [ ] Criar o commit final, com descrição fiel, após a verificação final e a revisão independente.
- [ ] Fazer push do commit final ao remoto e branch corretos após a verificação final e a revisão independente.
- [x] Criar a tag somente após a verificação funcional final, a revisão
  independente e o gate explícito de publicação. Evidência: `v2.0.0` e
  `controller-publication-preflight.json`.
- [x] Confirmar que o SHA remoto corresponde ao commit validado: release
  source SHA `b9d8f23ea845ef7fadc5f22a089e61f23d0a9175`.
- [x] Criar a GitHub Release com `gh release`, usando a tag do commit validado.
  Evidência: `release-verification.json` e URL oficial.
- [x] Instalar/verificar os artefatos publicados somente após a tag e a
  release válidas. Evidência: `installed-verification.json`, instalador exit
  0 e asset macOS ARM com SHA verificado.
- [x] Incluir notas e artefatos exigidos pelo projeto. Evidência: 9/9 assets
  verificados e release body/copy preparada.
- [x] Acompanhar checks/builds necessários à release. Evidência:
  `release-run-final.json`, workflow 35492066534 com 5/5 jobs PASS.
- [x] Verificar tag, release e artefatos remotos. Evidência:
  `release-verification.json`, 4 checks de arquitetura/SHA-256 e release URL.
- [x] Não declarar distribuição concluída apenas porque o comando de criação da
  release retornou sucesso; a conclusão foi baseada também em asset readback,
  instalação, 7 casos pós-instalação e smoke live de ferramenta.

### Relatório final

- [x] Informar quais requisitos foram atendidos: release, assets, instalação e
  pós-instalação estão registrados; OAuth live dos três provedores permanece
  fora da prova.
- [x] Listar os testes realmente executados e seus resultados nos logs de
  release/pós-instalação e no handback E3.
- [x] Declarar limitações ou bloqueios remanescentes: OAuth live e checkpoint
  final destes documentos.
- [x] Informar o SHA do commit publicado: `b9d8f23ea845ef7fadc5f22a089e61f23d0a9175`.
- [x] Informar tag/versão: `v2.0.0` / `2.0.0`.
- [x] Informar URL da GitHub Release:
  `https://github.com/samfaj/distill/releases/tag/v2.0.0`.

---

## 9. Ordem sugerida

1. Revalidar o estado do repositório, ler suas instruções e congelar o pipeline `/sam-orchestrate`.
2. Escrever gates, contratos de integração e responsabilidades.
3. Corrigir roteamento e estabelecer a fonte de verdade do modelo ativo.
4. Garantir paridade dos subagents e isolamento de estado.
5. Conectar os indicadores do principal e dos filhos.
6. Implementar onboarding reutilizando os fluxos existentes e corrigidos.
7. Concluir migração de nome, documentação e testes.
8. Executar revisão integrada e walkthrough do terminal.
9. Corrigir problemas encontrados e repetir as verificações.
10. Fazer commit/push de cada checkpoint aprovado; somente após a verificação funcional final, a revisão independente e um gate explícito, criar tag, release e instalar/verificar os artefatos.

Trabalho independente pode ser delegado em paralelo com contratos e arquivos bem definidos. Dependências entre tarefas devem ser respeitadas.

## Definição de concluído

Todos os requisitos solicitados foram implementados e comprovados, nenhum pedido foi silenciosamente omitido, as verificações passaram e a publicação foi confirmada.

Se houver impedimento real, registrar exatamente:
- O requisito pendente.
- O motivo.
- A evidência do bloqueio.
- O que falta para prosseguir.

Não substituir implementação por documentação, configuração por comportamento real, nem relato de worker por verificação independente.
