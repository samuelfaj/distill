
# TODO — entrega completa do Distill

## 0. Objetivo

Implementar todos os requisitos deste documento, revisar o resultado, comprovar o funcionamento e, somente depois, executar:

1. `git add` das alterações revisadas.
2. Commit.
3. Push.
4. Criação da GitHub Release com `gh release`.
5. Verificação da publicação e dos artefatos exigidos pelo projeto.

Esta fase usa o pipeline atual de `/sam-orchestrate`; referências históricas a
`/sam-goal` não substituem esse pipeline.

### Estado conhecido

- As implementações dos workers estão na árvore compartilhada, mas a validação
  integrada ainda está em andamento; nenhum gate funcional deve ser marcado sem
  prova independente do controlador.
- Houve investigação do código e criação de `goal/GATES.md` com critérios e
  evidências pendentes.
- Nenhum commit, push ou release foi executado nesta conversa.
- Algumas operações foram bloqueadas pela proteção local, mesmo após autorização. Reavaliar o ambiente sem contornar mecanismos de segurança.
- A árvore compartilhada está deliberadamente suja com alterações dos workers;
  verificar o manifesto final por produtor antes de publicar.
- A origem e a finalidade de `test.txt` não foram confirmadas. Não apagar nem incluir cegamente.
- O catálogo Jev anteriormente existente em `todo.md` foi arquivado como histórico. Suas marcações não comprovam a conclusão destes novos requisitos.
- As decisões de permissão H1/H2/P5 e os badges `jev·veto` do catálogo antigo foram removidos e não podem ser reativados nem usados como evidência atual.

### Regras de execução

- [ ] Ler as instruções atuais do repositório e verificar a árvore antes de editar.
- [ ] Seguir a skill `/sam-orchestrate`: registrar DAG, ownership, gates verificáveis e evidências.
- [ ] Contar unidades independentes e aplicar a regra de delegação da skill.
- [ ] Quando houver delegação, definir arquivos e responsabilidades sem escritores concorrentes.
- [ ] O coordenador deve reexecutar os testes dos workers antes de aceitar os resultados.
- [ ] Reutilizar mecanismos existentes, fazer alterações cirúrgicas e não adicionar dependências.
- [ ] Preservar alterações não relacionadas.
- [ ] Não declarar sucesso sem evidência nem reduzir silenciosamente o escopo.
- [ ] Registrar bloqueios, testes não executados e limitações explicitamente.

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

- [ ] Criar um fluxo próprio de onboarding.
- [ ] Separar `/onboarding` do tutorial atual, pois hoje ele é um alias de `/tutorial`.
- [ ] Preservar `/tutorial` e `/tour`.
- [ ] Persistir a conclusão para evitar repetição automática após finalizar.
- [ ] Permitir reabrir o fluxo por `/onboarding` mesmo depois de concluído.
- [ ] Mostrar progresso, por exemplo: `Step 1 of 4`.
- [ ] Oferecer navegação por teclado, retorno e saída claros.
- [ ] Preservar as opções de mouse já suportadas pelos componentes existentes.
- [ ] Adaptar texto e controles a terminais estreitos e baixos.
- [ ] Definir e testar o comportamento de fechar, reabrir e retomar.
- [ ] Não marcar conclusão quando a gravação do estado falhar.
- [ ] Preservar fluxos de prompt inicial, retomada/fork, login obrigatório, agentes externos e execução não interativa.
- [ ] Documentar quando a abertura automática precisa ser adiada, sem marcar o onboarding como concluído.
- [ ] Não abrir um modal invisível em modo mínimo: implementar suporte ou oferecer transição explícita para o modo compatível.

### 1.2. Etapa 1 — OpenRouter e economia

Apresentar o benefício de maneira animada, curta e compreensível.

- [ ] Explicar como e por que OpenRouter pode ajudar a economizar no Distill.
- [ ] Explicar a escolha de modelos e o roteamento de tarefas adequadas para modelos mais econômicos, conforme a implementação real.
- [ ] Distinguir:
  - Redução de custo.
  - Redução de tokens totais.
  - Redução de tokens processados pelo modelo mais caro.
- [ ] Não prometer percentuais não comprovados.
- [ ] Não afirmar que OpenRouter, sozinho, reduz a quantidade de tokens.
- [ ] Oferecer uma ação clara para continuar.

Sugestão de título: `Make your AI budget go further`.

### 1.3. Etapa 2 — Login e seleção de `/model`

- [ ] Oferecer as opções:
  - Grok.
  - Codex (ChatGPT).
  - OpenRouter.
- [ ] Reutilizar autenticação existente e contas já conectadas.
- [ ] Permitir selecionar o modelo principal pela funcionalidade existente de `/model`.
- [ ] Manter a seleção dentro da experiência guiada, sem exigir que o usuário descubra os comandos sozinho.
- [ ] Mostrar carregamento, sucesso, erro e retry.
- [ ] Voltar à etapa correta após cancelar login ou picker.
- [ ] Confirmar que o catálogo fica atualizado após autenticar.
- [ ] Confirmar que a seleção foi efetivamente persistida.
- [ ] Não armazenar credenciais no estado do onboarding.
- [ ] Não desconectar outros provedores.

Sugestão de título: `Connect your AI`.

### 1.4. Etapa 3 — Seleção opcional de `/worker-model`

- [ ] Explicar brevemente o papel do worker.
- [ ] Permitir selecionar um worker pela funcionalidade existente.
- [ ] Oferecer `Skip` claramente.
- [ ] Pular deve preservar um worker previamente configurado.
- [ ] Respeitar compatibilidade de conexão, backend e credenciais.
- [ ] Preservar guardas de contexto e reserva de resposta.
- [ ] Explicar o modo de esforço necessário ao roteamento automático.
- [ ] Não deixar um worker aparentemente ativo que nunca possa ser selecionado sem informar o motivo.

Sugestão de título: `Choose a worker model`.

### 1.5. Etapa 4 — Convite para seguir no X

- [ ] Convidar o usuário, em inglês, para seguir `@samfajreldines`.
- [ ] Oferecer ação para abrir exatamente:
  `https://x.com/samfajreldines/`
- [ ] Abrir no navegador do usuário pelo mecanismo existente.
- [ ] Oferecer `Skip`, concluindo sem abrir o navegador.
- [ ] Tratar falha de abertura com URL acessível e opção de finalizar.
- [ ] Não seguir automaticamente.
- [ ] Não afirmar que o follow foi verificado.

Sugestão de título: `Stay in the loop`.

### Aceite do onboarding

- [ ] Percorrer as quatro etapas no app real.
- [ ] Testar skips, retorno, fechamento, erros e cancelamentos.
- [ ] Reiniciar e confirmar a persistência da conclusão e das configurações.
- [ ] Reabrir usando `/onboarding`.
- [ ] Confirmar ausência de regressões no tutorial, login e seleção de modelos.

---

## 2. Roteamento reasoning/worker

### Objetivo

Garantir que o Distill selecione corretamente entre reasoning e worker em cada chamada.

“Alternar corretamente” significa escolher conforme a tarefa e as políticas, não fazer round-robin obrigatório.

### Implementação e verificação

- [ ] Rastrear o caminho:
  decisão → configuração final → requisição enviada.
- [ ] Provar uma sequência reasoning → worker → reasoning usando decisões controladas.
- [ ] Confirmar o modelo enviado de verdade, não apenas o modelo configurado.
- [ ] Preservar comportamento seguro quando:
  - Não há worker.
  - O worker é incompatível.
  - A conversa mais a reserva de resposta não cabe.
  - A decisão é incerta ou incompleta.
  - Há timeout ou falha da camada de decisão.
  - O roteamento está desabilitado.
- [ ] Testar esforço automático e manual.
- [ ] Testar modelos com nenhuma ou apenas uma opção de esforço.
- [ ] Corrigir bloqueios indevidos à seleção do worker sem ignorar escolhas explícitas do usuário.
- [ ] Preservar as verificações de endpoint, backend e credenciais.
- [ ] Preservar a conversa e as restrições do modelo selecionado.
- [ ] Verificar interação com modelos locais/utility existentes.
- [ ] Verificar retries e caminhos de recuperação sem persistir indevidamente a seleção anterior.

### Pontos já observados — revalidar na árvore atual

Arquivo:
`crates/codegen/distill-shell/src/session/acp_session_impl/jev_routing.rs`

- `jev_choose_model_and_effort` depende de esforço automático.
- Há retorno antecipado quando existem menos de duas opções de esforço.
- Verificar se isso impede uma decisão válida de modelo, mesmo quando existe worker elegível.
- As guardas de compatibilidade e contexto já existem e não devem ser removidas para forçar alternância.

### Aceite

- [ ] Testes capturam os modelos realmente enviados em cada chamada.
- [ ] Casos elegíveis usam worker conforme a decisão.
- [ ] Casos que exigem reasoning usam reasoning.
- [ ] Casos inválidos/incertos mantêm o comportamento seguro.
- [ ] Não há troca de modelo apenas para produzir uma aparência de alternância.

---

## 3. Mostrar o modelo realmente usado no status

### Objetivo

A linha de atividade deve identificar o modelo efetivo da chamada.

Problema mostrado pelo usuário:

`Writing file… 5.5s · jev ×64 · medium`

O esforço aparece, mas o modelo não.

### Requisitos

- [ ] Mostrar identidade explícita tanto do reasoning quanto do worker.
- [ ] Mostrar o esforço efetivamente aplicado quando existir.
- [ ] Usar a configuração final enviada ao sampler como fonte de verdade.
- [ ] Registrar depois de todos os ajustes de modelo/esforço, incluindo tier, effort floor e overrides.
- [ ] Não usar apenas:
  - Modelo padrão salvo.
  - Decisão provisória.
  - Modelo da chamada anterior.
  - Modelo de uma chamada auxiliar.
- [ ] Durante execução de ferramentas, mostrar o modelo que originou aquela ação.
- [ ] Não afirmar que uma inferência está ativa enquanto o modelo estiver ocioso.
- [ ] Atualizar corretamente em retry, fallback, cancelamento, próxima chamada e próximo turno.
- [ ] Não deixar chamadas de classificação/utility sobrescreverem o indicador da chamada principal.
- [ ] Identificar corretamente outro modelo quando ele realmente executar a chamada.
- [ ] Preservar atividade, tempo, contador Jev e controles.
- [ ] Manter legibilidade em terminais estreitos.
- [ ] Conferir coerência entre indicador, requisição enviada e relatório de uso por modelo/esforço.

### Lacunas observadas — confirmar antes de corrigir

1. `note_round_for_turn_report`, em `jev_routing.rs`:
   - O ledger recebe o modelo da configuração.
   - O indicador recebe `pending_route_model()`.
   - Isso permite mostrar apenas esforço quando a chamada usa reasoning.

2. `prepare_sampler_for_turn`, em:
   `crates/codegen/distill-shell/src/session/acp_session_impl/sampler_turn.rs`
   - O registro ocorre antes de `jev_apply_model_tier` e `jev_apply_effort_floor`.
   - O esforço registrado pode divergir do efetivamente enviado.

3. `crates/codegen/distill-shell/src/jev.rs`:
   - O estado de atividade é global ao processo.
   - Verificar se a atribuição pode se misturar entre sessões concorrentes.

### Aceite

- [ ] Testes cobrem o caminho integrado da decisão até o indicador.
- [ ] Modelo e esforço exibidos correspondem à requisição final.
- [ ] Não há nome obsoleto após troca, fallback ou cancelamento.
- [ ] O nome do reasoning aparece mesmo sem override de worker.
- [ ] Sessões concorrentes não contaminam o indicador umas das outras.

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

- [ ] Inventariar os mecanismos reais de economia de tokens existentes.
- [ ] Comparar principal e filhos quanto a:
  - Instruções e regras aplicáveis.
  - Flags e decisões Jev.
  - Esforço automático/manual.
  - Modelos reasoning, worker e utility.
  - Seleção/recorte de conteúdo.
  - Deduplicação.
  - Compressão/compaction.
  - Limites e reservas de contexto.
  - Guardas de segurança.
- [ ] Não inventar uma configuração genérica de “token saver” se o projeto usa mecanismos separados.
- [ ] Reutilizar o pipeline compartilhado em vez de duplicar políticas por tipo de subagent.
- [ ] Corrigir a causa compartilhada quando houver diferença indevida.
- [ ] Validar o worker contra o modelo efetivo e o contexto do filho.
- [ ] Respeitar modelos/esforços explicitamente solicitados.
- [ ] Definir e testar precedência entre herança e overrides.
- [ ] Não desativar silenciosamente outras políticas ao aplicar um override.

### Caminhos obrigatórios

- [ ] Criação normal.
- [ ] Tipos especializados.
- [ ] Fork de contexto.
- [ ] Retomada de subagent.
- [ ] Subagents iniciados por workflows.
- [ ] Filhos aninhados, quando permitidos.

### Segurança e restrições

- [ ] Preservar restrições próprias de cada tipo.
- [ ] Um `explore` continua somente leitura.
- [ ] Herança não pode ampliar permissões, ferramentas ou profundidade autorizada.
- [ ] Regras do principal não podem ser silenciosamente perdidas durante criação ou retomada.

### Estado e interface

- [ ] Uma chamada do filho não altera configuração ou indicador de outro filho ou do principal.
- [ ] A lista/detalhe dos subagents mostra o modelo realmente ativo.
- [ ] Distinguir modelo configurado e modelo ativo quando necessário.
- [ ] O rodapé corresponde à sessão exibida.
- [ ] Testar concorrência, fallback, retomada, conclusão e cancelamento sem estado obsoleto.

### Pontos de partida

- `crates/codegen/distill-shell/src/agent/subagent/handle_request.rs`
- `crates/codegen/distill-shell/src/agent/subagent/mod.rs`
- `crates/codegen/distill-subagent-resolution/`
- Caminhos compartilhados de criação e execução de sessões.
- Atualização da lista/status de subagents no pager, a localizar.

### Aceite

- [ ] Testes comparativos pai/filho comprovam políticas efetivas.
- [ ] Testes inspecionam modelo enviado e comportamento de economia de tokens.
- [ ] Testes cobrem criação e retomada.
- [ ] Testes comprovam isolamento entre sessões.
- [ ] Compartilhar a função de criação de sessões não é usado como única prova de paridade.

---
## 5. Migração de branding e contratos para Distill

### Objetivo

Localizar referências ao nome antigo e substituir pelo nome correto: **Distill**.

### Busca e alterações

- [ ] Buscar sem distinção de maiúsculas/minúsculas, inclusive em arquivos ocultos:
  - `Remote-Code`
  - `Remote Code`
  - `remote_code`
  - Formas compactas/capitalizadas relevantes.
- [ ] Incluir código, documentação, scripts, arquivos ocultos de configuração, schemas e artefatos gerados pertinentes.
- [ ] Atualizar textos e superfícies de produto.
- [ ] Corrigir links, verificando o destino real antes de substituí-los.
- [ ] Atualizar identificadores pertinentes de maneira consistente.
- [ ] Avaliar contratos serializados e compatibilidade antes de renomear chaves.
- [ ] Atualizar produtores, consumidores, schemas e testes em conjunto.
- [ ] Regenerar artefatos pelo processo existente quando aplicável.
- [ ] Repetir a busca final e revisar cada ocorrência restante.

### Compatibilidade

- [ ] Não quebrar configurações ou dados antigos com substituição textual cega.
- [ ] Se um identificador antigo precisar permanecer como alias de migração, registrar a justificativa e o teste.
- [ ] Não esconder exceções de compatibilidade.
- [ ] Não manter o nome antigo nas superfícies atuais do produto.

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

- [ ] Nenhuma menção antiga permanece ativa sem justificativa.
- [ ] URLs corrigidas apontam para destinos verificados.
- [ ] Configurações e contratos continuam funcionando.
- [ ] Aliases de compatibilidade, se necessários, estão documentados e testados.

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

- [ ] Escrever regressões que reproduzam os defeitos confirmados.
- [ ] Testar a intenção dos requisitos, não apenas valores de funções auxiliares.
- [ ] Cobrir o caminho integrado:
  decisão → configuração final → requisição → estado/evento → indicador.
- [ ] Usar decisões controladas para testes determinísticos de roteamento.
- [ ] Executar formatação, compilação e testes relevantes de `distill-shell`, `distill-pager` e demais crates afetados.
- [ ] Seguir os comandos e convenções atuais do repositório.
- [ ] Reexecutar testes após qualquer correção posterior à revisão.

### Verificação interativa

- [ ] Executar o app real via terminal/PTY com configuração isolada.
- [ ] Verificar terminal normal e estreito.
- [ ] Exercitar onboarding, navegação, persistência, erros e cancelamentos.
- [ ] Verificar modelos e indicadores durante as transições.
- [ ] Verificar subagents e concorrência.
- [ ] Testar abertura de URL e falha do navegador.
- [ ] Diferenciar prova com mocks de prova OAuth real.
- [ ] Se OAuth exigir interação humana, registrar o que foi e não foi verificado.
- [ ] Não alterar credenciais reais nem seguir contas automaticamente.

### Regressões

- [ ] `/tutorial` e `/tour`.
- [ ] Autenticação dos três provedores.
- [ ] `/model`.
- [ ] `/worker-model`.
- [ ] Configuração e persistência de tiers.
- [ ] Inicialização, retomada e fork.
- [ ] Relatório de uso por modelo/esforço.
- [ ] Criação, retomada e cancelamento de subagents.

### Interface web, caso seja afetada

- [ ] Se alguma aplicação web for alterada, verificar no navegador os fluxos completos.
- [ ] Verificar páginas que compartilham estado ou componentes.
- [ ] Testar desktop/mobile quando houver mudanças visuais.
- [ ] Para a interface de terminal, screenshots no navegador não substituem interação via terminal.

### Revisão final

- [ ] Revisar o diff completo.
- [ ] Procurar regressões além do caminho feliz.
- [ ] Remover apenas complexidade introduzida pelo próprio trabalho.
- [ ] Evitar refatorações alheias.
- [ ] Atualizar evidências de todos os gates.
- [ ] Executar validadores de gates, delegação e relatório da skill `/sam-orchestrate`.
- [ ] Não declarar COMPLETE com requisitos pendentes ou provas inexistentes.

---

## 8. Commit, push e GitHub Release

A publicação é um gate separado e não faz parte desta primeira fase. Não fazer
commit, push, tag ou release antes da revisão do controlador e da aprovação
explícita do gate de publicação.

### Preparação

- [ ] Verificar status, branch e remoto.
- [ ] Verificar tags/releases existentes e convenção de versionamento.
- [ ] Verificar autenticação do `gh`.
- [ ] Ler o processo de release e os workflows atuais.
- [ ] Revisar todos os arquivos a incluir, inclusive não rastreados.
- [ ] Não publicar segredos, credenciais ou temporários.
- [ ] Não incluir trabalho de origem desconhecida sem entender seu conteúdo.
- [ ] Não apagar trabalho do usuário para “limpar” a árvore.
- [ ] Resolver explicitamente qualquer conflito entre segurança e o pedido de incluir “all”.

### Publicação

- [ ] Confirmar evidências de todos os requisitos antes de solicitar publicação.
- [ ] Definir a versão segundo a convenção existente.
- [ ] Atualizar arquivos de versão e notas necessários.
- [ ] Não reutilizar uma tag publicada indevidamente.
- [ ] Fazer `git add` do conjunto revisado após aprovação do controlador.
- [ ] Criar commit(s) com descrição fiel após aprovação do controlador.
- [ ] Fazer push ao remoto e branch corretos após aprovação do controlador.
- [ ] Confirmar que o SHA remoto corresponde ao commit validado.
- [ ] Criar a GitHub Release com `gh release`, usando a tag do commit validado.
- [ ] Incluir notas e artefatos exigidos pelo projeto.
- [ ] Acompanhar checks/builds necessários à release.
- [ ] Verificar tag, release e artefatos remotos.
- [ ] Não declarar distribuição concluída apenas porque o comando de criação da release retornou sucesso.

### Relatório final

- [ ] Informar quais requisitos foram atendidos.
- [ ] Listar os testes realmente executados e seus resultados.
- [ ] Declarar limitações ou bloqueios remanescentes.
- [ ] Informar SHA do commit.
- [ ] Informar tag/versão.
- [ ] Informar URL da GitHub Release.

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
10. Somente após a revisão do controlador e um gate explícito, fazer commit, push e release.

Trabalho independente pode ser delegado em paralelo com contratos e arquivos bem definidos. Dependências entre tarefas devem ser respeitadas.

## Definição de concluído

Todos os requisitos solicitados foram implementados e comprovados, nenhum pedido foi silenciosamente omitido, as verificações passaram e a publicação foi confirmada.

Se houver impedimento real, registrar exatamente:
- O requisito pendente.
- O motivo.
- A evidência do bloqueio.
- O que falta para prosseguir.

Não substituir implementação por documentação, configuração por comportamento real, nem relato de worker por verificação independente.
