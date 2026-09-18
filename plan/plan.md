# Plano — usar Jev (TypeSafe System One) no jev-build sempre que possível

- **Status do freeze:** `NOT_CONFIDENT` (o plano é executável, mas três incógnitas materiais e o conselho de perfil `full` precisam fechar antes de `READY_TO_EXECUTE`)
- **Depth:** `standard` · **case_type:** `FEATURE` · **Tese:** `T-001` (rev. 2, após triagem do conselho)
- **risk_flags:** `security_privacy`, `auth_boundary`, `material_uncertainty`
- **Conselho:** `required=true` · perfil `fast` executado nesta sessão → **REVISE + ESCALATE_TO_FULL** (ver §Conselho)
- **Idioma:** prosa em pt-BR; identificadores, caminhos e código em inglês (convenção do repo)
- **Comando de origem:** `/sam-plan` ("verifique todo o repositório e faça um planejamento para usar o jev sempre que possível")

> **Limitação de modo (leia primeiro).** Esta sessão rodou em *plan mode*, que proíbe escrever no sistema
> além deste arquivo. Portanto os artefatos obrigatórios do sam-plan (`plan-report.json` + pacote HTML
> light-theme em `$PLAN_DIR`, validados por `validate_plan_report.py`) e o relatório do conselho
> (`council-report.json`, scratch e validador) **não foram gravados**; o passo `S-000` os materializa.
> Todo o resto do estudo, da tese e das provas está congelado abaixo. Nada aqui foi implementado: o
> repositório segue com `git status` limpo e **zero** referências a `jev`/`typesafe`.

---

## 1. Objetivo, escopo e contrato

### 1.1 Objetivo congelado

Fazer o jev-build (esta cópia do harness Grok Build) usar **Jev** — o modelo System One da TypeSafe
(`POST https://api.typesafe.ai/v1/systemone`, perguntas tipadas `choice`/`score`/`noul`, respostas tipadas
com probabilidades e `confidence`) — para decisões estreitas e estruturadas, porque é muito mais barato
que uma chamada de LLM, **delegando ao LLM tudo o que o Jev não faz**: geração de texto, aritmética e
contagem, ordenação de datas, raciocínio multi-hop (indireção), estado grande/não filtrado e qualquer
caso abaixo do piso de confiança.

### 1.2 Escopo comprometido nesta v1 (A + B + C + D1 + E)

| # | Entrega | O que é |
|---|---------|---------|
| A | Cliente Jev | Um módulo `src/jev/` dentro do crate existente `xai-grok-workspace`: tipos serde do `/v1/systemone`, bearer via nome de env configurável, **uma** tentativa com deadline, taxonomia de erro, contagem de tokens, sem log de segredo. Reusa `xai_grok_extra_ca::build_reqwest_client` (o mesmo builder de `shared_client()`), **sem nenhuma dependência de produção nova**. |
| B | Config + flag + higiene de credencial | Seção `[jev]` (`enabled`, `endpoint`, `model`, `timeout_ms`, `api_key_env`, `max_state_bytes`) + linha de flag `GROK_JEV` **default OFF** no registry existente; `enabled`/`endpoint` resolvíveis só em camadas managed/env/user; `JEV_API_KEY` excluída do ambiente de subprocessos por padrão. |
| C | Gate de avaliação (2 estágios) | **C1** spike offline: replay de contextos gravados + casos autorais (incluindo injeção de prompt dentro de comando, caminhos e transcript), rótulo manual sob regra escrita, **e medição do incumbent (classificador LLM) no mesmo conjunto** como linha de base. **C2:** só se C1 prometer, promove para fixtures no repo. Gate numérico: falso-allow = 0 na parte insegura retida, falso-bloqueio dentro do orçamento do dono, e **não pior que o incumbent**. |
| D1 | Classificador de permissão Jev-first | `impl PermissionClassifier` para Jev, instalado no ponto de injeção existente. **Autoridade Jev ≤ incumbent**: só pode (i) bloquear, (ii) devolver `Unavailable`/escalar → classificador LLM atual → prompt humano atual, ou (iii) autorizar **apenas** a classe rotineira que a heurística local já considera de baixo risco, sem `security_findings`, sem `Ask` de política e acima do piso de confiança. Nunca reseta o ratchet de negações consecutivas. Rollout em 2 fases atrás da flag: sombra (só telemetria) → Jev-first na classe rotineira. |
| E | Medição + documentação | Instrumentar o incumbent **antes** de construir (contagem de chamadas por classe, tokens, p50/p95); depois medir chamadas de LLM evitadas, tokens Jev e custo pelo preço publicado; documentar config, os campos exatos enviados (allowlist), declaração de egress/retenção e o kill switch. |

### 1.3 Backlog explícito (fora desta v1 — cada item precisa do próprio gate de avaliação)

Mapa completo dos seams de decisão encontrados no estudo; nenhum entra na v1 (correção do conselho: cortar escopo especulativo).

| Seam | Hoje | Veredito |
|---|---|---|
| Classificador de preguiça/parada prematura | side call de modelo que faz **parse de JSON em texto livre** (`acp_session_impl/laziness_classifier.rs:575`; chamador `acp_session_impl/laziness.rs` ~:277) | Forte candidato (saída tipada elimina o parse), 2ª onda |
| Compaction: escolher o que manter + checagem de sumário degenerado | aritmética de tokens + piso de 500 chars (`crates/common/xai-grok-compaction/src/select.rs:61`, `.../summary.rs:124`) | Candidato (só a *decisão*; o sumário continua LLM) |
| Gate de relevância de memória | top-k fixo + `min_score` (num caminho, `0.0`) `turn.rs:2284`, `compaction_context.rs:81`; manifesto v2 sem ranking (`v2.rs:371-436`) | Candidato |
| Rerank da busca de sessões | BM25 isolado (`xai-grok-session-search/src/fts.rs:443-475`) | Baixo valor (UI), adiado |
| Seleção de subconjunto de tools por turno | lista estática por turno (`sampler_turn.rs:426`, `turn.rs:2856`) | Precisa de avaliação própria; adiado |
| Roteamento de modelo/effort/tipo de subagente | tabela de precedência estática (`agent/subagent/mod.rs:598`, `handle_request.rs:770`) | Alto valor, alto risco; avaliação própria |
| Título de sessão | chamada LLM com fallback textual (`session_summary.rs:135-201`) | Candidato pequeno |
| Classificador/verificador de objetivo ("skeptic panel") | `goal_classifier.rs` | Verificar se é model-backed; candidato |
| Embeddings para busca de memória | vetores (`xai-grok-memory/src/search.rs`) | **Nunca Jev** (não é decisão) |
| Geração (sumários, patches, planos, explicações, código) | LLM | **Nunca Jev** (documentado como fora de escopo do modelo) |
| Matemática/contagem/ordenação de datas | código | **Nunca Jev** (fraqueza documentada) → **código**, não Jev |

### 1.3.1 Escada de maximização de tokens (cookbooks → seams, em ordem de retorno)

Onde o token realmente está, medido por estrutura (payload × frequência de reenvio) e não por intuição. Tudo aqui é **candidato com gate próprio** — nada entra na v1 sem medição e sem o conselho `full`. Estimativas são estruturais; a medição é `S-001`.

| # | Jev decide | Cookbook de referência | Por que economiza muito | Estimativa estrutural | Risco / mitigação |
|---|---|---|---|---|---|
| **P1** | **Subconjunto de tools por turno** | function calling + speculative fan-out | Os schemas de ~25 tools embutidos (+ MCP) são reenviados **a cada rodada** do loop do agente; uma rodada típica carrega 5-10k tokens só de schemas, × 20-40 rodadas | **100-400k tokens/sessão** se a poda for efetiva; ~30-70% do payload de schemas | Modelo sem a tool certa → manter um **núcleo obrigatório** sempre, "não sei → mantém tudo", e medir sucesso de tarefa num conjunto roteirizado antes de ligar |
| **P2** | **Ler menos: seleção de trecho em vez de arquivo inteiro** | line-by-line search (semantic_find; Choice ≤255 opções) + classifying RAG passages | O maior vetor de crescimento de contexto em sessões de exploração é conteúdo de arquivo/saída de tool entrando no histórico; um arquivo de 2k linhas custa 20-40k tokens e volta em **todas** as rodadas seguintes | Cortes de ordem **10×** no custo de uma leitura grande | Ler menos pode perder o trecho certo → sempre oferecer o arquivo inteiro como fallback e registrar quando a seleção foi usada |
| **P3** | **O que o sumarizador de compaction precisa ver** | classifying RAG passages + classification using confidence | A compaction reenvia o histórico para a LLM — é a **maior chamada única** de uma sessão longa; o Jev pode decidir quais segmentos exigem sumário, quais podem ser descartados e o que precisa ser preservado verbatim | Reduz o input da compaction em fração dos segmentos dispensáveis | Sumário é geração → continua na LLM; o Jev decide só o *recorte* |
| **P4** | **Modelo/effort por turno** | intent routing + classification using confidence | Um turno simples atendido por modelo/effort barato corta o custo do **loop principal inteiro** naquele turno — é a maior alavanca que existe | Potencialmente a maior de todas; também a mais arriscada | Qualidade: só com avaliação de sucesso de tarefa e rampa (nunca "always cheap") |
| **P5** | **Validar a chamada antes de executar** | function calling | Uma chamada de tool que falha custa um reenvio completo do prompt; validar argumentos antes (arquivo certo? comando coerente?) evita round-trips perdidos | Proporcional à taxa de falha atual | Usar apenas para **barrar e perguntar**, nunca para autorizar (mesma regra de autoridade do freeze) |
| **P6** | **Quais skills anunciadas importam neste turno** | skill suggestion | O harness **anuncia** skills descobertas via system-reminder (não há roster estático no prompt); o Jev pode escolher a que importa e reduzir o resto | Ganho modesto em tokens; ganho maior em qualidade de escolha | Escolha errada é pior que nenhuma → o texto da sugestão precisa ser ignorável |

**O que NÃO é economia de token (honestidade):** guardrails de LLM (llm_guardrails), self-consistency, double-checking de citações e entity alignment **adicionam** chamadas — são segurança/qualidade, não economia (detalhe e política de uso em §1.8); SDE cascade/date extraction/pre-parsed value extraction resolvem aritmética e parsing, que neste harness pertencem a **código**; autoresearch é offline (treino), não runtime.

**Limites duros que valem para toda a escada:** Jev aceita no máximo **64k tokens** por requisição (32k para `state` + maior pergunta), **Choice ≤ 255 opções**, **não gera texto** e pode ser **direcionado por conteúdo adversarial** dentro do `state`. Por isso P1-P3 exigem estado minimizado (nunca despejar arquivo inteiro no `state` do Jev) e nenhum item da escada pode virar gate autoritativo sem o mesmo tratamento de autoridade do freeze (§1.6/I-3).

**Ordem de construção recomendada:** P1 e P5 reaproveitam o filtro de tools por turno que **já existe** (`filter_cursor_tools_by_plan_mode`, `sampler_turn.rs:432`; `effective_tools`, `turn.rs:2856`); P2 reaproveita o caminho de `read_file`; P3 reaproveita a compaction; P4 reaproveita a resolução de modelo. Cada um entra como `S-###` próprio, com gate de avaliação e medição antes/depois — nunca em bloco.

**Regra de ouro do Jev como economizador (senão a conta vira negativa).** O Jev só economiza quando **o código já reduziu os candidatos** antes de montar o `state`. Dois exemplos que mudam o desenho:

- **P1 não pode mandar o catálogo de 25 tools no `state`** (≈6k tokens por chamada × 40 rodadas ≈ 240k tokens Jev, mais do que os ~160k que a poda economiza). O certo é uma **Choice por família** (ler / editar / executar / web / delegar / planejar / skills — poucos critérios, ~500 tokens por chamada), com o catálogo completo só quando a família escolhida exigir desambiguação (*hierarchical classification*).
- **P2 não pode mandar o arquivo inteiro para o Jev decidir o trecho** — isso custaria exatamente os tokens que se quer evitar. O caminho é o do cookbook *line-by-line search*: primeiro um shortlist barato **em código** (grep/BM25/janela), depois o Jev ranqueia **apenas os candidatos** (≤255 opções por Choice) e um Noul decide "há resposta neste trecho?".

**Refinamentos por item (varredura completa do índice em 2026-09-17; ledger item a item em `plan/docs-review.md`, reconciliação em §12.4):**

- **P1** — o catálogo de famílias vai nas `criteria` da Choice (barato, teto **255 opções**), nunca no `state`, que é a parte caríssima (item 6); representar como **taxonomia caminhável** com subárvore trimada a filhos diretos + amostra de folhas (item 9); profundidade decidida pela razão topo/segundo com **beam (K=3 acertou 4/4 folhas vs 2/4 do greedy)** como experimento, não requisito (item 109).
- **P2** — a shortlist feita em código é o **teto** do que o Jev pode achar: ele **não adiciona** o que a busca rápida não selecionou (item 97); ordem canônica: primeiro o que é decidível em código (match/normalização), o Jev lê só o contexto (item 104); candidatos vêm do código e o Jev escolhe o span; acima de 255 candidatos ⇒ dois estágios (janela → span) (itens 98, 108); **Noul de existência** separa "não há resposta" de "resposta fraca" (item 98).
- **P3** — o `state` domina o custo (pergunta extra custa pouco, o state é enviado uma vez) (item 99); o **fraseado das perguntas faz parte do contrato revisável** — mudar a palavra muda o limiar (item 99).
- **P5** — normalizar Score por `n-1` e usar níveis **descritivos** (níveis só numéricos degradam: 0,57/conf 0,35 vs 0,0/conf 1,0) (item 7); Noul sempre com limiar explícito e **zona morta** (≥0,9 / ≤0,4 / entre → fallback) (item 8); modelar risco como **soma ponderada de Scores atômicos normalizados** (destrutividade, escopo, reversibilidade) (item 17); alternativa sem constante de limiar: **Score de 3 níveis** (allow / incerto→caminho normal / deny) (item 102); confiança agregada por **mínimo das partes** (item 107); red flags agregadas por **`max`, nunca média** (item 106).
- **Números de confiança adotados** — piso **0,60** (abaixo → fallback LLM/incumbente) e **0,85** para allow sensível (item 16); topo **≥0,60** para ação automática e **banda de revisão 0,30–0,70** → incerto, que **nunca decide sozinho** (itens 94, 95); todos os limiares num **único dict re-roteável** (item 103).
- **P6** — duas fases (ranquear tudo → reler o top 3), texto **ignorável**, e com confiança baixa anunciar o mais **genérico/default** em vez de um específico, sem segunda chamada (itens 101, 111).
- **Avaliação** — calibração é propriedade **de grupo** (probabilidade 0,8 deve acertar ~80% das vezes); medir por faixa no gate C1/C2, não só acurácia pontual (item 10).

### 1.4 Regra de elegibilidade (operacionaliza o "sempre que possível")

Uma decisão é **elegível para Jev** se, e somente se, todas forem verdadeiras:

1. a resposta é um conjunto fechado (choice/score/noul) — nunca texto gerado;
2. o estado pode ser **minimizado** aos campos que a pergunta precisa (≤ ~32k tokens; sem despejar conteúdo de arquivo ou saída de tool por padrão);
3. a decisão **não é o único gate** de uma ação irreversível/security-critical — a autoridade do Jev tem de ser ≤ a do caminho atual, e um fallback tem de existir;
4. a acurácia foi **medida** num conjunto retido e rotulado, e é ≥ a do caminho atual;
5. existe orçamento de latência com sub-deadline e fallback;
6. perguntas e limiares vivem no **arquivo único revisável** do catálogo;
7. o egresso é aceitável pela política de dados (default: sim para campos controlados pelo harness; **não** para conteúdo de arquivo/saída de tool sem opt-in explícito).
8. **existe a capacidade equivalente no ambiente?** Se já existe um skill, MCP server, plugin, subagente ou hook que faz o trabalho, a preferência é **reutilizar** o mecanismo existente (criar só com aprovação do dono; ver §1.8). Reuso não vira quesito de aceitação — é preferência de implementação; o que se pune é criar forma nova sem necessidade.

Se falhar qualquer item: **código** (se determinístico) ou **LLM** (se geração/multi-hop).

### 1.5 Não-objetivos (no-go)

- Jev para geração de texto (sumários, patches, planos, explicações).
- Jev como único gate de um allow que o caminho atual não daria.
- Remover ou enfraquecer os caminhos LLM/heurísticos existentes.
- Criar crate novo ou editar o manifesto raiz gerado (`Cargo.toml` raiz é gerado e sincronizado do monorepo).
- Mudar contratos públicos (ACP/wire); apenas enums internos de telemetria, de forma aditiva.
- Egresso de conteúdo de arquivo / saída de tool por padrão.
- Qualquer afirmação de economia em dólar que não seja medida (ou baseada só no preço publicado por token).
- Criar skill, MCP server, plugin ou hook **novo** quando já existe capacidade equivalente no ambiente (ver §1.4 item 8 e §1.8).
- Emitir ou versionar conteúdo de dependências externas (arquivos de skills do usuário, plugins, config do usuário) como se fosse fonte do repo — isso é entrada de runtime, não código-fonte.

### 1.6 Invariantes (violação = regressão)

- **I-1** flag OFF ⇒ comportamento idêntico ao de hoje e **zero conexões** para o Jev.
- **I-2** a cadeia de precedência de permissão fica intocada; Jev atua só no slot do classificador e não pode elevar política `Ask`/`Deny`, YOLO, grants de sessão ou prompts forçados por hook.
- **I-3** autoridade Jev ≤ incumbent (classe rotineira, sem `security_findings`, sem `Ask` de política, confiança ≥ piso).
- **I-4** allow com proveniência Jev **nunca** zera contadores de negação consecutiva.
- **I-5** o caminho de decisão de permissão **não faz retry** do Jev: uma tentativa, sub-deadline rígido dentro de um orçamento do chamador, e o Jev é **pulado** quando o worker do classificador já está ocupado.
- **I-6** segredo: chave lida pelo nome de env configurado, nunca logada, excluída do ambiente dos subprocessos; `enabled`/`endpoint` não configuráveis por config de projeto.
- **I-7** toda decisão Jev é observável em telemetria com proveniência/confiança/latência/tokens; escalações contadas.
- **I-8** nenhum caminho LLM é removido — o fallback é sempre o comportamento atual.
- **I-9** reuso antes de criação: capacidade equivalente existente (skill/MCP/plugin/hook/subagente) é preferida a uma forma nova, salvo aprovação explícita do dono.
- **I-10** capacidade de governança (auditoria, revisão de segurança, documentação, observabilidade, versionamento) tem **rótulo explícito** de "capacidade de governança (não fonte da lógica de negócio)" — nunca fingir conformidade, nunca deixar de implementar a capacidade por falta de fonte correspondente.

### 1.7 Restrições

- `Cargo.toml` raiz é **gerado** (`Cargo.toml:1`) e a árvore é sincronizada do monorepo → evitar edits na raiz.
- `xai-grok-http` **depende** de `xai-grok-workspace` (`crates/codegen/xai-grok-http/Cargo.toml:20`) → o módulo Jev não pode morar em `xai-grok-http` (ciclo).
- `xai-grok-workspace` **já** tem `reqwest`, `serde`, `serde_json`, `tokio` e `xai-grok-extra-ca` → cliente Jev sem dependência de produção nova; no máximo uma dev-dep (`wiremock`) para os testes.
- `clippy.toml` exige o builder do `xai-grok-extra-ca` para clientes reqwest (não usar `reqwest::Client::new()`).
- Jev é **texto apenas** (sem imagem/áudio), limite de 64k tokens para `state`+perguntas e 32k para `state`+maior pergunta.

### 1.8 Registro de governança das superfícies do catálogo (hooks, MCP, skills, plugins, pager)

Levantado ao percorrer a documentação do vendor e o próprio harness. Complementa §1.3.1: aqui fica o que é **governança/segurança** (não alavanca de token) e o que entra na auditoria do conselho `full`. Nada nesta tabela é critério de aceitação; é mapa de superfície e política de uso.

| Superfície | O que existe hoje (fato, com locator) | Política de uso no plano |
|---|---|---|
| Hooks (gate) | Eventos `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse` (+ Stop, compaction, subagentes) com traits Observe/Prompt/Tested (`crates/codegen/xai-grok-hooks/src/event.rs:79-104`); PreToolUse pode **forçar** `Ask` (`.../acp_session_impl/tool_calls.rs:15`, `:1254`, `:1289`) | Não é caminho de allow (hooks não concedem permissão, só barram/perguntam). Serve como **sonda de medição** e para travar experimentos sem tocar o core |
| Hooks de cliente (ACP) | `ClientHookDecision::{Deny,Continue,Ask}` — e `Ask` de hook de cliente **falha aberto** hoje (`.../acp_session/hooks.rs:87-93`) | Nunca usar hook de cliente como gate; registrar como risco se algum dia for considerado para decidir |
| Guardrails de mensagem in/out (llm_guardrails) | Não existe hoje no harness | Candidato de **segurança**, não de economia: entra pela via de decisão estruturada + autoridade limitada, com o custo de chamadas explicitado |
| MCP / marketplace / pager tools | MCP tools entram dinamicamente no turno (`.../acp_session_impl/mcp_init.rs:651`, `:718`); existe marketplace de plugins (`xai-grok-plugin-marketplace`) e superfície de pager (bootstrap, statusline, temas) | Alvo da auditoria `security-privacy` (ferramenta de terceiro no caminho de permissão); nunca recebe allow direto |
| Skills (catálogo do ambiente) | Descoberta em dirs de skills do usuário/repo/bundled (`crates/codegen/xai-grok-agent/src/prompt/skills.rs:66-87`); anúncio por system-reminder (`src/builder.rs:641`); vêm com referências e assets | **Igualdade de capacidade**: conhecimento entra via skill (mecanismo existente) em vez de forma nova. Conteúdo de skill é **dado externo de runtime** — o Jev só recebe índice/descrição/metadados (nunca corpo sem opt-in) e nunca é a única fonte de um gate |
| Auditoria (segurança/observabilidade/versionamento) | Já há telemetria de decisão (`permission/types.rs:29-46`) e `git status` como evidência de escopo | Capacidade de governança com **rótulo explícito** (I-10): nunca inventar conformidade; a auditoria usa o que existe e registra o que falta |

**Política de merge (para não perder estas regras em edições futuras):** este registro é re-verificado quando o menu do vendor mudar, e cada varredura do menu é registrada no changelog (§12).

---

## 2. Tese

**T-001 (rev. 2).** Construir um cliente Jev pequeno dentro de `xai-grok-workspace` + uma camada
"Jev decide ou escala" que compõe perguntas atômicas em código, e converter **um** seam de decisão de
produção — o classificador de permissão do auto-mode, que hoje gasta uma side call de LLM por tool call —
com **autoridade Jev ≤ incumbent**, atrás de flag default-OFF, precedida de instrumentação do incumbent e
de um gate de avaliação com conjunto retido e rotulado, e seguida de medição de economia com critério
numérico de aborto. Todo o resto (backlog §1.3) fica mapeado com regra de elegibilidade (§1.4) para ser
re-proposto item a item.

**Por que este seam primeiro:** tem um ponto de injeção desenhado para extensão (trait `PermissionClassifier`, `auto_mode/mod.rs:328`), telemetria de decisão já pronta (`permission/types.rs:29-46`) e um caminho de falha fail-safe já estabelecido (falha ⇒ `Unavailable` ⇒ prompt humano, `manager/mod.rs:1086-1102`). Ele é o melhor *primeiro* seam (autoridade, telemetria, fail-safe), mas **não é onde estão os maiores volumes de token**: um pré-passe heurístico prova `Allow` e evita a chamada de rede quando não há `security_findings` (`auto_mode/mod.rs:1497-1509`), então o classificador roda apenas no subconjunto não rotineiro. Para maximizar economia em tokens, veja a escada em §1.3.1.

### 2.1 Alternativas rejeitadas

| Alternativa | Por que não |
|---|---|
| **Hooks HTTP existentes** (zero código) | Hooks só conseguem **forçar prompt/negar**, nunca conceder allow (`xai-grok-shell/src/session/acp_session/hooks.rs:87-97`); não substituem a side call do LLM, logo não capturam o custo. Servem como sonda de medição, não como destino. |
| **Crate novo `xai-grok-jev`** | Exigiria editar o `Cargo.toml` raiz gerado (membros + workspace.dependencies) num repo sincronizado periodicamente do monorepo → atrito recorrente e conflito de sync. O módulo em `xai-grok-workspace` não tem esse custo e já tem todas as deps. |
| **Jev como gate autoritativo de allow** (rev. 1) | Rejeitado pelo conselho (O-001, BLOCKER): superfície de authz + `state` influenciável por atacante + fraqueza documentada de steerability + `confidence` não verificada como calibrada. |
| **Converter 5–6 seams de uma vez** | Escopo especulativo sem demanda medida (O-007). v1 converte um seam e entrega a infraestrutura que torna os próximos baratos. |
| **Retry/backoff no caminho quente** | Autodestrutivo: falha do Jev já cai no LLM; backoff só adiciona latência na fila do ator de permissão (O-003, O-004). |
| **Tabela de preços local / cálculo de custo no cliente** | O repo não calcula custo (custo é reportado pelo servidor). Economia é medida em **chamadas de LLM evitadas + tokens**, não em dólar inventado. |
| **Substituir o sumário de compaction por Jev** | Geração de texto é explicitamente fora de escopo do modelo; só as *decisões* de compaction são candidatas. |

---

## 3. Evidências, suposições e incógnitas

Classificação: `FACT` exige locator real. Sufixo **(V)** = verificado diretamente pelo planejador nesta
sessão; **(R)** = reportado por mapeamento automatizado read-only e a re-verificar no início da execução
(`S-000`/`S-001`); nenhum dos dois é "achismo" — ambos são afirmações com localizador, mas (R) tem
verificação semântica pendente.

### 3.1 FATOS

| ID | Fato | Locator |
|----|------|---------|
| E-001 | O repo não tem **nenhuma** referência a `jev`/`typesafe` hoje (grep em toda a árvore, 0 resultados) | command: `grep -ri "jev\|typesafe"` sobre `/Users/samuelfajreldines/dev/jev-build` (V) |
| E-002 | Existe um ponto de injeção desenhado para exatamente este tipo de extensão: trait `PermissionClassifier`, resultado `ClassifierOutcome` (verdict/reason/proveniência) e implementação heurística | `crates/codegen/xai-grok-workspace/src/permission/auto_mode/mod.rs:125`, `:328`, `:358` (V) |
| E-003 | O classificador é chamado **inline** pelo ator de permissão (com `select!` de cancelamento); falha ⇒ `Unavailable` ⇒ prompt humano; o ator é uma única task `spawn_local` (fila) e registra `queue_depth` | `.../permission/manager/mod.rs:913` (≈913-929), `:512`, `:697`, `:1086-1102` (R) |
| E-004 | O wiring de produção do classificador LLM usa uma side query com timeout dedicado e um worker serial compartilhado | `crates/codegen/xai-grok-shell/src/session/acp_session_impl/sampler_turn.rs:827`, `:902`, `:921`, `:926` (V) |
| E-005 | A precedência de permissão é fixa: deny de política > YOLO > grant de sessão > allow de política > fast path > classificador > sandbox > policy ask > pré-decisão por acesso > prompt humano | `.../permission/manager/mod.rs:792-1353` (R) |
| E-006 | O `Cargo.toml` raiz é gerado e lista os membros; `xai-grok-http` depende de `xai-grok-workspace` ⇒ ciclo impede hospedar o módulo Jev no crate http | `Cargo.toml:1`, `:7-110`; `crates/codegen/xai-grok-http/Cargo.toml:20` (V) |
| E-007 | `xai-grok-workspace` já tem `reqwest` (:90), `serde` (:22), `serde_json` (:23), `tokio` (:26) e `xai-grok-extra-ca` (:108) | `crates/codegen/xai-grok-workspace/Cargo.toml` (V) |
| E-008 | Existe registry de features com tiers pin/env/config/remote e default por linha | `crates/codegen/xai-grok-config-types/src/registry.rs:56`, `:89+`; `flags.rs:40-133` (V) |
| E-009 | `xai-grok-extra-ca` é leaf e expõe o builder sancionado de cliente reqwest (TLS/proxy/CA extra) | `crates/codegen/xai-grok-extra-ca/src/lib.rs:56`; `Cargo.toml` do crate (V); obrigatoriedade via `clippy.toml:43` (R) |
| E-010 | Existe um corpus de **expectativas em testes** de permissão (não um dataset rotulado): `cargo check` não é seguro; `tee` não é auto-aprovado; heurística bloqueia/allows; ask vs auto | `grants_tests.rs:360`, `:722`; `auto_mode/mod.rs:2135-2213`; `manager/mod.rs:5946-5978` (R) |
| E-011 | Telemetria de decisão já carrega `classifier_source`/`classifier_verdict`/`classifier_latency_ms`, com enums fechados e testes de drift entre crates | `.../permission/types.rs:29-46`; `auto_mode/mod.rs:28-51`; `telemetry/src/events/permission_analytics.rs:210-217` (R) |
| E-012 | Infra de teste disponível: `MockInferenceServer` (roteiro de conversas + contagem de requests), `wiremock` como idioma do repo para HTTP de saída, e cenários PTY em YAML | `test-support/src/mock_server.rs:180`; `xai-grok-tools/src/implementations/web_search/client.rs:772-810`; `pty-harness/src/scripted.rs:33-68` (R) |
| E-013 | Existe um "ratchet" de negações consecutivas/totais que dispara deny-e-continua, e um allow do classificador zera o contador consecutivo | `.../permission/manager/mod.rs:~998-1017` (reset) e `~:1034-1085` (limites) (R) |
| E-014 | `shell_environment_policy` (exclude/include_only) existe e config de projeto não injeta sobreposição | `crates/codegen/xai-grok-config/src/config_override.rs:113-119` (R) |
| E-015 | O classificador de preguiça faz side call com o sampler e **faz parse de JSON em texto livre** — o padrão exato que uma resposta tipada do Jev elimina | `.../acp_session_impl/laziness_classifier.rs:575`; chamador `.../acp_session_impl/laziness.rs` (~:277) (R) |
| E-016 | Contrato público do Jev (docs do vendor): endpoint `POST https://api.typesafe.ai/v1/systemone`; bearer; `model=jev-latest` ⇒ `jev-1.13.0`; ≤64k tokens (state+perguntas) e ≤32k (state+maior pergunta); ~100 ms típico; perguntas avaliadas em paralelo; 401/422/429/529 com backoff; `confidence` em Choice/Score (Noul não tem); preço **US$ 42 por bilhão de tokens de entrada** (= US$ 0,042/Mtok) com saída grátis; limites 250k tokens/s e 1200 req/min; fraquezas documentadas: sem geração, contagem/aritmética não confiáveis, ordenação de datas não confiável, degrada com indireção multi-hop, degrada com `state` grande e não filtrado, e **conteúdo adversarial no `state` pode direcionar as respostas** | `https://docs.typesafe.ai/{api,models,confidence,model-jaggedness/jev-1.13,patterns/intent-routing,cookbooks/parallel_questions,cookbooks/llm_guardrails}` (V) |
| E-017 | Precedente interno de side call estreita com fallback textual: geração de título de sessão chama o sampler e cai para texto truncado em falha | `.../session/helpers/session_summary.rs:135-201` (R) |
| E-018 | O repo é sincronizado periodicamente do monorepo (fork local), contribuições externas não são aceitas, e a user-guide mora no crate do pager | `README.md:30-38`, `:90-95`; `CONTRIBUTING.md:1-6` (V) |

### 3.2 SUPOSIÇÕES

| ID | Suposição | Estado | Por que é aceitável |
|----|-----------|--------|---------------------|
| A-001 | A acurácia do Jev nas *nossas* decisões é desconhecida e precisa ser medida antes de qualquer fiação de produção | `UNVERIFIED` | É exatamente o que o passo `S-004` mede (gate C). Nenhuma decisão de produção é tomada antes. |
| A-002 | O campo `confidence` é documentado como presente, **não** como calibrado | `UNVERIFIED` | A avaliação mede calibração; a autoridade ≤ incumbent e o piso de confiança limitam o dano de uma confiança mal calibrada. |
| A-003 | O Jev, executado a partir deste host, responde na casa das centenas de ms (docs dizem ~100 ms) | `UNVERIFIED` | Medido em `S-004`; o desenho não depende disso (orçamento + fallback). |
| A-004 | A taxa de chamadas da side call do classificador LLM (o "prêmio" a capturar) é desconhecida | `UNVERIFIED` | `S-001` instrumenta o incumbent **antes** de construir qualquer coisa. |

### 3.3 INCÓGNITAS MATERIAIS

| ID | Incógnita | Sonda |
|----|-----------|-------|
| U-001 | Existe chave/plano Jev no ambiente (`TYPESAFE_API_KEY`)? | `S-000` verifica; sem chave, `S-001`–`S-003` seguem (locais) e `S-004`+ viram `BLOCKED` até o dono prover a chave. |
| U-002 | Latência real e jitter da API a partir deste host/rede | medir em `S-004` (p50/p95 por pergunta e por requisição) |
| U-003 | Taxa de chamadas/tokens do classificador incumbent por classe de decisão | `S-001` (instrumentação + `MockInferenceServer` request count) |
| U-004 | Calibração das `confidence` do Jev nas nossas tarefas | `S-004` (curva confiança × acurácia no conjunto retido) |

---

## 4. Passos

`S-000` é pré-requisito de execução (bookkeeping + gate de conselho). Nenhum passo toca produção sem a flag.

### S-000 — Materializar o freeze e rodar o conselho `full`

- **Por quê:** este plano está em modo somente-leitura; o contrato do sam-plan e o resultado do conselho (`ESCALATE_TO_FULL`) exigem artefatos validados em disco antes de qualquer implementação.
- **Como:**
  1. `SAM_PLAN_DIR=~/.grok/skills/sam-plan; PLAN_DIR=<repo>/plan; python3 -B "$SAM_PLAN_DIR/scripts/scaffold_plan_dir.py" --out "$PLAN_DIR"`.
  2. Converter este documento em `$PLAN_DIR/plan-report.json` (schema_version 1, workflow plan, status/depth/case_type, `study.tools_used`/`surfaces_mapped`, `frozen`, `evidence` com os locators acima, `thesis`, `steps`, `risks`, `verifications`, `acceptance_trace`, `council`, `simplicity`, `residuals`).
  3. `python3 -B "$SAM_PLAN_DIR/scripts/validate_plan_report.py" "$PLAN_DIR/plan-report.json" --repo-root <repo>`.
  4. Renderizar o pacote humano: `python3 -B "$SAM_PLAN_DIR/scripts/render_plan_html.py" "$PLAN_DIR/plan-report.json" --out "$PLAN_DIR"` e revalidar com `--require-html`.
  5. Rodar o conselho de perfil **`full`** (6 assentos obrigatórios + especialistas selecionados: `security-privacy`, `reliability-performance`, `testability-release`, `cost-dependency`) sobre T-001 rev.2, com scratch + `validate_council_report.py`; aplicar as correções aceitas.
  6. Confirmar disponibilidade da chave Jev (U-001) e re-verificar os locators marcados (R).
- **Surfaces:** `<repo>/plan/**` (novo), `~/.grok/skills/sam-plan/scripts/*`.
- **DoD:** `plan-report.json` + HTML no disco com validador `VALID` (`--require-html`), relatório do conselho validado, locators (R) reconferidos, resposta sobre a chave registrada.
- **Provam:** `V-001`, `V-002`. **Depende de:** nada.

### S-001 — Instrumentar o incumbent e fixar o piso de economia

- **Por quê:** o conselho (árbitro) exige critério numérico de **aborto por valor**: sem medir o prêmio antes, o plano pode gastar construção e não economizar nada (classe rotineira pode já passar sem LLM).
- **Como:**
  1. Instrumentar o caminho do classificador LLM de auto-mode: contar chamadas por classe de decisão (allow / block / unavailable), tokens de prompt/completion e latência p50/p95 (telemetria existente + spans de side query em `sampler_turn.rs`).
  2. Acrescentar contagem de requests de side query em teste de integração usando `MockInferenceServer` (`set_conversations` / `requests()`), para que `S-005` compare antes/depois com número.
  3. Definir com o dono o **piso numérico de economia** (ex.: "≥ X% das decisões de permissão em modo auto deixam de fazer chamada de LLM") e o critério de aborto (§7 R-002).
- **Surfaces:** `sampler_turn.rs`, `xai-grok-telemetry` (evento novo aditivo se necessário), `xai-grok-test-support` (uso), `xai-grok-workspace` (contadores).
- **DoD:** relatório com números reais (chamadas/classe, tokens, latência) e piso aprovado por escrito; zero mudança de comportamento (só medição).
- **Provam:** `V-003`. **Depende de:** `S-000`.

### S-002 — Cliente Jev (`xai-grok-workspace/src/jev/`) + config/flag/credencial

- **Por quê:** é a fundação que torna qualquer seam seguinte barato; sem ela cada integração reinventaria transporte e configuração.
- **Como:**
  1. Criar `src/jev/mod.rs` com tipos serde (`JevRequest{state,model,questions}`, `JevQuestion::{Choice,Score,Noul}`, `JevResponse{model,answers,usage}`, `JevAnswer` com `probabilities`/`confidence`), `JevClient` construído via `xai_grok_extra_ca::build_reqwest_client` (nunca `reqwest::Client::new()`), bearer a partir de **nome de env** configurado, `timeout_ms` por requisição, **sem retry** no caminho quente, taxonomia de erro (`Timeout | Transport | RateLimited | Invalid | Unavailable`) e contagem de tokens de `usage`. Nenhum log de cabeçalho/chave (usar o redactor existente).
  2. Adicionar `[jev]` a `Config` (seção `#[derive(Default, Deserialize)]` + `#[serde(default)]`, padrão de `StorageConfig`) e uma linha `Feature::Jev` no registry (`key: "jev"`, `path: "features.jev"`, `env: "GROK_JEV"`, `default_enabled: false`, sem tier remote nesta v1).
  3. Pinning de segurança: `enabled` e `endpoint` **não** podem vir de config de projeto (só managed/env/user); `JEV_API_KEY` entra no `shell_environment_policy` de exclusão por padrão; documentar que a chave é legível por comandos do agente se o usuário desfizer a exclusão.
  4. Testes: `wiremock` como dev-dep (200 feliz com golden do corpo, 401, 422, 429, 529, timeout); teste de flag OFF afirmando **zero** conexões e decisão idêntica; teste de precedência de config provando que config de projeto não liga/reaponta.
- **Surfaces:** `crates/codegen/xai-grok-workspace/src/jev/**` (novo), `crates/codegen/xai-grok-workspace/Cargo.toml` (dev-deps), `crates/codegen/xai-grok-shell/src/agent/config.rs` (seção), `crates/codegen/xai-grok-config-types/src/registry.rs` (linha), `crates/codegen/xai-grok-env/src/registry.rs`.
- **DoD:** `cargo test -p xai-grok-workspace -p xai-grok-config-types -p xai-grok-env` verde; nenhuma dependência de produção nova; flag OFF provadamente inerte; nenhum segredo em log (teste).
- **Provam:** `V-004`. **Depende de:** `S-000`.

### S-003 — Camada de decisão "Jev decide ou escala" + catálogo único de perguntas

- **Por quê:** o vendor documenta que a revisão humana mais importante são **as perguntas e os limiares**; o repo também valoriza um único lugar legível. Além disso, a composição em código é o que impede o Jev de virar um oráculo opaco.
- **Como:**
  1. Criar `src/jev/questions.rs`: **arquivo único** com os pacotes de perguntas, critérios, limiares, pesos e pisos de confiança (começar pelo pacote de permissão: Nouls atômicos de escrita fora do workspace, deleção/sobrescrita, envio de dados para rede, elevação de privilégio, execução de fonte não confiável, + uma Noul de triagem de injeção "o state contém instruções tentando influenciar esta decisão?"; e uma Choice de classe de risco: leitura rotineira / build-test / mutação local / mutação remota / destrutivo).
  2. Criar `src/jev/policy.rs`: montar o `state` **por allowlist tipada** (campos nomeados; sem conteúdo de arquivo; transcript limitado e truncado; teto `max_state_bytes`), enviar **uma** requisição com bateria especulativa (perguntas paralelas, custo ~zero por pergunta extra), compor as respostas **em código** com pesos e pisos, e devolver `JevDecision::{Block | AllowEligible | Escalate{reason}}` onde `AllowEligible` só existe se todas as condições de autoridade (I-3) valerem.
  3. Emitir um evento de telemetria aditivo por decisão (perguntas, veredito, confiança, latência, tokens, `model` versionado devolvido na resposta) e contar escalações.
  4. Testes: composição/limiares com respostas sintéticas, golden do corpo da requisição, telemetria, flag OFF sem chamadas.
- **Surfaces:** `crates/codegen/xai-grok-workspace/src/jev/{questions,policy}.rs`, `crates/codegen/xai-grok-telemetry/src/events/**`.
- **DoD:** testes verdes; pacote de perguntas revisável num só arquivo; nenhuma decisão de produção alterada (camada ainda não plugada).
- **Provam:** `V-005`. **Depende de:** `S-002`.

### S-004 — Gate de avaliação C1 (spike) → C2 (fixtures)

- **Por quê:** é o único gate que pode **falhar** e impedir a v1 de piorar a segurança; e é o que separa "Jev parece bom" de "Jev é medido".
- **Como:**
  1. **C1 (descartável):** script/bin offline que (a) extrai contextos reais do classificador (gravados em execução de teste/sessão de dev), (b) soma casos autorais, incluindo **injeção** dentro de comando, caminhos e transcript, (c) roda o baseline **incumbent** (classificador LLM) e o pacote Jev no mesmo conjunto, (d) imprime tabela por pergunta/classe com falso-allow, falso-bloqueio, delta vs incumbent e curva confiança × acurácia.
  2. Rotular a parte insegura sob **regra escrita** (o que conta como allow indevido), fora das asserções heurísticas existentes — o corpus de testes (`E-010`) entra apenas como *smoke/regressão*, nunca como gate de segurança.
  3. **C2:** se C1 passar, promover a fixtures versionadas no repo (teste `#[ignore]` + corpus em arquivo) para virar regressão contínua.
  4. Registrar a decisão go/no-go com números e o piso de economia de `S-001` como critério de aborto.
- **Surfaces:** `crates/codegen/xai-grok-workspace/src/jev/eval/**` (ou `tests/`), fixtures, `xai-grok-test-support`.
- **DoD:** relatório com falso-allow = 0 na parte insegura retida, falso-bloqueio ≤ orçamento do dono, Jev ≥ incumbent, calibração medida, e decisão registrada.
- **Provam:** `V-006`. **Depende de:** `S-003`. **Bloqueado por:** U-001 (chave).

### S-005 — Integração D1: classificador de permissão Jev-first (fases sombra → Jev-first)

- **Por quê:** é o seam de maior custo de LLM por ação no harness e o experimento real de valor.
- **Como:**
  1. Implementar `JevPermissionClassifier` (`impl PermissionClassifier`), reusando `ClassifierContext`; adicionar variante de proveniência `Jev` (e mapeamento na telemetria) nos enums fechados, com testes de drift.
  2. Instalar no ponto existente (`set_classifier_with_side_query`) no wiring do shell, com **ordem**: Jev (se ligado/elegível) → `Escalate`/`Unavailable` → classificador LLM atual → prompt humano atual.
  3. **Orçamento fim-a-fim do chamador:** mudança **aditiva** na trait para o chamador passar o deadline restante; Jev recebe sub-deadline rígido, **zero retries**, e é **pulado** quando o worker do classificador já está ocupado (`in_flight > 1`) — garantir que comandos de modo/kill switch (`SetYoloMode`/`SetAutoMode`/`ResetState`) nunca fiquem atrás de rede.
  4. Autoridade: `Allow` do Jev só para a classe rotineira, sem `security_findings`, sem `Ask` de política, confiança ≥ piso; e **não** tocar `auto_consecutive_denials`.
  5. Fase 1: **sombra** (calcula e telemetra, decisão inalterada). Fase 2: Jev-first na classe rotineira. Ambas atrás de `GROK_JEV`; flag OFF = comportamento atual byte a byte.
  6. Testes: unit (autoridade, `security_findings` ⇒ LLM, ratchet intacto, flag off), integração com `MockInferenceServer` (conta chamadas de LLM evitadas), teste de wall-clock com Jev travado (servidor que dorme) provando `S` de orçamento para decisão **e** atendimento de comando de modo dentro do limite, e um cenário PTY do fluxo de permissão.
- **Surfaces:** `crates/codegen/xai-grok-workspace/src/permission/{auto_mode/mod.rs,manager/mod.rs}`, `crates/codegen/xai-grok-telemetry/src/events/permission_analytics.rs`, `crates/codegen/xai-grok-shell/src/session/acp_session_impl/sampler_turn.rs`, `crates/codegen/xai-grok-pager-pty-harness/tests/scenarios/`.
- **DoD:** todos os testes verdes; nenhuma decisão de allow que o incumbent não daria; latência limitada mesmo com Jev lento/indisponível; contagem de chamadas de LLM medida.
- **Provam:** `V-007`, `V-008`, `V-009`, `V-010`. **Depende de:** `S-004`.

### S-006 — Medição pós-rollout, kill switch e documentação

- **Por quê:** fecha o ciclo de valor e deixa o operador no controle; e é o que permite ao dono decidir promover/reverter.
- **Como:**
  1. Medir em uso real: chamadas de LLM evitadas (por classe), tokens Jev, custo pelo preço publicado por token (`E-016`), latências p50/p95, taxa de escalação e de falso-bloqueio percebido.
  2. Aplicar o **critério de aborto**: se a economia ficar abaixo do piso de `S-001`, manter default OFF e registrar (não "ajustar até passar").
  3. Documentar na user-guide do pager: config `[jev]`, allowlist de campos enviados, declaração de egress/retenção, kill switch e como reverter.
  4. Registrar os candidatos do backlog (§1.3) com o que foi aprendido, para re-proposta individual.
- **Surfaces:** `crates/codegen/xai-grok-pager/docs/user-guide/**`, relatório de medição em `$PLAN_DIR`/artefato de medição.
- **DoD:** relatório de economia com números + página de doc publicada no repo; decisão de default registrada com base no critério.
- **Provam:** `V-011`. **Depende de:** `S-005`.

---

## 5. Mapa de aceitação (critérios → passos/provas)

| Critério de sucesso | Passos | Provas |
|---|---|---|
| C1. Cliente + config + flag existem; flag OFF = zero conexões e comportamento idêntico | S-002 | V-004 |
| C2. Ao menos um seam de produção (classificador de permissão) é Jev-first atrás de flag, com fallback LLM intacto e autoridade Jev ≤ incumbent | S-005 | V-007, V-009 |
| C3. Gate de avaliação com números: falso-allow 0 no conjunto retido inseguro, falso-bloqueio no orçamento, Jev ≥ incumbent; incumbent medido **antes** | S-001, S-004 | V-003, V-006 |
| C4. Latência: com Jev lento/indisponível nenhuma decisão de permissão excede o orçamento e o kill switch/modo continua atendido | S-005 | V-008 |
| C5. Valor: economia medida (chamadas de LLM evitadas) ≥ piso do dono; senão abortar e registrar | S-001, S-005, S-006 | V-003, V-010, V-011 |
| C6. Perguntas/limiares num arquivo único revisável + doc de egress e kill switch | S-003, S-006 | V-005, V-011 |
| C7. Freeze e conselho materializados e validados (contrato sam-plan) | S-000 | V-001, V-002 |

### Provas

| ID | Método | Estado |
|---|---|---|
| V-001 | `validate_plan_report.py --repo-root <repo>` sobre `plan-report.json` | `PLANNED` (bloqueado por plan mode; executa em S-000) |
| V-002 | `sam-council` perfil `full` + `validate_council_report.py` | `PLANNED` (S-000) |
| V-003 | Relatório de instrumentação do incumbent + contagem de requests via `MockInferenceServer` | `PLANNED` (S-001) |
| V-004 | `cargo test -p xai-grok-workspace -p xai-grok-config-types -p xai-grok-env` (wiremock: 200/401/422/429/529/timeout; flag OFF sem conexões; config de projeto não liga) | `PLANNED` (S-002) |
| V-005 | Testes de composição/limiares + golden do request + telemetria + flag OFF inerte | `PLANNED` (S-003) |
| V-006 | Relatório C1/C2: falso-allow 0, falso-bloqueio, delta vs incumbent, calibração | `PLANNED` (S-004) |
| V-007 | Testes do classificador Jev: autoridade ≤ incumbent, `security_findings` ⇒ LLM, ratchet intacto, flag OFF | `PLANNED` (S-005) |
| V-008 | Teste de wall-clock com Jev travado + comandos de modo atendidos no limite | `PLANNED` (S-005) |
| V-009 | Cenário PTY do fluxo de permissão em modo auto | `PLANNED` (S-005) |
| V-010 | Contagem de chamadas de LLM evitadas (mock `requests()`) | `PLANNED` (S-005) |
| V-011 | Relatório de economia com números + página de documentação | `PLANNED` (S-006) |

---

## 6. Riscos

| ID | Risco | Sev | Mitigação |
|----|-------|-----|-----------|
| R-001 | `state` adversarial direciona o Jev para um falso allow (authz) | alta | Autoridade Jev ≤ incumbent; fase sombra primeiro; `security_findings` ⇒ LLM; `Ask` de política nunca elevado; piso de confiança; falso-allow = 0 no conjunto retido com injeções; ratchet de negações intocado (I-3/I-4). |
| R-002 | **Valor não se materializa** (classe rotineira pode já passar sem LLM; pular Jev sob carga limita a economia) | alta | `S-001` mede o prêmio antes; piso numérico de economia + **critério de aborto** (correção do árbitro); se falhar, default OFF e re-propor outro seam (preguiça/compaction) com mais volume de chamadas. |
| R-003 | Fila/bloqueio no ator de permissão (kill switch atrás de rede) | alta | Orçamento do chamador + sub-deadline + **zero retry** + pular Jev quando ocupado; prova `V-008`; requisito explícito de que comandos de modo não fiquem atrás da rede. |
| R-004 | Egresso de dados a terceiro (privacidade) | média | `state` por allowlist tipada, sem conteúdo de arquivo/saída de tool por padrão, `max_state_bytes`, flag OFF por padrão, kill switch, doc de egress; revisão `security-privacy` no conselho `full`. |
| R-005 | `JEV_API_KEY` legível por comandos do agente | média | Exclusão no `shell_environment_policy` por padrão + doc do risco residual (segredo em env é legível se o usuário desfizer a exclusão); nunca logar. |
| R-006 | Mudança aditiva na trait (`budget`) + guarda `in_flight` são mecanismos novos | média | Diff explícito e revisado; testes dedicados (`V-008`); condição de fechamento do árbitro. |
| R-007 | Dependência de vendor (quota, preço, disponibilidade, lock-in) | média | Endpoint/model configuráveis; sem tabela de preço local; fallback LLM sempre presente; custo medido por tokens; kill switch; caminho de saída = flag OFF. |
| R-008 | Escopo "sempre que possível" crescer sem limite | baixa | Regra de elegibilidade (§1.4) + backlog explícito + gate por seam (correção O-007). |
| R-009 | Locators marcados (R) estarem desatualizados | baixa | Re-verificação em `S-000`; nenhuma decisão de desenho depende de número de linha. |

**Refinamentos de risco vindos da varredura do índice (itens citados no ledger `plan/docs-review.md`):**

- **R-001 (adversarial):** o filtro do Jev **não é fronteira de segurança** — uma passagem abaixo do corte ainda chega ao prompt, então o consumidor trata todo texto como não confiável (item 103); a steerability documentada segue valendo como limitação (item 93).
- **R-003 (latência/vendor):** divergência **deliberada** dos SDKs — o default deles é `maxRetries=2` em 408/429/5xx com backoff 0,5 s→5 s, jitter 0,25 e respeito a `Retry-After` (itens 32, 62); nós fixamos `maxRetries=0` no caminho quente. A deadline precisa cobrir a **leitura do corpo completo** (item 41); cancelamento do chamador é estado próprio, sem retry nem fallback (item 43).
- **R-004 (privacidade):** due diligence legal registrada — DPA/MCA disponíveis, compromisso de **não treinar** com dados de cliente, **zero data retention só em plano enterprise** (item 92); a minimização do `state` passa a ser mitigação contratada, não só higiene.
- **R-005 (segredo):** o SDK redige **headers** conhecidos mas **não os corpos** — proibido logar corpo de request/response (itens 23, 68); guardar apenas `x-typesafe-request-id` para correlação (itens 40, 70).
- **R-007 (vendor):** **pinar a versão** do modelo quando os limiares forem calibrados — a resposta traz o ID versionado (item 89); 404 é erro de configuração de endpoint/modelo, não indisponibilidade (item 47); 403 é autorização/escopo de conta, distinto de 401 (item 48).

---

## 7. Conselho (registro da triagem)

- **Perfil pedido:** `fast` (triagem) · **Topologia:** single-host · **Tese:** T-001 rev.1 → rev.2
- **Assentos cegos (3):** `frame-evidence` (id `01a0b154-21c6-70d0-a7fb-0d474bc80025`), `delivery-failure` (`...d5c89c99bbe`), `simplification` (`...d6575bca601`)
- **Verificação:** `triage-arbiter` fresco (`01a0b155-a0b8-76c2-9e69-4851aee8cbc8`)
- **Especialistas:** `security-privacy` SELECTED, `reliability-performance` SELECTED, `testability-release` SELECTED, `cost-dependency` SELECTED (todos disparam → escalação)
- **Resultado:** `REVISE` + `ESCALATE_TO_FULL` (perfil `fast` não aprova; conselho `full` obrigatório em `S-000` antes da fase 2 de `S-005`)

### Objeções e disposições

| ID | Sev | Objeção (resumo) | Disposição | Mudança na tese |
|----|-----|------------------|-----------|-----------------|
| O-001 | BLOCKER | D1 contradizia os próprios não-objetivos: Jev como gate autoritativo de allow + egresso de transcript antes de qualquer gate | `PARTIAL` | Autoridade **Jev ≤ incumbent**; rollout sombra→Jev-first; allowlist de state sem conteúdo de arquivo; pergunta de triagem de injeção; critério de promoção documentado |
| O-002 | HIGH | Gate não falsificável; corpus extraído de testes é in-sample; limiar inexistente; falso-bloqueio não medido | `ACCEPT` | Conjunto retido autoral + regra de rotulagem escrita; **baseline do incumbent no mesmo conjunto**; limiares numéricos; falso-bloqueio reportado; corpus de testes vira só smoke |
| O-003 | MEDIUM | Prêmio e calibração não medidos; retry no caminho quente prejudica latência | `ACCEPT` | `S-001` instrumenta antes; calibração medida em C1; **zero retry** |
| O-004 | HIGH | Worker serial + um timeout; Jev multiplica o pior caso; kill switch enfileirado atrás da rede | `ACCEPT` | Orçamento do chamador, sub-deadline, sem retry, pular Jev quando ocupado, teste de wall-clock + comandos de modo |
| O-005 | HIGH | Allow do Jev zeraria o ratchet de negações; `security_findings` deve forçar o caminho do modelo | `ACCEPT` | I-4 (ratchet intocado) + `security_findings` ⇒ LLM |
| O-006 | MEDIUM | Lista de credenciais só limpa o helper de login; config de projeto poderia ligar/reapontar o Jev | `ACCEPT` | `shell_environment_policy`; `enabled`/`endpoint` só em managed/env/user; teste de zero conexões com flag OFF |
| O-007 | HIGH | D2–D6 são escopo especulativo | `PARTIAL` | v1 = A+B+C+D1+E; resto vai para backlog com regra de elegibilidade |
| O-008 | HIGH | Passo C superdimensionado; E-010 era inferência, não fato | `PARTIAL` | C1 spike descartável → C2 fixtures; corpus de testes rebaixado a smoke |
| O-009 | MEDIUM | Passo A reimplementava transporte/retry do repo | `ACCEPT` | Reusar builder sancionado do `xai-grok-extra-ca`; uma tentativa; sem mudança na lista BYOK |

### Riscos novos do árbitro e tratamento

1. **Jev "tighten-only" pode deslocar zero chamadas de LLM (valor não falsificável)** — alta → tratado em `S-001` (medir antes), no piso numérico de economia e no critério de aborto (`R-002`).
2. **Mudança de interface (`classify` + budget) e guarda `in_flight` não verificadas** — média → `V-008` dedicada + diff explícito (`R-006`).
3. **Citação desatualizada do backlog** (preguiça) — baixa → corrigida para `laziness_classifier.rs:575` + chamador `laziness.rs`.

### Lacunas de conformidade do conselho (registradas, não escondidas)

- Sem `council-report.json` validado, sem scratch e sem prova de validação: `plan mode` proíbe escrita. Executa em `S-000`.
- Bridge de telemetria de subagente (`REMOTE_CODE_SUBAGENT_TELEMETRY_COMMAND`) não disponível/verificada nesta sessão → execução crua, gap registrado (previsto no contrato).
- O perfil `fast` **não** aprova; a aprovação, se vier, virá do conselho `full` em `S-000`.

---

## 8. Simplicidade

**Cortes (o que a v1 deliberadamente NÃO faz):** D2–D6 (compaction/preguiça/memória/busca/roteamento); crate novo; retry/backoff; mudança na lista de credenciais BYOK; tabela de preços local; qualquer remoção de caminho LLM; tool-subset por turno.

**Complexidades retidas (justificadas):** (a) **orçamento fim-a-dia + sub-deadline + pular quando ocupado** — sem isso o ator de permissão pode enfileirar o kill switch atrás de rede (R-003); (b) **gate de avaliação em 2 estágios com baseline do incumbent** — é o único mecanismo que pode reprovar a v1 por segurança e por valor (R-001/R-002); (c) **fase sombra antes de Jev-first** — permite medir concordância em produção sem alterar decisão.

---

## 9. Cobertura do estudo (o que foi verificado)

Ferramentas usadas: `grep` em toda a árvore; leitura direta de `Cargo.toml` (raiz, `xai-grok-http`, `xai-grok-workspace`, `xai-grok-extra-ca`), `README.md`, `CONTRIBUTING.md`, `permission/auto_mode/mod.rs`, `xai-grok-http/src/lib.rs`, `xai-grok-config-types/src/registry.rs`; seis mapeamentos read-only paralelos cobrindo: (1) caminho de sampling/LLM e modelos, (2) caminho de decisão de permissão, (3) loop do agente/tools/subagentes/doom-loop, (4) compaction/memória/busca/trimming, (5) config/env/auth/secrets/hooks/HTTP, (6) telemetria/custo/estratégia de testes; e a documentação do vendor (API, models, confidence, jaggedness, intent-routing, fan-out, parallel questions, guardrails, how-to-build).

**Honestidade de cobertura:** não foi feito leitura linha-a-linha das ~6.000 fontes; o estudo cobriu **todos os pontos onde há chamada de LLM ou decisão estruturada** (o que a pergunta exige) e os subsistemas que os cercam. Locators (R) precisam de re-verificação em `S-000`.

---

## 10. Residuais e estado final

**Residuais:**
1. Conselho `full` + artefatos sam-plan pendentes (contrato) — `S-000`.
2. Chave/plano Jev não verificados (U-001) — depende do dono; sem ela, `S-004`+ fica `BLOCKED`.
3. Números de valor/latência/calibração desconhecidos até `S-001`/`S-004` — por desenho, são os primeiros passos.
4. Backlog (§1.3) deliberadamente fora da v1, com regra de elegibilidade para re-proposta.
5. Gap do bridge de telemetria do conselho.

**Bloqueadores:** nenhum bloqueador absoluto do plano; a ausência de chave Jev é dependência externa do dono (U-001), não bloqueio dos passos locais `S-000`–`S-003`.

**Gate para `READY_TO_EXECUTE`:** (a) artefatos sam-plan validados com HTML (`--require-html`), (b) conselho `full` sem `BLOCKED`/`REVISE`/`ESCALATE_TO_FULL` pendente e sem risco alto não mitigado, (c) U-001 resolvida, (d) demais incógnitas materiais com sonda executada.

---

## 11. Freeze (para materializar em `plan-report.json`)

```json
{
  "schema_version": 1,
  "workflow": "plan",
  "status": "NOT_CONFIDENT",
  "depth": "standard",
  "case_type": "FEATURE",
  "complexity_rationale": "Integração de um serviço de decisão externo de terceiro num caminho de autorização (permissão/auto-mode) de um harness Rust grande, com contrato de API próprio, gate de avaliação necessário e restrições de manifesto gerado; escopo reduzido a um seam de produção + fundação reutilizável.",
  "risk_flags": ["security_privacy", "auth_boundary", "material_uncertainty"],
  "study": {
    "tools_used": ["grep -ri jev|typesafe (repo inteiro)", "leitura direta de Cargo.toml raiz/xai-grok-http/xai-grok-workspace/xai-grok-extra-ca", "leitura direta de permission/auto_mode/mod.rs, xai-grok-http/src/lib.rs, config-types/registry.rs", "6 mapeamentos read-only paralelos (sampling, permissão, loop do agente, compaction/memória/busca, config/env/auth/hooks/http, telemetria/custo/testes)", "web_fetch docs.typesafe.ai (api, models, confidence, jaggedness, intent-routing, fan-out, parallel_questions, llm_guardrails, how-to-build-with-system-one, agent-skill)"],
    "surfaces_mapped": ["crates/codegen/xai-grok-workspace/src/permission/**", "crates/codegen/xai-grok-workspace/Cargo.toml", "crates/codegen/xai-grok-shell/src/session/acp_session_impl/{sampler_turn.rs,turn.rs,laziness_classifier.rs}", "crates/codegen/xai-grok-shell/src/agent/config.rs", "crates/codegen/xai-grok-sampler/src/**", "crates/codegen/xai-grok-config-types/src/{registry.rs,flags.rs}", "crates/codegen/xai-grok-env/src/registry.rs", "crates/codegen/xai-grok-http/src/lib.rs", "crates/codegen/xai-grok-extra-ca/src/lib.rs", "crates/codegen/xai-grok-telemetry/src/events/**", "crates/codegen/xai-grok-test-support/src/mock_server.rs", "crates/codegen/xai-grok-pager-pty-harness/src/scripted.rs", "crates/common/xai-grok-compaction/src/**", "crates/codegen/xai-grok-memory/src/{search.rs,v2.rs}", "crates/codegen/xai-grok-session-search/src/fts.rs", "Cargo.toml"],
    "prompt_ambiguities": ["'sempre que possível' não é finito: operacionalizado pela regra de elegibilidade (§1.4) + backlog explícito, com v1 restrita a um seam de produção"],
    "repo_root": "/Users/samuelfajreldines/dev/jev-build"
  },
  "frozen": {
    "prompt_hash": "sha256:PLACEHOLDER_AT_S000",
    "prompt_summary": "Usar /sam-plan para verificar todo o repositório e planejar o uso do Jev (TypeSafe) sempre que possível no jev-build, com o Jev delegando à LLM o que não puder fazer.",
    "goal": "Fazer o harness usar Jev para decisões estreitas e estruturadas sempre que a regra de elegibilidade permitir, começando pelo classificador de permissão do auto-mode, com autoridade Jev <= incumbent, fallback LLM intacto, flag default OFF, gate de avaliação medido e economia medida.",
    "non_goals": ["Jev para geração de texto", "Jev como único gate de allow", "remover caminhos LLM/heurísticos", "criar crate novo ou editar o manifesto raiz gerado", "mudar contratos públicos", "egresso de conteúdo de arquivo/saída de tool por padrão", "afirmação de economia em dólar não medida", "criar skill/MCP/plugin/hook novo quando existe capacidade equivalente", "versionar conteúdo de dependências externas (skills do usuário, plugins) como fonte do repo"],
    "success_criteria": ["Cliente+config+flag com flag OFF provadamente inerte e sem conexões", "Classificador de permissão Jev-first atrás de flag com fallback LLM e autoridade <= incumbent", "Gate de avaliação com falso-allow 0 no conjunto retido, falso-bloqueio no orçamento e Jev >= incumbent, com incumbent medido antes", "Latência limitada e kill switch não bloqueado com Jev lento/indisponível", "Economia medida (chamadas de LLM evitadas) >= piso do dono, senão abortar e registrar", "Perguntas/limiares em arquivo único revisável + doc de egress/kill switch", "Freeze e conselho `full` materializados e validados"],
    "invariants": ["I-1 flag OFF = comportamento idêntico + zero conexões", "I-2 precedência de permissão intocada", "I-3 autoridade Jev <= incumbent", "I-4 allow Jev nunca zera ratchet de negações", "I-5 zero retry no caminho quente, sub-deadline, pular quando ocupado", "I-6 segredo nunca logado e fora do ambiente de subprocessos por padrão", "I-7 toda decisão Jev observável em telemetria", "I-8 nenhum caminho LLM removido", "I-9 reuso antes de criação de skill/MCP/plugin/hook", "I-10 capacidade de governança com rótulo explícito, sem conformidade fingida"],
    "constraints": ["Cargo.toml raiz é gerado/sincronizado", "xai-grok-http depende de xai-grok-workspace (ciclo)", "xai-grok-workspace já tem reqwest/serde_json/tokio/extra-ca", "clippy.toml exige o builder do extra-ca", "Jev é texto apenas; 64k/32k tokens"],
    "no_go": ["implementar sem o gate C", "ligar por default", "enviar conteúdo de arquivo/saída de tool sem opt-in", "reapontar endpoint/enabled por config de projeto", "retry/backoff no caminho quente"]
  },
  "output": {"plan_dir": "/Users/samuelfajreldines/dev/jev-build/plan", "html_files": []},
  "evidence": [
    {"id": "E-001", "kind": "grep", "classification": "FACT", "claim": "Repo não tem referências a jev/typesafe hoje", "locator": "command: grep -ri jev|typesafe em /Users/samuelfajreldines/dev/jev-build (0 resultados)"},
    {"id": "E-002", "kind": "code", "classification": "FACT", "claim": "Trait PermissionClassifier e ClassifierOutcome são o ponto de injeção existente", "locator": "crates/codegen/xai-grok-workspace/src/permission/auto_mode/mod.rs:328"},
    {"id": "E-003", "kind": "code", "classification": "FACT", "claim": "Classificador chamado inline pelo ator de permissão; falha => Unavailable => prompt humano; ator é task única", "locator": "crates/codegen/xai-grok-workspace/src/permission/manager/mod.rs:913"},
    {"id": "E-004", "kind": "code", "classification": "FACT", "claim": "Wiring do classificador LLM com timeout e worker serial", "locator": "crates/codegen/xai-grok-shell/src/session/acp_session_impl/sampler_turn.rs:921"},
    {"id": "E-006", "kind": "manifest", "classification": "FACT", "claim": "Manifesto raiz gerado; xai-grok-http depende de xai-grok-workspace (ciclo)", "locator": "Cargo.toml:1"},
    {"id": "E-007", "kind": "manifest", "classification": "FACT", "claim": "xai-grok-workspace já tem reqwest/serde/serde_json/tokio/extra-ca", "locator": "crates/codegen/xai-grok-workspace/Cargo.toml:108"},
    {"id": "E-008", "kind": "code", "classification": "FACT", "claim": "Registry de features com tiers e default por linha", "locator": "crates/codegen/xai-grok-config-types/src/registry.rs:56"},
    {"id": "E-016", "kind": "docs", "classification": "FACT", "claim": "Contrato do Jev: endpoint, auth, modelo, limites, preço, fraquezas", "locator": "command: web_fetch https://docs.typesafe.ai/api.md, /models.md, /model-jaggedness/jev-1.13.md"},
    {"id": "E-018", "kind": "docs", "classification": "FACT", "claim": "Repo é sincronizado do monorepo; user-guide mora no pager", "locator": "README.md:90"}
  ],
  "assumptions": [
    {"id": "A-001", "claim": "Acurácia do Jev nas nossas decisões precisa ser medida antes de fiação de produção", "state": "UNVERIFIED", "decision_reason": "medida no passo S-004 (gate C); nenhuma decisão de produção antes"},
    {"id": "A-002", "claim": "confidence é documentado presente, não calibrado", "state": "UNVERIFIED", "decision_reason": "calibração medida em S-004; autoridade <= incumbent limita o dano"},
    {"id": "A-003", "claim": "Latência ~100 ms (docs) não medida neste host", "state": "UNVERIFIED", "decision_reason": "medida em S-004; desenho não depende dela"},
    {"id": "A-004", "claim": "Taxa de chamadas da side call do classificador é desconhecida", "state": "UNVERIFIED", "decision_reason": "S-001 instrumenta antes de construir"}
  ],
  "unknowns": [
    {"id": "U-001", "claim": "Existe chave/plano Jev no ambiente?", "material": true, "probe": "S-000 verifica TYPESAFE_API_KEY e conta; sem chave S-004+ fica BLOCKED"},
    {"id": "U-002", "claim": "Latência/jitter reais da API", "material": true, "probe": "medir p50/p95 em S-004"},
    {"id": "U-003", "claim": "Chamadas/tokens do incumbent por classe", "material": true, "probe": "S-001"},
    {"id": "U-004", "claim": "Calibração das confidences do Jev", "material": true, "probe": "curva confiança x acurácia em S-004"}
  ],
  "thesis": {
    "id": "T-001",
    "summary": "Cliente Jev pequeno em xai-grok-workspace + camada 'decide ou escala', convertendo UM seam de produção (classificador de permissão) com autoridade Jev <= incumbent, default OFF, gate de avaliação com conjunto retido e medição de economia com critério de aborto.",
    "approach": "Fundação reutilizável (cliente, config, flag, catálogo de perguntas, telemetria) -> instrumentar incumbent -> gate C1/C2 -> integração D1 em 2 fases -> medição/docs.",
    "rejected_alternatives": ["hooks HTTP (não podem conceder allow)", "crate novo xai-grok-jev (exige editar manifesto raiz gerado)", "Jev como gate autoritativo de allow (rev.1, rejeitado pelo conselho)", "converter 5-6 seams de uma vez", "retry/backoff no caminho quente", "tabela de preços local", "substituir sumário de compaction (geração é fora de escopo do Jev)"]
  },
  "steps": [
    {"id": "S-000", "title": "Materializar o freeze e rodar o conselho full", "why": "Contrato sam-plan + escalação do conselho exigem artefatos validados antes de implementar", "how": ["scaffold_plan_dir.py --out $PLAN_DIR", "escrever plan-report.json a partir deste freeze", "validate_plan_report.py --repo-root", "render_plan_html.py + --require-html", "rodar sam-council perfil full (6 assentos + 4 especialistas) com validate_council_report.py", "verificar U-001 e re-verificar locators (R)"], "depends_on": [], "surfaces": ["plan/**"], "dod": "plan-report.json + HTML no disco validados (--require-html), relatório do conselho validado, locators (R) reconferidos", "proof_ids": ["V-001", "V-002"]},
    {"id": "S-001", "title": "Instrumentar o incumbent e fixar o piso de economia", "why": "Critério de aborto por valor exigido pelo árbitro; medir o prêmio antes de construir", "how": ["contar chamadas do classificador por classe + tokens + latência p50/p95", "contagem de requests de side query via MockInferenceServer em teste de integração", "definir piso numérico de economia e critério de aborto com o dono"], "depends_on": ["S-000"], "surfaces": ["crates/codegen/xai-grok-shell/src/session/acp_session_impl/sampler_turn.rs", "crates/codegen/xai-grok-telemetry/src/events/**", "crates/codegen/xai-grok-test-support/src/mock_server.rs"], "dod": "relatório com números reais e piso aprovado; zero mudança de comportamento", "proof_ids": ["V-003"]},
    {"id": "S-002", "title": "Cliente Jev + config/flag/credencial", "why": "Fundação que torna os próximos seams baratos", "how": ["criar src/jev/ com tipos serde, cliente via xai_grok_extra_ca::build_reqwest_client, bearer por nome de env, timeout, sem retry, taxonomia de erro, tokens", "adicionar seção [jev] e linha Feature::Jev (GROK_JEV, default off)", "pin de enabled/endpoint em managed/env/user; JEV_API_KEY no shell_environment_policy", "testes wiremock 200/401/422/429/529/timeout + flag OFF sem conexões + config de projeto não liga"], "depends_on": ["S-000"], "surfaces": ["crates/codegen/xai-grok-workspace/src/jev/**", "crates/codegen/xai-grok-workspace/Cargo.toml", "crates/codegen/xai-grok-shell/src/agent/config.rs", "crates/codegen/xai-grok-config-types/src/registry.rs", "crates/codegen/xai-grok-env/src/registry.rs"], "dod": "cargo test dos 3 crates verde; sem dep de produção nova; flag OFF inerte; sem segredo em log", "proof_ids": ["V-004"]},
    {"id": "S-003", "title": "Camada decide-ou-escala + catálogo único de perguntas", "why": "Perguntas/limiares revisáveis num só arquivo; composição em código com autoridade limitada", "how": ["questions.rs com pacotes/limiares/pesos/pisos (pacote de permissão: Nouls atômicos + Noul de injeção + Choice de classe de risco)", "policy.rs com state por allowlist tipada, uma requisição com bateria especulativa, composição em código, JevDecision{Block|AllowEligible|Escalate}", "evento de telemetria aditivo por decisão + contagem de escalações", "testes de composição/limiares/golden/telemetria/flag OFF"], "depends_on": ["S-002"], "surfaces": ["crates/codegen/xai-grok-workspace/src/jev/{questions,policy}.rs", "crates/codegen/xai-grok-telemetry/src/events/**"], "dod": "testes verdes; nada plugado em produção ainda", "proof_ids": ["V-005"]},
    {"id": "S-004", "title": "Gate de avaliação C1 -> C2", "why": "Único gate que pode reprovar a v1 por segurança e por valor", "how": ["C1: spike offline com contextos gravados + casos autorais com injeção; rodar incumbent e Jev no mesmo conjunto", "rotular parte insegura sob regra escrita; corpus de testes só como smoke", "C2: promover a fixtures versionadas se C1 passar", "registrar go/no-go com números e piso de economia"], "depends_on": ["S-003"], "surfaces": ["crates/codegen/xai-grok-workspace/src/jev/eval/**", "crates/codegen/xai-grok-test-support/src/**"], "preconditions": ["U-001"], "dod": "falso-allow 0, falso-bloqueio <= orçamento, Jev >= incumbent, calibração medida, decisão registrada", "proof_ids": ["V-006"]},
    {"id": "S-005", "title": "Integração D1: classificador de permissão Jev-first", "why": "Seam de maior custo de LLM por ação; experimento real de valor", "how": ["impl JevPermissionClassifier + variante de proveniência Jev nos enums fechados + drift tests", "instalar no wiring do shell com ordem Jev -> Escalate/Unavailable -> LLM -> prompt", "mudança aditiva na trait para orçamento do chamador; sub-deadline; zero retry; pular quando in_flight>1; comandos de modo nunca atrás de rede", "allow só na classe rotineira, sem security_findings/Ask, confiança >= piso; nunca tocar auto_consecutive_denials", "fase 1 sombra; fase 2 Jev-first; flag OFF = comportamento atual", "testes unit + mock (LLM evitado) + wall-clock com Jev travado + cenário PTY"], "depends_on": ["S-004"], "surfaces": ["crates/codegen/xai-grok-workspace/src/permission/auto_mode/mod.rs", "crates/codegen/xai-grok-workspace/src/permission/manager/mod.rs", "crates/codegen/xai-grok-telemetry/src/events/permission_analytics.rs", "crates/codegen/xai-grok-shell/src/session/acp_session_impl/sampler_turn.rs", "crates/codegen/xai-grok-pager-pty-harness/tests/scenarios/**"], "dod": "testes verdes; nenhuma decisão além do incumbent; latência limitada com Jev lento", "proof_ids": ["V-007", "V-008", "V-009", "V-010"]},
    {"id": "S-006", "title": "Medição pós-rollout, kill switch e documentação", "why": "Fecha o ciclo de valor e devolve controle ao operador", "how": ["medir chamadas de LLM evitadas, tokens Jev, custo pelo preço publicado, latências, escalações", "aplicar critério de aborto (abaixo do piso => default OFF + registro)", "documentar config, allowlist do state, egress/retenção, kill switch e reversão na user-guide", "registrar candidatos do backlog para re-proposta"], "depends_on": ["S-005"], "surfaces": ["crates/codegen/xai-grok-pager/docs/user-guide/**"], "dod": "relatório de economia com números + doc publicada; decisão de default registrada", "proof_ids": ["V-011"]}
  ],
  "risks": [
    {"id": "R-001", "claim": "State adversarial direciona o Jev para falso allow", "severity": "high", "mitigation": "Autoridade <= incumbent; sombra primeiro; security_findings => LLM; piso de confiança; falso-allow 0 no conjunto retido; ratchet intocado"},
    {"id": "R-002", "claim": "Valor não se materializa", "severity": "high", "mitigation": "S-001 mede antes; piso numérico + critério de aborto; se falhar, default OFF e re-propor outro seam"},
    {"id": "R-003", "claim": "Fila/bloqueio no ator de permissão", "severity": "high", "mitigation": "Orçamento do chamador, sub-deadline, zero retry, pular quando ocupado, prova V-008"},
    {"id": "R-004", "claim": "Egresso de dados a terceiro", "severity": "medium", "mitigation": "Allowlist de state, sem conteúdo de arquivo por padrão, max_state_bytes, opt-in, doc, conselho full"},
    {"id": "R-005", "claim": "Chave legível por comandos do agente", "severity": "medium", "mitigation": "shell_environment_policy + doc do risco residual + nunca logar"},
    {"id": "R-006", "claim": "Mudança de interface e guarda de concorrência novas", "severity": "medium", "mitigation": "Diff explícito + V-008 + condição de fechamento do árbitro"},
    {"id": "R-007", "claim": "Dependência de vendor", "severity": "medium", "mitigation": "Endpoint/model configuráveis; sem preço local; fallback LLM sempre presente; kill switch"},
    {"id": "R-008", "claim": "Escopo sem limite", "severity": "low", "mitigation": "Regra de elegibilidade + backlog + gate por seam"},
    {"id": "R-009", "claim": "Locators (R) desatualizados", "severity": "low", "mitigation": "Re-verificação em S-000"}
  ],
  "verifications": [
    {"id": "V-001", "status": "PLANNED", "reason": "rodar validate_plan_report.py --repo-root sobre plan-report.json em S-000 (plan mode proibiu escrita)"},
    {"id": "V-002", "status": "PLANNED", "reason": "rodar sam-council perfil full + validate_council_report.py em S-000"},
    {"id": "V-003", "status": "PLANNED", "reason": "relatório de instrumentação do incumbent + contagem de requests no mock em S-001"},
    {"id": "V-004", "status": "PLANNED", "reason": "cargo test -p xai-grok-workspace -p xai-grok-config-types -p xai-grok-env em S-002"},
    {"id": "V-005", "status": "PLANNED", "reason": "testes de composição/limiares/golden/telemetria/flag OFF em S-003"},
    {"id": "V-006", "status": "PLANNED", "reason": "relatório C1/C2 com falso-allow/falso-bloqueio/delta/calibração em S-004"},
    {"id": "V-007", "status": "PLANNED", "reason": "testes do classificador Jev (autoridade, security_findings, ratchet, flag off) em S-005"},
    {"id": "V-008", "status": "PLANNED", "reason": "teste de wall-clock com Jev travado + comandos de modo atendidos em S-005"},
    {"id": "V-009", "status": "PLANNED", "reason": "cenário PTY do fluxo de permissão em S-005"},
    {"id": "V-010", "status": "PLANNED", "reason": "contagem de chamadas de LLM evitadas via mock em S-005"},
    {"id": "V-011", "status": "PLANNED", "reason": "relatório de economia + doc em S-006"}
  ],
  "acceptance_trace": [
    {"criterion": "Cliente+config+flag com flag OFF provadamente inerte e sem conexões", "step_ids": ["S-002"], "proof_ids": ["V-004"]},
    {"criterion": "Classificador de permissão Jev-first atrás de flag com fallback LLM e autoridade <= incumbent", "step_ids": ["S-005"], "proof_ids": ["V-007", "V-009"]},
    {"criterion": "Gate de avaliação com falso-allow 0 no conjunto retido, falso-bloqueio no orçamento e Jev >= incumbent, com incumbent medido antes", "step_ids": ["S-001", "S-004"], "proof_ids": ["V-003", "V-006"]},
    {"criterion": "Latência limitada e kill switch não bloqueado com Jev lento/indisponível", "step_ids": ["S-005"], "proof_ids": ["V-008"]},
    {"criterion": "Economia medida (chamadas de LLM evitadas) >= piso do dono, senão abortar e registrar", "step_ids": ["S-001", "S-005", "S-006"], "proof_ids": ["V-003", "V-010", "V-011"]},
    {"criterion": "Perguntas/limiares em arquivo único revisável + doc de egress/kill switch", "step_ids": ["S-003", "S-006"], "proof_ids": ["V-005", "V-011"]},
    {"criterion": "Freeze e conselho `full` materializados e validados", "step_ids": ["S-000"], "proof_ids": ["V-001", "V-002"]}
  ],
  "council": {
    "required": true,
    "skip_reason": null,
    "runs": [
      {"profile": "fast", "topology": "single-host", "seats": ["frame-evidence=01a0b154-21c6-70d0-a7fb-0d474bc80025", "delivery-failure=01a0b154-21c6-70d0-a7fb-0d5c89c99bbe", "simplification=01a0b154-21c6-70d0-a7fb-0d6575bca601"], "verification": ["triage-arbiter=01a0b155-a0b8-76c2-9e69-4851aee8cbc8"], "specialists": ["security-privacy=SELECTED", "reliability-performance=SELECTED", "testability-release=SELECTED", "cost-dependency=SELECTED"], "result": "REVISE + ESCALATE_TO_FULL", "report_path": null, "limitations": "plan mode impediu council-report.json/scratch/validador; bridge de telemetria indisponível; conselho full obrigatório em S-000"}
    ]
  },
  "simplicity": {
    "cuts": ["D2-D6 fora da v1", "crate novo", "retry/backoff", "mudança na lista BYOK", "tabela de preços local", "remoção de caminhos LLM", "tool-subset por turno"],
    "retained_complexity_justifications": ["orçamento do chamador + sub-deadline + pular quando ocupado (R-003)", "gate C1/C2 com baseline do incumbent (R-001/R-002)", "fase sombra antes de Jev-first (medir sem alterar decisão)"]
  },
  "extensions": {
    "identity_preference": "Reusar capacidade equivalente existente (skill/MCP/plugin/hook/subagente) antes de criar forma nova; conteúdo de skill e de plugins é dado externo de runtime — Jev recebe só índice/descrição/metadados e nunca é a única fonte de um gate; capacidade de governança (auditoria/observabilidade/versionamento) leva rótulo explícito e nunca finge conformidade.",
    "governance_registry": [
      {"surface": "hooks", "locator": "crates/codegen/xai-grok-hooks/src/event.rs:79", "policy": "não concede allow; serve de sonda e de trava de experimento"},
      {"surface": "client hooks (ACP)", "locator": "crates/codegen/xai-grok-shell/src/session/acp_session/hooks.rs:87", "policy": "Ask de hook de cliente falha aberto; nunca usar como gate"},
      {"surface": "guardrails in/out", "locator": "não existe hoje", "policy": "segurança, não economia; custo de chamadas explícito"},
      {"surface": "MCP/marketplace/pager", "locator": "crates/codegen/xai-grok-shell/src/session/acp_session_impl/mcp_init.rs:651", "policy": "alvo da auditoria security-privacy; sem allow direto"},
      {"surface": "skills (catálogo)", "locator": "crates/codegen/xai-grok-agent/src/prompt/skills.rs:66", "policy": "igualdade de capacidade; conteúdo externo não versionado"}
    ],
    "maximization_ladder_ref": "§1.3.1 (P1-P6 + regra de ouro dos candidatos pré-reduzidos)",
    "estimate": {"baseline_session_input_tokens": "3-5M (sessão longa)", "expected_savings_pct": "20-50% (central ~35%)", "expected_savings_tokens": "0.8-2.2M por sessão longa", "jev_own_tokens": "150-300k", "caveat": "estrutural; medir em S-001"},
    "cookbook_sweep": "18 cookbooks do menu lidos e classificados em §12 (economia P1-P6 / segurança / fora de escopo)",
    "changelog_ref": "§12 Changelog (varredura do menu docs.typesafe.ai)"
  },
  "residuals": ["Conselho full + artefatos sam-plan pendentes (S-000)", "Chave/plano Jev não verificados (U-001)", "Números de valor/latência/calibração até S-001/S-004", "Backlog fora da v1", "Gap do bridge de telemetria do conselho"],
  "blockers": []
}
```

---

## 12. Changelog — varredura do menu docs.typesafe.ai (item → lição → ajuste)

Varredura feita em **2026-09-17 19:22 (-03)**. Cada linha é um item do menu efetivamente lido; "sem ajuste" = lição já estava refletida no plano e foi apenas conferida.

### 12.1 Cookbooks (menu lateral)

| # | Item lido | Lição aprendida | Ajuste no plano |
|---|---|---|---|
| 1 | Self-consistency: nouls | Noul pode retornar 0,5 honesto; rotear incerteza em vez de forçar resposta | Sem canal de auto-revisão nesta v1 (não é economia); registrado em §1.8 como ciência comportamental, não ferramenta ativa |
| 2 | Self-consistency: choices | Distribuição de probabilidades revela opções ambíguas; comparar rótulo com acordo | Sem ajuste (mesmo caso do #1) |
| 3 | Parallel questions | Uma requisição com N perguntas = ~12× mais barato e ~10× mais rápido que N requisições | Já em S-003 (bateria especulativa numa requisição); reforçado no custo do Jev em §1.3.1 |
| 4 | Re-ranking | Ranquear candidatos com uma pergunta por par candidato→consulta supera BM25 puro | Aplicado em P2/P3 (poucos candidatos, ranqueados antes de entrar no contexto) |
| 5 | Line-by-line search | Anotar ids de linha e ranquear com Choice; Noul separa "sem resposta" de "resposta ruim"; teto de 255 opções por Choice | Virou a base técnica de P2 + regra de ouro (shortlist em código antes do Jev) |
| 6 | Structure recovery | Parsear estrutura em blocos com perguntas encadeadas | Sem ajuste (fora do escopo: extração de estrutura de documento) |
| 7 | Function calling | Mapear nomes de função e argumentos de conjunto fechado para perguntas tipadas; `stated` pergunta se o argumento foi mencionado | Virou P1 (seleção de tools) e P5 (validar a chamada antes de executar) |
| 8 | Skill suggestion | Ranquear todo o catálogo, depois re-ler os 3 primeiros; aceitar "nenhuma"; lista errar menos é pior que ajudar | Virou P6 + igualdade de capacidade em §1.8 (reusar skill existente em vez de criar forma nova) |
| 9 | Knowledge graph entity alignment | Uma Score basta quando os níveis *são* as ações (mesclar/não/curar) | Sem ajuste (domínio diferente); princípio já coberto pela regra de elegibilidade §1.4 |
| 10 | Classifying RAG passages | Pontuar cada passagem e decidir em código o que chega ao modelo; sinalizar injeção | Base de P2 (filtrar saída de tool antes do histórico) e P3 (recorte da compaction) |
| 11 | Double-checking citations | Verificar citação contra a fonte; confiança sinaliza revisão | Deferido (qualidade, não economia); anotado em §1.8 como candidato de segurança/qualidade |
| 12 | Guardrails for LLMs | Triagem in/out com bateria de Nouls + Score de severidade e roteamento por limiares | Registrado em §1.8 como **segurança, não economia** (adiciona chamadas); entra pela via de decisão estruturada com autoridade limitada |
| 13 | SDE cascade | Cascata mini → verificação → raciocínio para extração estruturada | Sem ajuste (extração de dados, não decisão de harness); conceito de cascata já é o "Jev primeiro, LLM no resto" |
| 14 | Date extraction | Extração é julgamento; aritmética de data pertence ao código | Reforça a fraqueza documentada (E-016) e a regra "matemática/datas = código" (§1.3, linha final) |
| 15 | Pre-parsed value extraction | Regex acha candidatos; o modelo escolhe o span; código normaliza | Reforça a regra de ouro de §1.3.1 (candidatos primeiro, Jev depois) |
| 16 | Hierarchical classification | Classificar por hierarquia (grupo → item) com busca em feixe sobre as probabilidades | Aplicado em P1: perguntar por família primeiro, catálogo completo só quando precisar desambiguar |
| 17 | Autoresearch feature discovery | Loop offline que propõe perguntas e treina modelo clássico com os erros | Sem ajuste (offline/treino, não runtime) — registrado como "não é runtime" em §1.3.1 |
| 18 | Classification using confidence | Usar a confiança para decidir entre o rótulo específico e o nível acima | Já é o padrão de S-003/S-005 (piso de confiança + escalação); limite de 255 opções anotado |

### 12.2 Demais seções do menu (lidas na mesma varredura)

| Seção | Lição | Ajuste |
|---|---|---|
| Introduction / Quickstart | Contrato HTTP, bearer, `TYPESAFE_API_KEY`, `jev-latest` | Já em E-016/S-002 |
| System One / State | Estado é material de painel; separar conteúdo de pergunta; estado grande degrada | Já em §1.4 item 2 e S-003; regra de estado minimizado reforçada |
| Primitives + Advanced: structure | Choice/Score/Noul e estrutura JSON nos critérios | Já em S-003 |
| Confidence | Faixas por risco; agir/perguntar/escalar | Já em §1.4 itens 3-4 |
| Patterns (fan-out, confidence-routing, composite-scoring, intent-routing) | Bateria especulativa; compor em código; rotear por intenção | Fan-out/composição em S-003; intent-routing em P4 |
| Models / API / SDKs (Python, JS) | **Não há SDK Rust** — cliente HTTP próprio é o caminho | Já em S-002 (HTTP direto) |
| Agent skill | Instalação via plugin/marketplace e `npx skills add` | Virou §1.8 (igualdade de capacidade) + §1.4 item 8 |
| Demos | Playground e exemplos | Sem ajuste |
| Legal | Termos e políticas | Sem ajuste (nada no plano) |
| Model jaggedness (jev-1.13) | Limites: sem geração, aritmética/datas contagem não confiáveis, indireção, estado grande, conteúdo adversarial | Já em E-016 e nas regras de autoridade (I-3) e de elegibilidade (§1.4) |

### 12.3 Anexos

**Anexo A — o que mudou nesta rodada de varredura:** §1.3.1 (escada de maximização P1-P6 + regra de ouro dos candidatos pré-reduzidos) · §1.4 item 8 (reuso antes de criação) · §1.5 (dois não-objetivos novos: não criar capacidade duplicada; não versionar dependência externa) · §1.6 I-9/I-10 · §1.8 (registro de governança: hooks, hooks de cliente, guardrails, MCP/marketplace/pager, skills, auditoria) · §11 `extensions` (ladder, identidade, governança, estimativa) · §12 (este changelog).

**Anexo B — limitações honestas da varredura (atualizado em 2026-09-17, 2ª rodada):** a varredura cobre o índice inteiro (111 itens) item a item — 16 itens lidos diretamente pelo planejador e 94 por cinco leitores read-only dedicados (67 páginas de referência de SDK Python/JS, 13 cookbooks restantes, 13 páginas de fundamentos e Legal). **Nenhuma falha de leitura.** O ledger item a item (lição + veredito + ajuste) está em `plan/docs-review.md`; a reconciliação de contagem, em §12.4. Limitação remanescente: as páginas de SDK são espelhos de tipos de runtime JS/Python — nelas, "aplicável" vale para o **contrato HTTP** (shapes, erros, defaults), não para herança de classe.

### 12.4 Reconciliação da cobertura do índice (111 itens)

- Snapshot do índice: `plan/assets/docs-review-index.md` (111 itens, capturado de `llms.txt`).
- Ledger: `plan/docs-review.md` — 111 entradas, cada uma com `Lição:` e `Veredito:`; **111/111 lidos, 0 falhas de leitura**.
- Vereditos: **97 aplicáveis** (cada um com ajuste citado) e **14 não aplicáveis** com motivo: itens 19 (índice de demos), 22, 25, 27, 29 (SDK Python: específico de linguagem, índice ou duplicata), 51, 56, 57, 72, 74, 75, 82 (stubs ou inferência de tipo JS sem efeito no wire), 105 (guardrails: segurança, não economia), 110 (autoresearch: offline/treino).
- Ajustes desta 2ª rodada: §1.3.1 (refinamentos P1/P2/P3/P5/P6, números de confiança 0,60/0,85 e banda 0,30–0,70, calibração por faixa na avaliação), §6 (R-001/R-003/R-004/R-005/R-007), §12.5 (contrato HTTP, mapa de erros e defaults) e `extensions.docs_scan` no `plan-report.json`.
- Núcleo congelado: status, `frozen`, tese e invariantes **I-1..I-10** permanecem presentes e com o mesmo sentido; nada nesta rodada os enfraquece (I-9 e I-10 foram conferidos, não editados).

### 12.5 Anexo C — contrato HTTP, mapa de erros e defaults (itens 2, 21–35 e 36–88)

**Request** — `POST https://api.typesafe.ai/v1/systemone`; bearer de env configurável (`TYPESAFE_API_KEY`-like, atrás de `GROK_JEV`); `model` **resolvido antes do envio** (default `jev-latest`); corpo `{state, model, questions}` com campos extras encaminhados (null inclusive); `state` = string | objeto | array, com nulls aninhados (texto puro); `questions` não-vazio; `Choice.criteria` = mapa label→descrição|null (≤255 opções); `Score.criteria` = **array ordenado** com ≥2 níveis; `Noul.criteria` = `{true?, false?}`.

**Response** — `{answers{<name>: {type, noul | choice+probabilities+confidence | score+legend+probabilities+confidence}}, model, usage{input_tokens, output_tokens}}` (snake_case; `legend`/`probabilities` com chaves string numéricas); capturar `x-typesafe-request-id` por chamada como evidência.

**Contrato de tipos (serde) para S-002** — pergunta é união tagueada pelo campo `type` (`noul`/`score`/`choice`) com resposta correspondente por nome (itens 78, 79, 53, 63, 58); `instructions` default `null` e construtores `choice(instructions, criteria)`, `noul(instructions?, criteria?)` e `score(instructions, criteria)` validando ≥2 níveis (itens 86, 87, 88); `ChoiceCriteria` é mapa de labels livres → descrição|`null` (item 71); respostas expõem `noul`, `choice`+`probabilities`+`confidence`, `score`+`legend`+`probabilities`+`confidence`, com chaves numéricas serializadas como string (itens 59, 54, 64, 81); `usage` em snake_case (`input_tokens`/`output_tokens`) (item 69); `state`, `instructions` e descrições aceitam string, objeto, array ou `null` aninhado (itens 73, 76); campos extras do request são preservados (itens 65, 66); `Logger` expõe quatro níveis (`debug`/`info`/`warn`/`error`) e a lista canônica é `debug|info|warn|error|off`, default `warn` (itens 55, 77, 84); ao desligar a flag/caminho, o pool HTTP é fechado de forma determinística (item 28).

**Erros** — 400/401/403/404/422 e corpo 200 estruturalmente inválido → `Invalid` (com `field_path` quando houver); 408/429 → `RateLimited` (retry-after em ms); 5xx e **529** → `Unavailable` (**529 é mapeamento nosso**: não existe classe para ele nos SDKs); rede/TLS → `Transport`; timeout → `Timeout`; abort do chamador → estado próprio, sem retry nem fallback.

**Defaults** — base URL `https://api.typesafe.ai`; timeout **10 s por operação/tentativa** (no nosso caso: deadline cobrindo a leitura do corpo, sem retry no caminho quente); nível de log default `warn` com `debug|info|warn|error|off`; precedência **explícito > env > default**, env vazia ignorada; superfície de referência pinada no SDK **0.6.0**.

**Divergências deliberadas dos SDKs** — (a) `maxRetries=0` no caminho quente (default dos SDKs: 2, em 408/429/5xx, com retry de erros de conexão/timeout também ligado por padrão, backoff 0,5 s→5 s, jitter 0,25 e `Retry-After` limitado a 60 s); (b) kind de resposta desconhecido → `Invalid` (os SDKs registram e ignoram).

### 12.6 Rastreabilidade item → local no plano (97 itens aplicáveis; os 14 não aplicáveis estão no ledger com motivo)

| Itens do índice | Onde a lição vive no plano |
|---|---|
| 1, 3, 20 | §1.6 **I-9** + §1.5: o Jev devolve tipo/probabilidade/confidence e **nunca gera texto**; o fallback é o caminho LLM/pessoa |
| 4, 5, 93 | §1.7 (texto apenas; tetos 64k/32k) + §12.5 (`state` por allowlist; caminhos em backticks) |
| 6, 9, 109 | §1.3.1 **P1** (opções em `criteria` com teto 255; taxonomia trimada; beam como experimento) |
| 97, 98, 104, 108 | §1.3.1 **P2** (shortlist em código é o teto; decidível em código primeiro; spans; dois estágios; Noul de existência) |
| 99 | §1.3.1 **P3** (o `state` domina o custo; fraseado é contrato) |
| 7, 8, 17, 102, 106, 107 | §1.3.1 **P5** (normalização por `n-1`; zona morta; composição ponderada; Score de 3 níveis; `max`/`min`) |
| 100 | §1.3.1 **P1/P5** (omissão via `stated`; confiança do julgamento mais fraco) |
| 11, 16, 94, 95, 103 | §1.3.1 (números: piso 0,60 / 0,85 / topo 0,60 / banda 0,30–0,70 / dict único) + §1.6 I-3 |
| 101, 111 | §1.3.1 **P6** (duas fases; texto ignorável; genérico quando incerto) |
| 12, 13, 18 | §1.3.1 **P4** (rotear por incerteza e por intenção; categoria oficial "Harness Engineering") |
| 10 | §1.3.1 (avaliação: calibração por faixa) + §4 **S-004** |
| 14, 15, 96 | §1.3.1 + §3 **S-003** (quatro padrões oficiais; fan-out; 1 requisição por decisão) |
| 91 | §3 **S-003** (catálogo único revisável; argmax quando só importa a melhor opção) |
| 89 | §3 **E-016** + §6 **R-007** (preço por token de entrada; pin de versão) |
| 92 | §6 **R-004** (DPA/não-treino; ZDR só enterprise; minimização do `state`) |
| 2, 21, 23, 24, 26, 28, 30, 31, 33, 35, 36, 37, 50, 53, 54, 55, 58, 59, 60, 63, 64, 65, 66, 67, 68, 69, 70, 71, 73, 76, 77, 78, 79, 80, 81, 83, 84, 85, 86, 87, 88 | §12.5 (contrato de request/response, tipos serde, defaults e evidência por chamada) + §4 **S-002** |
| 32, 34, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 52, 61, 62, 90 | §12.5 (mapa de erros e divergência deliberada de retry) + §6 **R-003** |

**Não aplicáveis (14), com motivo no ledger `plan/docs-review.md`:** 19, 22, 25, 27, 29, 51, 56, 57, 72, 74, 75, 82, 105, 110.

### 12.7 Sobrescrita do dono — defaults ligados (2026-09-17)

O dono determinou "deixe tudo ativo por padrão" para o Jev. Isso **sobrescreve o invariante I-1** (flag default OFF) deste plano congelado e antecipa a fase 2 de S-005 sem o conselho `full` que o §4 S-000 item 5 exigia. O que mudou e o que não mudou está registrado em `plan/plan-report.json` → `extensions.owner_overrides`, e a chave foi exportada em `~/.zshrc` (600) com exclusão do ambiente de subprocessos via `[shell_environment_policy]`. Residuais aceitos: gate de conselho não executado, amostra de calibração pequena, alavancas P1–P6 ligadas mas ainda não fiadas nos seams vivos, e o segredo em texto plano no perfil do shell.

### 12.8 Catálogo completo (23 itens) ligado no caminho vivo (2026-09-18)

Implementação do catálogo do `todo.md` §2: as 23 decisões (A1–A6, B1–B6, C1–C7, D1–D4) passaram a ter ponto de
chamada vivo no harness, cada uma atrás da **sua própria flag**, através do gate único (`crate::jev::ask_item`:
interruptor mestre + flag do item + credencial + uma tentativa com orçamento de 4 s + fail-defer). As sete que
faltavam — B2 (modelo/effort, só rebaixa), B3 (tipo de subagente desconhecido), B5 (skill anunciada), C1/C3
(parada prematura / pedido cumprido, no caminho de preguiça), D1 (recorte da compaction) e D3 (recuperação
pós-compaction) — foram fiadas depois das dezesseis anteriores.

Evidência desta rodada (em `{SCRATCH}`):

| Passo | Comando | Resultado |
|---|---|---|
| Fiação estrutural | `wiring_check.py` | `WIRING: PASS (23/23 catalogue items; 2/2 live points H1/H2)` |
| Cobertura do catálogo | `todo-coverage.py` | `TODO COVERAGE: PASS (25 items, each with locator + test)` |
| Unidades | `cargo test -p xai-grok-workspace --lib jev::` · `-p xai-grok-config-types -p xai-grok-env` | 90 · 75+8 passed, 0 failed |
| Compilação | `cargo check -p xai-grok-workspace -p xai-grok-shell -p xai-grok-pager -p xai-grok-pager-bin` | exit 0 |
| Execução real #1 | TUI, `--permission-mode auto`, `GROK_LOG_JEV=1` | 24 linhas novas, 8 levers do catálogo, rodapé `auto · jev` |
| Execução real #2 | TUI, `--always-approve`, `GROK_LOG_JEV=1` | 17 linhas novas; B1/B6/P1 com a **mesma classe** da execução #1, rodapé `always-approve · jev·veto` |
| Interruptor mestre | TUI, `GROK_JEV=0`, um turno inteiro | 0 linhas novas, rodapé `jev:off` |
| API real (gate) | `cargo test --test jev_live -- --ignored` | 4/4 verdes: corpus de permissão 0 falso-allow / 0 falso-block; A1/A3 e D2 com respostas tipadas e `usage > 0`; 0 descartes indevidos |

Os itens **B2** e **C6** continuam **OFF** por decisão do próprio gate (§ regra "item cujo gate não passa fica off
com o número registrado"), já fiados e testados.
