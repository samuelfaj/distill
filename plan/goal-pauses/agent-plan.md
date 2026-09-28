# Plano de implementação: Diagnosticar por que todos os goals param antes de concluir e planejar a correção completa do harness Distill, com base em 3 execuções reais \(macos\-app, lacco, empath\)\.

Status: **READY\_TO\_EXECUTE** · Depth: **deep**

## Objetivo
Fazer os goals do Distill avançarem até a conclusão: sem pausas por falta de progresso, com o reasoning model assumindo quando o trabalho trava, pausas só para bloqueios que dependem do usuário, Jev e reviews alinhados ao objetivo e à entrega reais, e nenhum trabalho autônomo com o goal pausado\.

## Limites do escopo
### Critérios de sucesso
- Nenhum caminho automático pausa um goal por falta de progresso \(checkpoint, fim de rodada ou stall do verificador\)\.
- Com 2 avaliações seguidas sem progresso, um reasoning\-executor roda no workspace de entrega e se repete até surgir progresso, quando o main model volta a conduzir sozinho\.
- Progresso inclui sinais determinísticos: fingerprint novo do workspace, resultado novo de check \(inclusive Playwright, xcodebuild, npm run e2e e tarefas em background\) ou evidência nova do avaliador\.
- Um bloqueio requires\_user pausa na primeira avaliação que o confirma, com opções, e '/goal resume &lt;instrução&gt;' retoma o mesmo goal com a instrução\.
- Instruções explícitas do objetivo do goal prevalecem sobre instruções do repositório em planner, worker, avaliador, verificador e reviewers\.
- Planos, conselhos, hints e reviews do Jev usam o objetivo do goal como pedido atual\.
- O review de entrega analisa a raiz e o baseline reais da entrega, incluindo commits, e roda nessa raiz\.
- Reviews de entrega têm no máximo 3 veredictos 'revise' por turno e não repetem review com evidência inalterada\.
- Com o goal pausado automaticamente, o turno termina e nenhum wake de background inicia trabalho até o usuário agir\.
- Cada avaliação do goal e cada execução de escalonamento fica registrada em goal/evaluations\.jsonl\.
- As suítes afetadas, a suíte lib do distill\-shell, os testes do distill\-agent e o clippy passam\.

### Fora do escopo
- Mudar o cap de 10 rejeições do verificador \(pausa BackOff\)\.
- Corrigir a contabilização de tokens do goal\.
- Mudar o effort padrão do usuário ou a política de effort do Jev\.
- Dar shell ao code\-reviewer\.
- Pular automaticamente tasks com human gate\.
- Commit, push ou release sem pedido\.

## Decisões e restrições
1\) Checks e sinais determinísticos \(fingerprint do workspace e resultados novos de checks, inclusive em background\) passam a contar como progresso\. 2\) Nenhum caminho pausa por falta de progresso: após 2 avaliações sem progresso, o goal chama o reasoning\-executor no workspace de entrega, repetindo até surgir progresso, com o strategist a cada 3 execuções sem avanço\. 3\) O avaliador ganha blocker\_kind: bloqueio 'requires\_user' pausa na primeira confirmação, até em checkpoint, com opções, e '/goal resume &lt;instrução&gt;' leva a resposta ao worker\. 4\) A instrução explícita do goal vence regras do repositório em todos os prompts\. 5\) O Jev ancora no objetivo do goal\. 6\) O review usa a raiz e o baseline reais da entrega, tem limite de 3 'revise' por turno e não repete review com evidência inalterada\. 7\) Com o goal pausado, o turno acaba e wakes ficam pendentes\. 8\) Cada avaliação fica registrada em goal/evaluations\.jsonl\.

### Invariantes
- Estado de goal gravado por versões anteriores continua carregando\.
- Bloqueios que dependem do usuário, falhas de infraestrutura, pausa manual e o cap do verificador continuam pausando\.
- O code\-reviewer continua read\-only\.
- A semântica de compactação \(is\_real\_user\_turn\) não muda\.
- Permissões, sandbox e modos de aprovação não mudam\.

### Restrições
- Seguir o estilo do repo \(Rust, testes no módulo e em acp\_session\_tests\)\.
- Rodar testes com GROK\_HOME temporário, RUST\_MIN\_STACK=16777216 e \-\-test\-threads=1\.
- Build local e aceite ao vivo só com autorização do usuário\.

### Não fazer
- Reverter o commit 398b161a inteiro\.
- Resolver só aumentando limiares\.
- Conceder shell ao reviewer\.

## Etapas ordenadas de implementação

### 1. Reconhecer checks reais \(Playwright, xcodebuild, e2e e tarefas em background\)
Os 19 runs Playwright do lacco e os builds xcodebuild ficaram invisíveis para reviewers e para o sinal de progresso \(E\-032, E\-033\)\.

**Implementação**
- Em looks\_like\_check\_command \(reasoning\_gates\.rs:959\), adicionar npx/pnpx/bunx como runners e reconhecer 'playwright test', 'xcodebuild' com build\|test\|build\-for\-testing\|test\-without\-building, 'detox test', 'maestro test', 'cypress run' e scripts 'e2e', 'test:\*' ou '\*:test' depois de npm\|pnpm\|yarn\|bun run\.
- Manter como não\-check: 'npx playwright install', 'npm run dev', 'npx expo start', 'xcodebuild \-list', 'npm install'\.
- Em note\_tool\_result \(reasoning\_gates\.rs:291\), registrar também ToolOutput::TaskOutput\(Result\) com exit\_code Some e comando de check \(failed = exit\_code \!= 0\), com a mesma deduplicação dos checks Bash\.
- Estender o teste check\_commands\_are\_tests\_builds\_type\_checks\_and\_lints com os casos positivos e negativos acima e criar um teste para o caminho TaskOutput\.

**Caminhos relevantes**
- crates/codegen/distill\-shell/src/session/acp\_session\_impl/reasoning\_gates\.rs

**Definição de pronto**
- Os comandos listados são reconhecidos e os negativos continuam falsos\.
- Um TaskOutput concluído com 'npx playwright test' aparece em review\_checks\(\)\.

**Como verificar**
- Testes de detecção de checks e do caminho TaskOutput\. — Depois de S\-001: GROK\_HOME=$\(mktemp \-d\) RUST\_MIN\_STACK=16777216 cargo test \-p distill\-shell \-\-lib \-\- \-\-test\-threads=1 reasoning\_gates::tests

### 2. Sinais determinísticos de progresso no loop de goal
Hoje só observações emitidas pelo avaliador contam como progresso\. Trabalho não commitado não tem revision, e o lacco pausou com um spec passando \(E\-003, E\-004, E\-018\)\.

**Implementação**
- Pré\-requisito: concluir S\-001 antes deste passo\.
- Em goal\_classifier/evidence\.rs, criar workspace\_fingerprint\(root\): blake3 de 'rev\-parse HEAD', 'diff HEAD \-\-binary' e hash dos não rastreados via 'git hash\-object \-\-stdin\-paths', reusando git\_command e DIFF\_COMMAND\_TIMEOUT\. Retorna None se a raiz não for Git\.
- Raiz de entrega = verification\_target validado \(validate\_verification\_target\) ou, na falta dele, o cwd da sessão\.
- Em GoalProgress \(goal\_evaluator\.rs:36\), adicionar seen\_workspace\_fingerprints \(máx\. 64\) e seen\_check\_outcomes \(máx\. 256, chave blake3\(command\_hash\|cwd\|failed\|fingerprint\)\) com serde\(default\)\. Manter no\_progress\_rounds, agora contando avaliações sem progresso\.
- Fazer record\(\) receber os sinais: há progresso quando um critério passa a verified, há observação nova, o fingerprint é inédito ou há resultado de check inédito\. Atualizar as duas chamadas em goal\.rs \(validação em evaluate\_goal\_round e merge em evaluate\_goal\_progress\)\.
- Enviar harness\_observed ao avaliador \(workspace\_changed, raiz, até 30 arquivos alterados, checks novos com comando, status e 600 chars finais\) e instruir no SYSTEM\_PROMPT que esses são fatos capturados, usando o fingerprint como revision de trabalho não commitado\.
- Testes em goal\_evaluator\.rs: mudança de workspace zera o contador; o mesmo check no mesmo estado não conta; check novo que passa conta; estado de goal gravado pela 2\.0\.16 continua deserializando\.

**Caminhos relevantes**
- crates/codegen/distill\-shell/src/session/goal\_evaluator\.rs
- crates/codegen/distill\-shell/src/session/goal\_classifier/evidence\.rs
- crates/codegen/distill\-shell/src/session/acp\_session\_impl/goal\.rs

**Definição de pronto**
- O contador não sobe quando o workspace mudou ou há check novo\.
- Repetir um check no mesmo estado não conta como progresso\.
- Estado antigo carrega sem erro\.

**Como verificar**
- Testes de sinais de progresso e de compatibilidade de estado\. — Depois de S\-002: GROK\_HOME=$\(mktemp \-d\) RUST\_MIN\_STACK=16777216 cargo test \-p distill\-shell \-\-lib \-\- \-\-test\-threads=1 goal\_evaluator::tests

### 3. Nunca pausar por falta de progresso: escalar para o reasoning model
Decisão D3\. As pausas no\_progress do macos\-app e do lacco \(E\-013, E\-017, E\-018\) e o stall do verificador \(E\-010, E\-011\) passam a acionar o reasoning model em vez de parar\.

**Implementação**
- Pré\-requisito: concluir S\-002 antes deste passo\.
- Em evaluate\_goal\_progress \(goal\.rs:1643\), remover a auto\-pausa NoProgress de checkpoints e fins de rodada\. Com no\_progress\_rounds &gt;= 2, rodar o reasoning\-executor e injetar o resultado como system reminder \(mesmo formato de append\_stall\_note\), repetindo a cada nova avaliação sem progresso\.
- Dar a reasoning\_role \(jev\_routing\.rs:186\) um parâmetro cwd, preenchido com a raiz de entrega do S\-002\. O prompt do executor leva objetivo, critérios pendentes, next\_step do avaliador, harness\_observed e checks recentes\.
- A cada 3 execuções do executor sem progresso, rodar o strategist uma vez \(maybe\_run\_goal\_strategist\) e colocar a nota dele no próximo prompt do executor\. Adaptar goal\_strategist\_prompt\.md para o gatilho 'sem progresso', além do 'rejeição do verificador'\.
- Na primeira avaliação com progresso, zerar o escalonamento e registrar um evento no histórico\. Enquanto o escalonamento do goal estiver ativo, pular o handoff de stall do Jev \(jev\_routing\.rs:743\) para não haver dois executores ao mesmo tempo\.
- No stall do verificador \(goal\.rs:348 e caminho legado goal\.rs:2455\), trocar auto\_pause\_for\_classifier\_stall por strategist mais executor, sem pausar\. Manter o enum NoProgressPaused para compatibilidade, mas nunca mais produzi\-lo automaticamente\.
- Sem reasoning model configurado, rodar o executor no main model com reasoning\_effort 'high' via SubagentRuntimeOverrides\.
- Reescrever o teste goal\_round\_pauses\_repeated\_continue\_without\_losing\_proof para 'escala sem perder prova' \(status Active, spawn do reasoning\-executor com cwd na raiz, prova mantida após compactação\) e testar a volta ao normal quando há progresso\. Atualizar o trecho de goal\_rules\.md sobre 'bounded no\-progress'\.

**Caminhos relevantes**
- crates/codegen/distill\-shell/src/session/acp\_session\_impl/goal\.rs
- crates/codegen/distill\-shell/src/session/acp\_session\_impl/goal\_support\.rs
- crates/codegen/distill\-shell/src/session/acp\_session\_impl/jev\_routing\.rs
- crates/codegen/distill\-shell/src/session/goal\_tracker\.rs
- crates/codegen/distill\-shell/src/session/templates/goal\_rules\.md
- crates/codegen/distill\-shell/src/session/templates/goal\_strategist\_prompt\.md
- crates/codegen/distill\-shell/src/session/acp\_session\_tests/goal/goal\_compaction\_reseed\_tests\.rs

**Definição de pronto**
- Nenhum caminho automático grava NoProgressPaused\.
- 2 avaliações sem progresso criam um reasoning\-executor com cwd na raiz de entrega\.
- Progresso encerra o escalonamento\.
- Sem reasoning model, o executor roda no main model\.

**Como verificar**
- Testes de ator: sem pausa NoProgress, spawn do reasoning\-executor com cwd na raiz, desescalonamento com progresso e stall do verificador escalando\. — Depois de S\-003: GROK\_HOME=$\(mktemp \-d\) RUST\_MIN\_STACK=16777216 cargo test \-p distill\-shell \-\-lib \-\- \-\-test\-threads=1 goal\_compaction\_reseed\_tests goal\_tracker::tests

### 4. Bloqueio que depende do usuário pausa cedo, com opções e '/goal resume &lt;instrução&gt;'
Decisão D2\. O empath gastou 25 min e 20M tokens depois de o gate já ser conhecido \(E\-021\), a pausa ofereceu um único caminho \(E\-022\) e '/goal resume &lt;texto&gt;' cria um goal novo \(E\-047\)\.

**Implementação**
- Pré\-requisito: concluir S\-002 antes deste passo\.
- Em GoalEvaluatorVerdict e no schema, adicionar blocker\_kind \('' \| 'transient' \| 'requires\_user'\), obrigatório e não vazio só em blocked \(mesma validação de blocker\_key\)\.
- No SYSTEM\_PROMPT, definir requires\_user \(label de human gate ou aprovação, decisão só do usuário, acesso que só ele concede, dependência aberta de outra pessoa\) e exigir no next\_step 'Opções:' com 1 a 3 caminhos compatíveis com o objetivo, como a próxima task elegível\.
- Em evaluate\_goal\_progress, tratar requires\_user antes do retorno de checkpoint \(goal\.rs:1656\) e pausar na hora com evidência, opções e a dica '/goal resume &lt;instrução&gt;'\. 'transient' mantém o streak de 3 em fim de rodada\.
- Em slash\_commands\.rs:351, fazer 'resume &lt;texto&gt;' virar GoalResume com guidance; atualizar os matches em turn\.rs:728 e slash\_exec\.rs:900\.
- No resume, gravar a guidance como mensagem do usuário marcada com &lt;goal\_resume\_guidance&gt; antes de retomar a continuação\.
- Testes: requires\_user num checkpoint pausa na hora com opções; transient continua exigindo 3; '/goal resume pegue a próxima' retoma o mesmo goal com a guidance\.

**Caminhos relevantes**
- crates/codegen/distill\-shell/src/session/goal\_evaluator\.rs
- crates/codegen/distill\-shell/src/session/acp\_session\_impl/goal\.rs
- crates/codegen/distill\-shell/src/session/acp\_session\_impl/goal\_support\.rs
- crates/codegen/distill\-shell/src/session/slash\_commands\.rs
- crates/codegen/distill\-shell/src/session/slash\_commands\_tests\.rs
- crates/codegen/distill\-shell/src/session/acp\_session\_impl/turn\.rs
- crates/codegen/distill\-shell/src/session/acp\_session\_impl/slash\_exec\.rs

**Definição de pronto**
- requires\_user pausa na primeira confirmação, inclusive em checkpoint\.
- A mensagem de pausa traz opções e a dica de resume\.
- '/goal resume &lt;texto&gt;' não cria goal novo\.

**Como verificar**
- Testes: requires\_user pausa em checkpoint com opções; transient mantém o streak; '/goal resume &lt;texto&gt;' retoma com guidance\. — Depois de S\-004: GROK\_HOME=$\(mktemp \-d\) RUST\_MIN\_STACK=16777216 cargo test \-p distill\-shell \-\-lib \-\- \-\-test\-threads=1 goal\_compaction\_reseed\_tests slash\_commands

### 5. Instrução explícita do goal vence regras do repositório
Decisão D1\. No macos\-app, planner, avaliador, worker e reviewers seguiram o AGENTS\.md contra o pedido 'trabalhe direto em production' e deixaram o critério impossível \(E\-015, E\-016, E\-040, E\-041, E\-042\)\.

**Implementação**
- Pré\-requisito: concluir S\-004 antes deste passo\.
- goal\_rules\.md e goal\_rules\_legacy\.md: instruções explícitas do objetivo prevalecem sobre AGENTS\.md, CLAUDE\.md, regras e skills do repositório quando conflitam; siga o objetivo, informe a sobreposição no relatório e nunca troque local ou entregável\.
- goal\_planner\_prompt\.md: nunca reescrever instrução explícita do OBJECTIVE para cumprir instrução do repositório; registrar a sobreposição em Risks/Contradictions como resolvida a favor do OBJECTIVE\.
- SYSTEM\_PROMPT do avaliador \(goal\_evaluator\.rs\) e goal\_verifier\_prompt\.md: requisito do OBJECTIVE não é dispensado nem reescopado por instrução conflitante do repositório, e cumpri\-lo não é defeito\.
- Prompts do Jev \(jev\_routing\.rs:133, plano em :249 e code\-reviewer em :1276\) e code\_reviewer em distill\-agent/src/config\.rs:1553: quando o pedido do usuário sobrepõe explicitamente uma instrução do projeto, seguir e julgar pelo pedido\.
- Adicionar asserts de conteúdo nos testes de template/prompt existentes, ou testes unitários via include\_str\!, garantindo a cláusula nos 7 textos\.

**Caminhos relevantes**
- crates/codegen/distill\-shell/src/session/templates/goal\_rules\.md
- crates/codegen/distill\-shell/src/session/templates/goal\_rules\_legacy\.md
- crates/codegen/distill\-shell/src/session/templates/goal\_planner\_prompt\.md
- crates/codegen/distill\-shell/src/session/templates/goal\_verifier\_prompt\.md
- crates/codegen/distill\-shell/src/session/goal\_evaluator\.rs
- crates/codegen/distill\-shell/src/session/acp\_session\_impl/jev\_routing\.rs
- crates/codegen/distill\-agent/src/config\.rs

**Definição de pronto**
- Todos os 7 textos contêm a regra de precedência\.
- Os testes de conteúdo passam\.

**Como verificar**
- Asserts de conteúdo da regra de precedência nos 7 textos\. — Depois de S\-005: GROK\_HOME=$\(mktemp \-d\) RUST\_MIN\_STACK=16777216 cargo test \-p distill\-shell \-\-lib \-\- \-\-test\-threads=1 goal\_evaluator jev\_routing e cargo test \-p distill\-agent code\_reviewer
- Aceite ao vivo com 3 goals no formato das sessões, auditando evaluations\.jsonl\. — Depois do build local e com autorização do usuário: rodar os 3 goals e checar zero pausas no\_progress, escalate→deescalate, pausa cedo com opção no empath e nenhum wake com goal pausado\.

### 6. Jev ancorado no objetivo do goal
O kickoff do goal some para o Jev \(E\-020, E\-023, E\-024\)\. Planos, conselhos, hints e reviews julgaram tudo contra 'por que pausou?' e mandaram o worker parar o cadastro real \(E\-019, E\-025, E\-026, E\-027\)\.

**Implementação**
- Pré\-requisito: concluir S\-004 antes deste passo\.
- Criar em jev\_routing\.rs um helper request\_anchor\(items, goal\): o último turno humano real ou, se houver kickoff de goal mais recente \(marcador 'A goal has been set:' de goal\_rules\.md:1\), o objetivo do goal\_tracker somado às mensagens &lt;goal\_resume\_guidance&gt; posteriores\.
- Usar request\_anchor em last\_real\_request \(:106, chamado em :679 e :1140\), em jev\_latest\_real\_human\_request \(:2167, que alimenta os hints de jev\_tool\_result\.rs:819\) e no início de work\_since\_request \(reasoning\_gates\.rs:831\)\.
- Não mudar compaction\_utils::is\_real\_user\_turn, que segue definindo a semântica de compactação\.
- Testes: a conversa \[humano 'por que pausou?', kickoff do goal\] ancora no objetivo; plano, review e hint recebem o objetivo; work\_since\_request começa depois do kickoff\.

**Caminhos relevantes**
- crates/codegen/distill\-shell/src/session/acp\_session\_impl/jev\_routing\.rs
- crates/codegen/distill\-shell/src/session/acp\_session\_impl/reasoning\_gates\.rs
- crates/codegen/distill\-shell/src/session/acp\_session\_impl/jev\_tool\_result\.rs

**Definição de pronto**
- Durante um goal, 'User request' dos consults do Jev é o objetivo\.
- A compactação não muda de comportamento\.

**Como verificar**
- Testes do request\_anchor com kickoff de goal e guidance\. — Depois de S\-006: GROK\_HOME=$\(mktemp \-d\) RUST\_MIN\_STACK=16777216 cargo test \-p distill\-shell \-\-lib \-\- \-\-test\-threads=1 jev\_routing::tests reasoning\_gates::tests

### 7. Review de entrega enxerga a entrega real \(raiz, baseline, commits\)
No macos\-app, 6 reviews receberam diff vazio do checkout errado; no lacco, o trabalho commitado ficou invisível; as edições mais novas somem depois do teto \(E\-015, E\-028, E\-029, E\-030, E\-031\)\.

**Implementação**
- Pré\-requisito: concluir S\-003 antes deste passo\.
- Criar review\_target\(\) em jev\_routing\.rs: verification\_target validado do goal \(qualquer status\) → changes\_baseline\_commit do goal no cwd → HEAD do turno, capturado na primeira prepare\_sampler\_for\_turn do turno e guardado em ReasoningGates \(zera por turno, jev\_ledger\.rs:268\)\.
- Com baseline, montar 'Changes' com capture\_changes\_diff\(baseline, raiz, created\_at\) \(commits, não commitados e lista de não rastreados\), revision via 'git \-C raiz rev\-parse HEAD' e reviewer com cwd = raiz \(parâmetro do S\-003\)\.
- Nunca trocar edições registradas por 'Current repository diff is empty\.' \(jev\_routing\.rs:1199\); com diff vazio e ledger com mudanças, enviar as duas fontes com a origem\.
- Em review\_changes \(reasoning\_gates\.rs:733\), priorizar as edições mais novas quando passar de REVIEW\_DIFF\_BYTES\.
- Abrir o prompt de review com 'Delivery worktree: &lt;raiz&gt;; baseline &lt;sha&gt;'\.
- Testes com repo temporário no padrão de delivery\_reviewer\_receives\_shell\_edits\_from\_the\_git\_diff: worktree ≠ cwd, commit depois do baseline e cwd do spawn = raiz\.

**Caminhos relevantes**
- crates/codegen/distill\-shell/src/session/acp\_session\_impl/jev\_routing\.rs
- crates/codegen/distill\-shell/src/session/acp\_session\_impl/reasoning\_gates\.rs
- crates/codegen/distill\-shell/src/session/acp\_session\_impl/sampler\_turn\.rs

**Definição de pronto**
- O review de um goal em worktree mostra o diff do worktree\.
- Commits feitos no turno aparecem no review\.
- O spawn do reviewer usa a raiz de entrega\.

**Como verificar**
- Testes com repo temporário: worktree ≠ cwd, commit depois do baseline e cwd do spawn\. — Depois de S\-007: GROK\_HOME=$\(mktemp \-d\) RUST\_MIN\_STACK=16777216 cargo test \-p distill\-shell \-\-lib \-\- \-\-test\-threads=1 jev\_routing::tests

### 8. Loop de review limitado e com contrato de evidência
No lacco foram 14 'revise' na fase Worker, sem teto; no macos\-app, só reescrever o relatório disparou novo review; reviewers pediam revise por não poderem rodar testes \(E\-019, E\-035, E\-036, E\-037, E\-038\)\.

**Implementação**
- Pré\-requisito: concluir S\-001, S\-007 antes deste passo\.
- Em note\_verdict \(reasoning\_gates\.rs:672\), contar 'revise' de todo review de entrega do turno; ao atingir MAX\_FLOW\_REVISIONS \(3\), instruir uma vez 'reporte os achados não resolvidos' e parar de revisar no turno, incluindo o review forçado por check com falha \(jev\_routing\.rs:1425\)\.
- Guardar a evidence key \(diff \+ identidade dos checks\) do último 'revise' e não revisar de novo se ela não mudou; só o texto do relatório mudar não gera review\.
- Nos prompts de review \(jev\_routing\.rs:1276, reasoning\_system\_prompt e code\_reviewer em config\.rs:1553\), dizer que Checks são execuções capturadas pelo harness e que não conseguir rodar ou ver evidência é limite a declarar no approve\. Revise só para defeito concreto e corrigível, com arquivo/linha ou evidência\.
- Testes: o loop opcional para após 3 'revise'; evidência inalterada com relatório reescrito não gera review novo\.

**Caminhos relevantes**
- crates/codegen/distill\-shell/src/session/acp\_session\_impl/reasoning\_gates\.rs
- crates/codegen/distill\-shell/src/session/acp\_session\_impl/jev\_routing\.rs
- crates/codegen/distill\-agent/src/config\.rs

**Definição de pronto**
- No máximo 3 'revise' por turno em qualquer fase\.
- Sem review repetido com evidência igual\.

**Como verificar**
- Testes do teto de 3 'revise' e de review não repetido com evidência inalterada\. — Depois de S\-008: GROK\_HOME=$\(mktemp \-d\) RUST\_MIN\_STACK=16777216 cargo test \-p distill\-shell \-\-lib \-\- \-\-test\-threads=1 jev\_routing::tests reasoning\_gates::tests

### 9. Goal pausado: tudo para até o usuário
Decisão D4\. O macos\-app seguiu por cerca de 3 h com review 'revise' e 5 turnos de wake \(269M \+ 312M tokens\); o turno de wake do lacco gastou 72M \(E\-015, E\-019, E\-043, E\-044, E\-045\)\.

**Implementação**
- Pré\-requisito: concluir S\-004 antes deste passo\.
- Em turn\.rs \(≈1346\-1390\), guardar se o goal estava Active antes do round\-end; se ele pausou nessa avaliação, sair do loop antes de jev\_delivery\_review e das continuações do stop gate\.
- Em admit\_task\_completion\_wake \(run\_loop\.rs:137\), recusar a admissão com goal em qualquer status pausado \(push\_task\_wake\_fallback para pendentes\)\.
- Em maybe\_drain\_notifications \(notification\_drain\.rs:523\), retornar sem drenar enquanto pausado; as notificações ficam pendentes e são entregues no próximo turno do usuário ou resume \(consume\_deferred\_completions, :55\)\. O descarte em Active/Complete fica como está\.
- Aplicar a mesma regra de pausado à supressão de wake de workflow \(run\_loop\.rs:2116\)\.
- Testes: goal pausado \+ tarefa concluída não abre turno e a notificação fica retida; no próximo turno do usuário ela é entregue; auto\-pausa em fim de rodada não cria reviewer\.

**Caminhos relevantes**
- crates/codegen/distill\-shell/src/session/acp\_session\_impl/turn\.rs
- crates/codegen/distill\-shell/src/session/acp\_session\_impl/run\_loop\.rs
- crates/codegen/distill\-shell/src/session/acp\_session\_impl/notification\_drain\.rs

**Definição de pronto**
- Nenhum turno começa por wake com goal pausado\.
- A auto\-pausa encerra o turno sem review\.
- Notificações são entregues depois\.

**Como verificar**
- Testes de wake retido com goal pausado e de auto\-pausa sem reviewer\. — Depois de S\-009: GROK\_HOME=$\(mktemp \-d\) RUST\_MIN\_STACK=16777216 cargo test \-p distill\-shell \-\-lib \-\- \-\-test\-threads=1 notification\_drain prompt\_queue\_actor\_tests

### 10. Registrar cada avaliação do goal
Os veredictos brutos não ficam em lugar nenhum \(E\-014\), sem isso não dá para provar nem ajustar o escalonamento\.

**Implementação**
- Pré\-requisito: concluir S\-003, S\-004 antes deste passo\.
- Anexar uma linha JSON por avaliação em &lt;sessão&gt;/goal/evaluations\.jsonl \(helper de caminho ao lado de plan\_path/strategy\_path no goal\_tracker\) com ts, goal\_id, tipo \(checkpoint\|round\_end\), modelo, decisão, next\_step, blocker\_key/blocker\_kind, observações novas, delta de critérios, harness\_observed, no\_progress\_rounds e ação \(continue\|nudge\|escalate\|deescalate\|pause\_blocked\|pause\_infra\)\.
- Registrar também cada execução do reasoning\-executor/strategist do escalonamento \(início, fim, resultado resumido\)\.
- Adicionar ao histórico do goal os eventos de escalonamento e desescalonamento, sem mudar os enums de telemetria\.
- Teste: sequência roteirizada gera as linhas esperadas com todos os campos\.

**Caminhos relevantes**
- crates/codegen/distill\-shell/src/session/acp\_session\_impl/goal\.rs
- crates/codegen/distill\-shell/src/session/goal\_tracker\.rs

**Definição de pronto**
- evaluations\.jsonl tem uma linha por avaliação e por execução de escalonamento\.

**Como verificar**
- Teste de evaluations\.jsonl com sequência roteirizada\. — Depois de S\-010: GROK\_HOME=$\(mktemp \-d\) RUST\_MIN\_STACK=16777216 cargo test \-p distill\-shell \-\-lib \-\- \-\-test\-threads=1 goal\_compaction\_reseed\_tests

### 11. Verificação completa, build local e aceite ao vivo
Provar que as mudanças juntas eliminam as pausas prematuras sem regressões e repetir os 3 cenários reais\.

**Implementação**
- Pré\-requisito: concluir S\-001, S\-002, S\-003, S\-004, S\-005, S\-006, S\-007, S\-008, S\-009, S\-010 antes deste passo\.
- Rodar os filtros afetados com GROK\_HOME=$\(mktemp \-d\) RUST\_MIN\_STACK=16777216 cargo test \-p distill\-shell \-\-lib \-\- \-\-test\-threads=1 &lt;filtro&gt; para goal\_evaluator, goal\_tracker, goal\_classifier, goal\_compaction\_reseed\_tests, jev\_routing, reasoning\_gates, notification\_drain e slash\_commands\.
- Rodar a suíte lib completa do distill\-shell, cargo test \-p distill\-agent e cargo clippy no padrão do repo\.
- Com OK do usuário, gerar build local no padrão ~/\.local/share/distill/local\-builds/2\.0\.16\+local\.&lt;nome&gt;/ sem publicar release\.
- Com autorização, reexecutar 3 goals no formato das sessões \(lacco onboarding, macos\-app seletores, empath Todo\) e auditar evaluations\.jsonl: zero pausas no\_progress, pares escalate→deescalate, empath pausando na 1ª confirmação do gate com a opção DEV\-3275 e nenhum turno de wake com goal pausado\.

**Caminhos relevantes**
- crates/codegen/distill\-shell/src/session
- crates/codegen/distill\-agent/src/config\.rs
- goal/GATES\.md

**Definição de pronto**
- Todas as suítes e o clippy passam\.
- O aceite ao vivo cumpre as 4 condições\.

**Como verificar**
- Suíte lib completa do distill\-shell, distill\-agent e clippy\. — Depois de S\-001\.\.S\-010: GROK\_HOME=$\(mktemp \-d\) RUST\_MIN\_STACK=16777216 cargo test \-p distill\-shell \-\-lib \-\- \-\-test\-threads=1; cargo test \-p distill\-agent; cargo clippy
- Aceite ao vivo com 3 goals no formato das sessões, auditando evaluations\.jsonl\. — Depois do build local e com autorização do usuário: rodar os 3 goals e checar zero pausas no\_progress, escalate→deescalate, pausa cedo com opção no empath e nenhum wake com goal pausado\.

## Riscos
- high / MITIGATED: Nunca pausar por falta de progresso pode queimar tokens num goal realmente travado que o avaliador não classifica como blocked\. Mitigation: Escalonamento com executor limitado \(5 turns por execução\), strategist a cada 3 execuções sem avanço, pausa cedo para requires\_user, token budget opcional \(/goal \-\-budget\) e visibilidade em evaluations\.jsonl\. É o custo aceito ao optar por D3\.
- medium / MITIGATED: A pausa cedo em requires\_user pode criar pausas falsas se o avaliador classificar mal um erro comum\. Mitigation: requires\_user exige evidência concreta \(label, estado da dependência, pedido de decisão\); o padrão é transient com streak 3; há testes com veredictos roteirizados e aceite ao vivo\.
- medium / ACCEPTED: Com D1, um goal pode sobrepor regras protetivas do repositório \(ex\.: o RemoteCode em execução é off\-limits\) e afetar um checkout em uso\. Mitigation: Decisão explícita do dono \(D1\); a sobreposição é declarada no relatório do worker e registrada nas Risks do plano do goal\.
- medium / MITIGATED: O escalonamento aumenta o custo com chamadas ao reasoning model\. Mitigation: Executor com contexto focado, 5 turns por execução, desescalonamento no primeiro progresso e sinais determinísticos que reduzem falsos 'sem progresso'\.
- medium / MITIGATED: O campo obrigatório novo \(blocker\_kind\) no schema do avaliador pode aumentar falhas de parse e pausas por infraestrutura\. Mitigation: Mesma validação já usada para blocker\_key, json\_schema estrito enviado ao provedor, 2 tentativas por avaliação e testes com respostas roteirizadas\.
- low / MITIGATED: Reconhecer mais comandos como checks pode gerar falsos positivos \(ex\.: servidores de dev\)\. Mitigation: Exigir subcomando de check \(test/build/run\) e casos negativos explícitos nos testes\.
- medium / MITIGATED: Contar mudanças no workspace como progresso deixa sem escalonamento um churn de edições que nunca converge\. Mitigation: O painel do verificador, o cap de 10 rejeições e o token budget continuam limitando; registrado como residual\.
- low / MITIGATED: Reter wakes com goal pausado atrasa informação para o usuário\. Mitigation: As notificações ficam pendentes e são entregues no próximo turno do usuário ou no resume, e as tarefas continuam listadas na UI\.
- low / MITIGATED: Campos novos no estado persistido do goal podem quebrar sessões antigas\. Mitigation: serde\(default\) nos campos novos \(GoalProgress não usa deny\_unknown\_fields\) e teste carregando um state\.json da 2\.0\.16\.

## Pendências e condições para interromper
- O cap de 10 rejeições do verificador \(BackOff\) continua pausando; falta decidir se também vira escalonamento\.
- O contador de tokens do goal mostra o tamanho do contexto, não o gasto acumulado \(empath: 391k exibidos contra 23,24M reais\), e continua subindo com o goal pausado\.
- O effort padrão 'max' do config do usuário e a política de effort do Jev \(só 27 de 120 chamadas reduzidas no empath\) afetam custo, não pausas\.
- Ao substituir um goal, o scratch do goal anterior é apagado \(o lacco perdeu drive\-rerun3\.log\)\.
- O nudge 'Next step', tirado do primeiro checkbox do plano, pode redirecionar para trabalho já feito \(empath, rodada 2\)\.
- Churn de edições sem critérios verificados não escala nem pausa \(efeito de D3\); para goals sem supervisão, usar \-\-budget\.

## Verificações de aceitação
- Nenhum caminho automático pausa um goal por falta de progresso \(checkpoint, fim de rodada ou stall do verificador\)\. — Testes de ator: sem pausa NoProgress, spawn do reasoning\-executor com cwd na raiz, desescalonamento com progresso e stall do verificador escalando\.; Aceite ao vivo com 3 goals no formato das sessões, auditando evaluations\.jsonl\.
- Com 2 avaliações seguidas sem progresso, um reasoning\-executor roda no workspace de entrega e se repete até surgir progresso, quando o main model volta a conduzir sozinho\. — Testes de ator: sem pausa NoProgress, spawn do reasoning\-executor com cwd na raiz, desescalonamento com progresso e stall do verificador escalando\.; Aceite ao vivo com 3 goals no formato das sessões, auditando evaluations\.jsonl\.
- Progresso inclui sinais determinísticos: fingerprint novo do workspace, resultado novo de check \(inclusive Playwright, xcodebuild, npm run e2e e tarefas em background\) ou evidência nova do avaliador\. — Testes de detecção de checks e do caminho TaskOutput\.; Testes de sinais de progresso e de compatibilidade de estado\.
- Um bloqueio requires\_user pausa na primeira avaliação que o confirma, com opções, e '/goal resume &lt;instrução&gt;' retoma o mesmo goal com a instrução\. — Testes: requires\_user pausa em checkpoint com opções; transient mantém o streak; '/goal resume &lt;texto&gt;' retoma com guidance\.; Aceite ao vivo com 3 goals no formato das sessões, auditando evaluations\.jsonl\.
- Instruções explícitas do objetivo do goal prevalecem sobre instruções do repositório em planner, worker, avaliador, verificador e reviewers\. — Asserts de conteúdo da regra de precedência nos 7 textos\.; Aceite ao vivo com 3 goals no formato das sessões, auditando evaluations\.jsonl\.
- Planos, conselhos, hints e reviews do Jev usam o objetivo do goal como pedido atual\. — Testes do request\_anchor com kickoff de goal e guidance\.
- O review de entrega analisa a raiz e o baseline reais da entrega, incluindo commits, e roda nessa raiz\. — Testes com repo temporário: worktree ≠ cwd, commit depois do baseline e cwd do spawn\.
- Reviews de entrega têm no máximo 3 veredictos 'revise' por turno e não repetem review com evidência inalterada\. — Testes do teto de 3 'revise' e de review não repetido com evidência inalterada\.
- Com o goal pausado automaticamente, o turno termina e nenhum wake de background inicia trabalho até o usuário agir\. — Testes de wake retido com goal pausado e de auto\-pausa sem reviewer\.; Aceite ao vivo com 3 goals no formato das sessões, auditando evaluations\.jsonl\.
- Cada avaliação do goal e cada execução de escalonamento fica registrada em goal/evaluations\.jsonl\. — Teste de evaluations\.jsonl com sequência roteirizada\.
- As suítes afetadas, a suíte lib do distill\-shell, os testes do distill\-agent e o clippy passam\. — Suíte lib completa do distill\-shell, distill\-agent e clippy\.
