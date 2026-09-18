# Ledger de revisão — varredura do menu de docs.typesafe.ai

**Objetivo:** para cada item do índice canônico (https://docs.typesafe.ai/llms.txt), registrar a lição aprendida, um veredito (aplicável / não aplicável / falha de leitura) e — quando aplicável — onde o plano foi ajustado.

**Snapshot do índice (linha de base):** capturado de `llms.txt` em 2026-09-17. Total: **111 itens** = 18 cookbooks + 67 páginas de referência de SDK (Python/JS) + 26 páginas de introduction/concepts/primitives/patterns/demos/models/api/confidence/agent-skill/legal/jaggedness. Lista item-a-item com URLs: `plan/assets/docs-review-index.md` (e `{SCRATCH}/docs-review/index.md`).

**Método:** 16 itens lidos diretamente pelo planejador (`{SCRATCH}/docs-review/notes-self.md`); 94 itens lidos por cinco leitores read-only dedicados, com lição + veredito + citação literal por página. **Falhas de leitura: nenhuma** (111/111 obtidos).

**Regra de veredito:** `APLICÁVEL(<onde>)` = gerou ajuste concreto no plano; `NÃO APLICÁVEL <motivo>` = sem ajuste aplicável (stub, duplicata, ou específico de linguagem/domínio que não vale para o harness Rust).

**Mapa das localizações citadas nos ajustes:** `§A cliente` e `§B config/flag` = linhas **A** (cliente Jev) e **B** (config + flag + higiene de credencial) da tabela de escopo em `plan.md` §1.2; `§1.3.1` = escada de maximização; `§1.4` = regra de elegibilidade; `§1.5` = não-objetivos; `§1.6` = invariantes I-1..I-10; `§6` = riscos; `§3` = evidências; `§4 S-00N` = passos; `§12`/`§12.4`/`§12.5` = changelog da varredura, reconciliação e anexo de contrato; `extensions.docs_scan` = espelho de máquina no `plan-report.json`.

---

## Fundamentos e conceitos (itens 1–20)

### 1. Introduction (https://docs.typesafe.ai/introduction.md)
- Lição: Jev avalia perguntas tipadas contra um `state` e devolve resultados estruturados (choice/probabilities/confidence; score; noul), sem geração de texto; três primitivas misturáveis na mesma chamada, avaliadas em paralelo e em isolamento; perguntas atômicas compostas em código.
- Veredito: APLICÁVEL(§1.4; §1.5 no-go; §1.6 I-9)
- Ajuste: proibição de geração mantida no no-go; novo invariante **I-9** registra que o Jev devolve tipo+probabilidade+confidence e nunca gera texto, com escalada como fallback do seam.

### 2. Quick start (https://docs.typesafe.ai/introduction/quickstart.md)
- Lição: `POST https://api.typesafe.ai/v1/systemone` com bearer; `model: jev-latest`; corpo `{state, model, questions}`; resposta `{model, answers, usage{input_tokens,output_tokens}}`; SDK lê `TYPESAFE_API_KEY`.
- Veredito: APLICÁVEL(§A cliente; §4 S-002)
- Ajuste: contrato de request/response e nome de env fixados (espelho em `extensions.docs_scan.contract`).

### 3. System One (https://docs.typesafe.ai/concepts/system-one.md)
- Lição: devolve decisões tipadas e probabilidades calibradas, nunca texto; aceita só texto (string/JSON/array), sem imagem/áudio; inclui `confidence` para decidir quando agir e quando escalar para pessoa ou modelo de raciocínio.
- Veredito: APLICÁVEL(§1.6 I-9)
- Ajuste: I-9 (adicionado) + nota de que o insumo é texto puro (§12).
- Citação-chave: "It returns typed decisions and probabilities rather than generated text."

### 4. State (https://docs.typesafe.ai/concepts/state.md)
- Lição: `state` aceita string, objeto JSON ou array — apenas texto; separar conteúdo de perguntas; preferir objeto com campos nomeados.
- Veredito: APLICÁVEL(§1.3.1 P1/P2; §1.4)
- Ajuste: reforço de "state por allowlist tipada; nunca conteúdo binário" (§12).

### 5. Primitives (Questions) (https://docs.typesafe.ai/primitives.md)
- Lição: IDs de pergunta não vão ao modelo; perguntas são independentes; referenciar campos do state com caminhos em backticks; orçamento ~32.000 tokens (~150.000 caracteres) compartilhado entre state e perguntas.
- Veredito: APLICÁVEL(§3 S-003; §1.3.1)
- Ajuste: catálogo exige caminhos em backticks e orçamento de estado por chamada (§12).

### 6. Choice (https://docs.typesafe.ai/primitives/choice.md)
- Lição: até **255 opções**; o doc manda enviar a **lista completa em `criteria`** (cada opção custa poucos tokens), não uma shortlist; usar `other` quando a lista pode não cobrir tudo.
- Veredito: APLICÁVEL(§1.3.1 P1)
- Ajuste: P1 refinado — o catálogo de famílias vai nas `criteria` (barato, teto 255), **nunca** no `state` (a parte cara) (§12).
- Citação-chave: "give the model the full list of teams, categories, or products rather than a shortlist"

### 7. Score (https://docs.typesafe.ai/primitives/score.md)
- Lição: rubrica de 2–10 níveis; devolve média ponderada (pode cair entre níveis); distribuições diferentes geram o mesmo score; normalizar `score/(len(criteria)-1)` antes de ponderar; níveis só numéricos degradam (0.57/conf 0.35 vs 0.0/conf 1.0).
- Veredito: APLICÁVEL(§1.3.1 P5)
- Ajuste: P5 exige níveis descritivos e normalização por `n-1` antes de qualquer limiar; proibido comparar score bruto entre escalas de tamanhos diferentes (§12).

### 8. Noul (https://docs.typesafe.ai/primitives/noul.md)
- Lição: devolve só `noul` (0–1), **sem `confidence`**; o código corta num limiar; frasear de modo que "sim" = probabilidade alta; 0.5 é ambiguidade, não "médio" (para espectro, usar Score).
- Veredito: APLICÁVEL(§1.3.1 P5)
- Ajuste: P5 exige limiar explícito **com zona morta** (≥0.9 allow / ≤0.4 deny / entre → fallback LLM) e fraseado com "sim" = permitido (§12).

### 9. Advanced: structure (https://docs.typesafe.ai/primitives/advanced.md)
- Lição: `instructions`, opções de Choice, níveis de Score e criteria de Noul aceitam JSON estruturado; classificação hierárquica caminha a taxonomia nível a nível (opção = filho, valor = subárvore); subárvores grandes devem ser trimadas para filhos diretos + amostra de folhas.
- Veredito: APLICÁVEL(§1.3.1 P1)
- Ajuste: P1 deve representar famílias como **Choice de taxonomia com subárvore trimada**, não o catálogo serializado no state (§12).
- Citação-chave: "If a branch is too large, trim the value to its direct children and a sample of leaves."

### 10. AI primer (https://docs.typesafe.ai/introduction/machine-learning-primer.md)
- Lição: calibração é propriedade **de grupo**: probabilidade 0.2 deve ocorrer ~20% das vezes, 0.8 ~80%, 1.0 100%; nada garante a resposta individual; RLHF premia sycophancy, por isso a decisão é o alvo de treino.
- Veredito: APLICÁVEL(§3 evidências; §4 S-004)
- Ajuste: o gate de avaliação passa a medir **calibração por faixa** (bin 0.8 → ~80% de acerto) como critério, além de acurácia (§12; espelho em `extensions.docs_scan.eval_gate`).
- Citação-chave: "Outcomes assigned a probability of 0.8 should occur about 80% of the time."

### 11. Confidence (https://docs.typesafe.ai/confidence.md)
- Lição: `confidence` deriva da distribuição; três faixas (alta = agir, média = cautela, baixa = não agir); limiares escalam com o risco; probabilidades completas disponíveis para medida própria.
- Veredito: APLICÁVEL(§1.6 I-3/I-10)
- Ajuste: pisos por classe de risco agora têm números e a banda de revisão virou invariante **I-10** (§12).

### 12. How to build with TypeSafe (https://docs.typesafe.ai/concepts/how-to-build-with-system-one.md)
- Lição: manter fluxo de controle/regras/efeitos no código; decompor state e perguntas; combinar respostas em código (somas ponderadas); **rotear por incerteza** (escalar para humano ou modelo mais caro); decompor não aumenta round-trips.
- Veredito: APLICÁVEL(§2 tese; §1.3.1 P4)
- Ajuste: "rotear por incerteza" formalizado como mecanismo de escalada Jev → LLM (§12).

### 13. Example use cases (https://docs.typesafe.ai/concepts/use-case-map.md)
- Lição: lista casos oficiais, incluindo **"Harness Engineering"** (routing de modelo, retrieval semântico, detecção de erro/guardrails, classificação de traces) e **"Model routing"**; cita 150 ms e "100x cheaper".
- Veredito: APLICÁVEL(§1.3.1 P4; §3)
- Ajuste: P4 ganhou a validação externa da categoria "Harness Engineering" (e mantém a ressalva de que P4 economiza dinheiro, não tokens) (§12).
- Citação-chave: "Use Jev queries to make your harness smarter - model routing, semantic context retrieval, LLM error detection"

### 14. Patterns (https://docs.typesafe.ai/patterns.md)
- Lição: existem apenas quatro padrões oficiais — Speculative Fan-Out, Confidence-Gated Routing, Composite Scoring e Intent Routing.
- Veredito: APLICÁVEL(§backlog / §1.3.1)
- Ajuste: backlog alinhado aos nomes oficiais (nosso v1 é confidence-gated routing; fan-out já usado) (§12).

### 15. Speculative fan-out (https://docs.typesafe.ai/patterns/fan-out.md)
- Lição: enviar todas as perguntas (inclusive especulativas) numa chamada; perguntas extras quase não custam latência; o código ignora as irrelevantes.
- Veredito: APLICÁVEL(§3 S-003)
- Ajuste: bateria especulativa única por decisão mantida e documentada (§12).

### 16. Confidence-gated routing (https://docs.typesafe.ai/patterns/confidence-routing.md)
- Lição: números concretos — **piso de 0.6** → rota a humano; ação de baixo risco age com 0.6; ação de alto risco (aprovar transferência) exige **>0.85**, senão confirmar antes.
- Veredito: APLICÁVEL(§1.3.1 P5; §1.6 I-3)
- Ajuste: adotados dois limiares no seam: piso **0.60** (abaixo → fallback LLM/incumbente) e **0.85** para allow de ação sensível; nunca um limiar único (§12).
- Citação-chave: "Below 0.6 confidence on any action, route to a human"

### 17. Composite scoring (https://docs.typesafe.ai/patterns/composite-scoring.md)
- Lição: quebrar o julgamento em Scores atômicos normalizados (ex. `score/4`) e combinar com **pesos explícitos no código**; pesos visíveis permitem ajustar quando o ranking decepciona.
- Veredito: APLICÁVEL(§1.3.1 P5)
- Ajuste: P5 modela o risco da tool call como soma ponderada de Scores atômicos normalizados (destrutividade, escopo, reversibilidade), não um Score único (§12).

### 18. Intent routing (https://docs.typesafe.ai/patterns/intent-routing.md)
- Lição: classificar primeiro e rotear para código determinístico, LLM especialista ou humano; `confidence < 0.5` → humano; score de complexidade decide LLM vs humano.
- Veredito: APLICÁVEL(§1.3.1 P4)
- Ajuste: P4 especificado como Choice de intenção + Score de complexidade com gate de confiança (§12).

### 19. Demos (https://docs.typesafe.ai/demos.md)
- Lição: página-índice; só existe um demo público; nenhum requisito, limite ou número próprio.
- Veredito: NÃO APLICÁVEL — índice sem conteúdo técnico acionável.

### 20. Smart home assistant demo (https://docs.typesafe.ai/demos/smart-home.md)
- Lição: demonstra fan-out especulativo (sequencial é "mais lento e mais caro") e o pareamento TypeSafe+LLM: Noul detecta pedido composto → LLM divide; pedido conversacional → LLM responde; a latência do Jev é negligenciável frente à do LLM.
- Veredito: APLICÁVEL(§1.6 I-8/I-9)
- Ajuste: registrado como precedente de que o caminho LLM permanece quando o Jev classifica "não-comando" (§12).
- Citação-chave: "the system calls an LLM to generate a freeform response"

---

## SDK Python (itens 21–35) — referência para o cliente Rust hand-written

### 21. Client SDKs (https://docs.typesafe.ai/sdk.md)
- Lição: retry é responsabilidade do SDK ("handle retries automatically with their default retry policy"), não requisito do servidor; existe o caminho de chamar a HTTP API diretamente de qualquer linguagem.
- Veredito: APLICÁVEL(§A cliente; §6 R-003)
- Ajuste: registrado que cliente Rust sem SDK é suportado e que a nossa política **sem retry** no caminho quente é escolha deliberada, não lacuna (§12).

### 22. TypeSafe Python SDK (https://docs.typesafe.ai/sdk/python.md)
- Lição: `TYPESAFE_API_KEY` obrigatória; `system_one(state, questions)`; respostas em `response.nouls[nome].noul`, `.choices[nome].choice`, `.scores[nome].score`.
- Veredito: NÃO APLICÁVEL — Python-only; shapes cobertos pelas páginas de types.

### 23. Usage (https://docs.typesafe.ai/sdk/python/usage.md)
- Lição: envs `TYPESAFE_API_KEY`, `TYPESAFE_BASE_URL` (default `https://api.typesafe.ai`), `TYPESAFE_DEFAULT_MODEL` (default `jev-latest`), `TYPESAFE_LOG_LEVEL`; **headers secretos são redigidos, corpos NÃO**; `extra_body` é merge raso last-write-wins; answer kind desconhecido é logado e ignorado.
- Veredito: APLICÁVEL(§B config/flag; §6 R-005)
- Ajuste: espelhar nomes `TYPESAFE_*` atrás de `GROK_JEV`; divergência deliberada: answer kind desconhecido vira `Invalid` (não ignorado); proibido logar corpo (§12).
- Citação-chave: "Request and response bodies are **not** redacted."

### 24. Changelog (Python) (https://docs.typesafe.ai/sdk/python/changelog.md)
- Lição: v0.6.0 (2026-09-15) quebra `Score.criteria` para **sequência ordenada** (não dict por inteiros); v0.5.7 foi o release inicial.
- Veredito: APLICÁVEL(§A cliente)
- Ajuste: contrato fixado na HTTP API: `Score.criteria` serializado como **array JSON ordenado** (§12).

### 25. API reference (Python) (https://docs.typesafe.ai/sdk/python/api.md)
- Lição: página-índice (links para clients/types/retries/exceptions/constants); sem conteúdo técnico próprio.
- Veredito: NÃO APLICÁVEL — índice de navegação.

### 26. Asynchronous client (https://docs.typesafe.ai/sdk/python/api/clients/async/client.md)
- Lição: opções explícitas vencem env; valores de env vazios/só-espaço são ignorados; `system_one` rejeita questions vazias e `Score.criteria` vazio; `extra_body` sobrescreve `state`/`model`/`questions` (last-write-wins).
- Veredito: APLICÁVEL(§A cliente; §B)
- Ajuste: validar previamente questions e rubrica vazias como `Invalid`; `extra_body` não pode sobrescrever `state`/`questions` no nosso cliente (§12).

### 27. Models resource (async) (https://docs.typesafe.ai/sdk/python/api/clients/async/models.md)
- Lição: `models.list()` devolve name/description/release_date; headers de auth/SDK/Accept não são sobreponíveis.
- Veredito: NÃO APLICÁVEL — o harness só usa `system_one`.

### 28. Synchronous client (https://docs.typesafe.ai/sdk/python/api/clients/sync/client.md)
- Lição: espelho síncrono com as mesmas validações; `close()` libera recursos de rede e fecha o HTTP client subjacente.
- Veredito: APLICÁVEL(§A cliente)
- Ajuste: exigir close determinístico do pool HTTP do cliente Rust ao desligar o caminho (§12).

### 29. Models resource (sync) (https://docs.typesafe.ai/sdk/python/api/clients/sync/models.md)
- Lição: duplicata síncrona do recurso Models (retry/timeout/headers por chamada; auth/SDK/Accept protegidos).
- Veredito: NÃO APLICÁVEL — duplicata não usada.

### 30. Questions (https://docs.typesafe.ai/sdk/python/api/types/questions.md)
- Lição: `state` nunca None mas valores internos podem ser; dicts usam `type` `"noul"|"choice"|"score"` com campos extras permitidos; `Choice.criteria` = mapping label→descrição|None; `Score.criteria` = sequência não vazia, uma descrição por score desde zero.
- Veredito: APLICÁVEL(§A cliente)
- Ajuste: aceitar nulls aninhados no state e campos extras nos três tipos de pergunta (§12).

### 31. Answers and responses (https://docs.typesafe.ai/sdk/python/api/types/responses.md)
- Lição: resposta expõe `request_id` (header `x-typesafe-request-id`), `model`, `usage` (input/output tokens `int|None`) e `answers` por nome; `ScoreAnswer` traz `score` (pode cair entre níveis), `confidence`, `legend` e `probabilities` por inteiro.
- Veredito: APLICÁVEL(§3 evidências)
- Ajuste: persistir por chamada request_id, model, tokens, confidence e probabilities como evidência do caminho Jev (§12; espelho em `extensions.docs_scan.evidence`).

### 32. Retries (https://docs.typesafe.ai/sdk/python/api/retries.md)
- Lição: defaults do SDK — `max_retries=2`, backoff 0.5 s dobrando até 5.0 s, jitter 0.25, statuses `{408,429,500–599}`, respeita `Retry-After`/`retry-after-ms`, timeout 30 s de orçamento total por chamada.
- Veredito: APLICÁVEL(§6 R-003)
- Ajuste: declarado que o nosso single-attempt+deadline equivale a `RetryPolicy(max_retries=0)`; 408/429→`RateLimited`, 5xx→`Unavailable`, sem reenvio no caminho quente (§12).
- Citação-chave: "Maximum retries after the initial attempt; 0 disables retries."

### 33. Common types (https://docs.typesafe.ai/sdk/python/api/types/common.md)
- Lição: `JSONValue`/`JSONContent` = string, objeto ou sequência aninháveis contendo `None` — é o que `state` e descrições aceitam.
- Veredito: APLICÁVEL(§A cliente)
- Ajuste: tipar o payload Rust como string | objeto | array recursivo com nulls (§12).

### 34. Exceptions (https://docs.typesafe.ai/sdk/python/api/exceptions.md)
- Lição: `TypeSafeAPIError` carrega `status`, `body`, `headers`, `endpoint` e `request_id`; subclasses para 400/401/403/404/422, 429 (com `retry_after_ms`) e 5xx; erros de conexão/timeout; e **response validation error** para 200 estruturalmente inválido, com `field_path`.
- Veredito: APLICÁVEL(§A cliente)
- Ajuste: mapa de erros ampliado — 400/401/403/404/422 e corpo inválido→`Invalid` (com `field_path`), 429→`RateLimited(retry_after_ms)`, 5xx→`Unavailable`, rede→`Transport`, timeout→`Timeout` (§12).
- Citação-chave: "Dotted path to the offending field, such as `answers.tone.confidence`."

### 35. Constants (https://docs.typesafe.ai/sdk/python/api/constants.md)
- Lição: envs `TYPESAFE_*`; defaults `https://api.typesafe.ai`, `jev-latest` e `DEFAULT_TIMEOUT = 10.0` por operação HTTP.
- Veredito: APLICÁVEL(§B config/flag)
- Ajuste: deadline default de **10 s por operação** e modelo default `jev-latest` adotados como ponto de partida (§12).

---

## SDK JavaScript (itens 36–88)

### 36. JavaScript SDK (https://docs.typesafe.ai/sdk/javascript.md)
- Lição: pacote `@typesafe-ai/sdk` (Node 20+); credencial por `TYPESAFE_API_KEY`; `client.systemOne({state, questions})`; resposta em `response.answers.<nome>.<tipo>`.
- Veredito: APLICÁVEL(§A cliente)
- Ajuste: payload `{state, questions}` e nome de env espelhados no cliente Rust (§12).

### 37. Changelog (JS) (https://docs.typesafe.ai/sdk/javascript/changelog.md)
- Lição: v0.6.0 (2026-09-15) quebra `Score.criteria` para sequência ordenada; v0.5.7 release inicial.
- Veredito: APLICÁVEL(§3 evidências)
- Ajuste: registrado "espelha a superfície HTTP da v0.6.0" como referência contratual (§12).

### 38. API reference (JS) (https://docs.typesafe.ai/sdk/javascript/api.md)
- Lição: índice que fixa o mapa de erros: 400/401/403/404/422/429/5xx + `APIConnectionError`, `APITimeoutError`, `APIUserAbortError`; **não existe classe para 529**.
- Veredito: APLICÁVEL(§A cliente)
- Ajuste: 529 registrado como mapeamento **nosso** (→`Unavailable`/`RateLimited`) e não como contrato do vendor (§12).

### 39. Class: APIConnectionError (…/classes/APIConnectionError.md)
- Lição: falha de entrega do request/response-body (DNS, TLS, conexão fechada); base de `APITimeoutError` (timeout é subtipo de conexão em JS).
- Veredito: APLICÁVEL(§A cliente)
- Ajuste: socket/TLS→`Transport`; documentada a divergência (nós separamos `Timeout` de `Transport`) (§12).

### 40. Class: APIError (…/classes/APIError.md)
- Lição: campos `status`, `body` (JSON/texto/undefined), `headers`, `requestId` de `x-typesafe-request-id`; `static fromResponse(...)`.
- Veredito: APLICÁVEL(§A cliente)
- Ajuste: guardar `x-typesafe-request-id` no struct de erro Rust (nunca logar header de auth) (§12).

### 41. Class: APIPromise<T> (…/classes/APIPromise.md)
- Lição: non-2xx rejeita com `APIError` (inclusive via `asResponse()`); o SDK **bufferiza o corpo inteiro sob o timeout do request**.
- Veredito: APLICÁVEL(§A cliente)
- Ajuste: a deadline do cliente Rust deve cobrir a **leitura do corpo completo**, não só os headers (§12).
- Citação-chave: "SDK requests buffer the full body under the request timeout before handoff"

### 42. Class: APITimeoutError (…/classes/APITimeoutError.md)
- Lição: "the full response did not arrive within the timeout"; `readonly timeoutMs`; herda de `APIConnectionError`.
- Veredito: APLICÁVEL(§6 R-003)
- Ajuste: registrada a divergência: nossa taxonomia separa `Timeout` e o caminho quente **não** retenta (a JS retenta) (§12).

### 43. Class: APIUserAbortError (…/classes/APIUserAbortError.md)
- Lição: cancelamento do chamador via `AbortSignal`; estende `TypeSafeError` (sem status).
- Veredito: APLICÁVEL(§A cliente)
- Ajuste: cancelamento local tratado como estado próprio, sem retry nem fallback, distinto de `Timeout` (§12).

### 44. Class: AuthenticationError (…/classes/AuthenticationError.md)
- Lição: exclusivo de HTTP 401 (autenticação falhou).
- Veredito: APLICÁVEL(§A cliente)
- Ajuste: 401→`Invalid` sem retry; nunca incluir a API key em erro/log (§12).

### 45. Class: BadRequestError (…/classes/BadRequestError.md)
- Lição: exclusivo de HTTP 400 ("the request is invalid"); sem propriedades adicionais.
- Veredito: APLICÁVEL(§A cliente)
- Ajuste: 400→`Invalid` não-retryável, preservando o body para diagnóstico (sem segredos) (§12).

### 46. Class: InternalServerError (…/classes/InternalServerError.md)
- Lição: classe única para todo 5xx; **não há classe dedicada a 529**.
- Veredito: APLICÁVEL(§A cliente)
- Ajuste: 5xx→`Unavailable`; caminho quente faz 1 tentativa e cai no fallback LLM (§12).

### 47. Class: NotFoundError (…/classes/NotFoundError.md)
- Lição: HTTP 404 = recurso inexistente (modelo/endpoint errado), não indisponibilidade.
- Veredito: APLICÁVEL(§B config/flag)
- Ajuste: 404→`Invalid` sem retry, surfaced como erro de configuração de endpoint/modelo (§12).

### 48. Class: PermissionDeniedError (…/classes/PermissionDeniedError.md)
- Lição: HTTP 403, distinto de 401: autenticação passou, autorização não.
- Veredito: APLICÁVEL(§A cliente; §6)
- Ajuste: 403→`Invalid` sem retry; classificado como escopo/plano da conta, não credencial inválida (§12).

### 49. Class: RateLimitError (…/classes/RateLimitError.md)
- Lição: HTTP 429 com `readonly retryAfterMs` (indefinido quando ausente/inválido).
- Veredito: APLICÁVEL(§A cliente)
- Ajuste: 429→`RateLimited` carregando retry-after em ms; caminho quente mantém 1 tentativa e cai no fallback (§12).

### 50. Class: TypeSafeClient (…/classes/TypeSafeClient.md)
- Lição: precedência **opções explícitas > env > defaults**, env vazia/whitespace ignorada; knobs `baseURL`, `defaultModel`, `timeout` (por tentativa), `retry`, `defaultHeaders`, `logLevel`, `fetch`; `systemOne` aceita `RequestOptions` por chamada.
- Veredito: APLICÁVEL(§B config/flag)
- Ajuste: precedência espelhada (`flag/env > default`), `GROK_JEV` OFF por padrão, env vazia ignorada, timeout por tentativa (§12).

### 51. Class: TypeSafeError (…/classes/TypeSafeError.md)
- Lição: apenas base class JS (`extends Error`, mensagem obrigatória, `ErrorOptions`), sem status/headers/defaults.
- Veredito: NÃO APLICÁVEL — hierarquia de runtime JS; nossa taxonomia é enum Rust.

### 52. Class: UnprocessableEntityError (…/classes/UnprocessableEntityError.md)
- Lição: HTTP 422 = falha de validação do pedido, separado semanticamente do 400.
- Veredito: APLICÁVEL(§A cliente)
- Ajuste: 422→`Invalid` sem retry, com requestId + body para evidência (§12).

### 53. Interface: ChoiceQuestion<T> (…/interfaces/ChoiceQuestion.md)
- Lição: `type:"choice"`; `criteria: T` obrigatório (labels→EntryType); `instructions?`.
- Veredito: APLICÁVEL(§A cliente/tipos) — serde: tag `"choice"`, criteria `Map<String, Value>`.

### 54. Interface: ChoiceResponse<T> (…/interfaces/ChoiceResponse.md)
- Lição: `choice: string`, `confidence: number`, `probabilities` label→number, `type:"choice"`.
- Veredito: APLICÁVEL(§A cliente/tipos) — answer com `probabilities: HashMap<String,f64>`.

### 55. Interface: Logger (…/interfaces/Logger.md)
- Lição: quatro níveis `debug/info/warn/error(message, ...args)`; compatível com console.
- Veredito: APLICÁVEL(§B) — logger do harness expõe esses quatro níveis, sem `trace`.

### 56. Interface: ModelCard (…/interfaces/ModelCard.md)
- Lição: `name`, `description`, `release_date` (metadados de listagem).
- Veredito: NÃO APLICÁVEL — o harness não lista modelos.

### 57. Interface: Models (…/interfaces/Models.md)
- Lição: recurso `list(options?) → ModelCard[]`.
- Veredito: NÃO APLICÁVEL — sem catálogo no harness.

### 58. Interface: NoulQuestion (…/interfaces/NoulQuestion.md)
- Lição: `type:"noul"`; `criteria? = {false?, true?}`; `instructions?`.
- Veredito: APLICÁVEL(§A cliente/tipos) — `Option<Value>` serializado só quando presente.

### 59. Interface: NoulResponse (…/interfaces/NoulResponse.md)
- Lição: `noul: number` (0–1), `type:"noul"` — o campo se chama `noul`, não `probability`/`confidence`.
- Veredito: APLICÁVEL(§A cliente/tipos) — nome do campo fixado no serde.

### 60. Interface: Questions (…/interfaces/Questions.md)
- Lição: índice `[name: string]: Question` — os nomes viram as chaves das respostas.
- Veredito: APLICÁVEL(§A cliente/tipos) — `questions: HashMap<String, Question>`.

### 61. Interface: RequestOptions (…/interfaces/RequestOptions.md)
- Lição: por chamada: `headers`, `retry?`, `signal?`, `timeout` por tentativa — **sem orçamento total**.
- Veredito: APLICÁVEL(§3 evidências) — no caminho quente o retry é desabilitado por override explícito (§12).

### 62. Interface: RetryPolicy (…/interfaces/RetryPolicy.md)
- Lição: default `maxRetries:2`; retry em 408/429/500–599; backoff 500 ms→5000 ms; jitter 0.25; respeita `Retry-After`/`retry-after-ms`; retenta **também** erros de conexão e de timeout por padrão (`apiConnectionError`/`apiTimeoutError` = true) e limita o atraso do servidor em `maxRetryAfterMs` (default 60000).
- Veredito: APLICÁVEL(§3 evidências; §6 R-003) — evidência de que o default retenta 429/5xx; fixamos `maxRetries=0` no caminho quente (§12).

### 63. Interface: ScoreQuestion<T> (…/interfaces/ScoreQuestion.md)
- Lição: `type:"score"`; `criteria: T` (rubrica ordenada) obrigatório.
- Veredito: APLICÁVEL(§A cliente/tipos) — criteria serializado como array JSON.

### 64. Interface: ScoreResponse<T> (…/interfaces/ScoreResponse.md)
- Lição: `score` (pode ser fracionário), `confidence`, `legend` por score, `probabilities` por score.
- Veredito: APLICÁVEL(§A cliente/tipos) — `legend`/`probabilities` com chaves string numéricas.

### 65. Interface: SystemOneRequest<Q> (…/interfaces/SystemOneRequest.md)
- Lição: `state`, `questions` não-vazio, `model?`; **campos extras são encaminhados, inclusive null**.
- Veredito: APLICÁVEL(§A cliente/tipos) — `#[serde(flatten)]` para preservar extras/null.

### 66. Interface: SystemOneRequestPayload (…/interfaces/SystemOneRequestPayload.md)
- Lição: corpo do `POST /v1/systemone` com `model: string` **resolvido** (não-opcional) antes do envio.
- Veredito: APLICÁVEL(§A cliente) — resolver o modelo (explícito ou default) ao montar o body (§12).

### 67. Interface: SystemOneResult<Q> (…/interfaces/SystemOneResult.md)
- Lição: `answers` keyed por nome, `model`, `usage` — todos obrigatórios na resposta.
- Veredito: APLICÁVEL(§A cliente/tipos) — envelope com os três campos obrigatórios.

### 68. Interface: TypeSafeClientConfig (…/interfaces/TypeSafeClientConfig.md)
- Lição: `baseURL` default `https://api.typesafe.ai`; `timeout` **10000 ms por tentativa**; `logger` console prefixado; `logLevel` default `warn`; credenciais conhecidas são redigidas, **corpos não**.
- Veredito: APLICÁVEL(§B config/flag) — espelhar baseURL/timeout/defaultModel; nunca logar corpo (§12).

### 69. Interface: Usage (…/interfaces/Usage.md)
- Lição: `input_tokens` e `output_tokens` (snake_case).
- Veredito: APLICÁVEL(§A cliente/tipos) — rename snake_case no struct Usage.

### 70. Interface: WithResponse<T> (…/interfaces/WithResponse.md)
- Lição: `data`, `response`, `requestId` de `x-typesafe-request-id` ou undefined.
- Veredito: APLICÁVEL(§3 evidências) — capturar o request id para correlacionar evidências/erros (§12).

### 71. Type Alias: ChoiceCriteria (…/type-aliases/ChoiceCriteria.md)
- Lição: `[label: string]: EntryType` — labels livres; descrição pode ser null.
- Veredito: APLICÁVEL(§A cliente/tipos) — mapa string→Value, sem conjunto fixo de labels.

### 72. Type Alias: Description (…/type-aliases/Description.md)
- Lição: alias trivial de EntryType; `null` deixa o label sem descrição.
- Veredito: NÃO APLICÁVEL — stub/alias sem conteúdo próprio.

### 73. Type Alias: EntryType (…/type-aliases/EntryType.md)
- Lição: `string | objeto JSON | array JSON | null` para state, instructions e criteria.
- Veredito: APLICÁVEL(§A cliente/tipos) — modelar como `serde_json::Value` aceitando null.

### 74. Type Alias: EnvVar (…/type-aliases/EnvVar.md)
- Lição: tipo TS derivado de `ENV`; não existe em runtime.
- Veredito: NÃO APLICÁVEL — puro tipo de linguagem.

### 75. Type Alias: Fetch (…/type-aliases/Fetch.md)
- Lição: assinatura compatível com fetch global, injetável.
- Veredito: NÃO APLICÁVEL — o harness Rust tem cliente HTTP próprio.

### 76. Type Alias: JsonValue (…/type-aliases/JsonValue.md)
- Lição: string|number|boolean|null|array|objeto recursivo.
- Veredito: APLICÁVEL(§A cliente/tipos) — reutilizar `serde_json::Value`.

### 77. Type Alias: LogLevel (…/type-aliases/LogLevel.md)
- Lição: `"debug"|"info"|"warn"|"error"|"off"`.
- Veredito: APLICÁVEL(§B) — parser aceita exatamente esses cinco valores, default `warn`.

### 78. Type Alias: Question (…/type-aliases/Question.md)
- Lição: união tagueada `NoulQuestion | ScoreQuestion | ChoiceQuestion` pelo campo `type`.
- Veredito: APLICÁVEL(§A cliente/tipos) — enum serde `#[serde(tag="type")]`.

### 79. Type Alias: ResultFor<T> (…/type-aliases/ResultFor.md)
- Lição: pergunta→resposta correspondente (noul/score/choice).
- Veredito: APLICÁVEL(§A cliente/tipos) — desserializar por nome + tag de tipo.

### 80. Type Alias: ScoreCriteria (…/type-aliases/ScoreCriteria.md)
- Lição: tupla readonly com **≥2** descrições, indexadas de zero; entradas podem ser null.
- Veredito: APLICÁVEL(§A cliente/tipos) — validar em construção ≥2 níveis.

### 81. Type Alias: ScoreLegend (…/type-aliases/ScoreLegend.md)
- Lição: `legend` mapeia score→descrição da rubrica.
- Veredito: APLICÁVEL(§A cliente/tipos) — parsear como mapa chave-numérica→Value.

### 82. Type Alias: ScoreOf<T> (…/type-aliases/ScoreOf.md)
- Lição: inferência de índices de tupla em TS; no wire as chaves já são strings numéricas.
- Veredito: NÃO APLICÁVEL — inferência de tipos TS.

### 83. Variable: ENV (…/variables/ENV.md)
- Lição: `TYPESAFE_API_KEY`, `TYPESAFE_BASE_URL`, `TYPESAFE_DEFAULT_MODEL` (jev-latest), `TYPESAFE_LOG_LEVEL` (warn); opções explícitas vencem.
- Veredito: APLICÁVEL(§B config/flag) — ler essas quatro com precedência explícito>env>default; `GROK_JEV` continua OFF (§12).

### 84. Variable: LOG_LEVELS (…/variables/LOG_LEVELS.md)
- Lição: array ordenado do mais para o menos verboso.
- Veredito: APLICÁVEL(§B) — usar a ordem debug→off para validar/filtrar nível.

### 85. Variable: VERSION (…/variables/VERSION.md)
- Lição: SDK JS pinado em `"0.6.0"`.
- Veredito: APLICÁVEL(§3 evidências) — registrar "espelha SDK Jev 0.6.0" nos testes contratuais (§12).

### 86. Function: choice() (…/functions/choice.md)
- Lição: `choice(instructions, criteria)` → `{type:"choice", instructions, criteria}` (ambos obrigatórios).
- Veredito: APLICÁVEL(§A cliente/tipos) — construtor Rust com o mesmo JSON.

### 87. Function: noul() (…/functions/noul.md)
- Lição: `instructions` default `null`; `criteria? = {true?, false?}`.
- Veredito: APLICÁVEL(§A cliente/tipos) — `noul(None, None)` serializa `instructions:null` sem `criteria`.

### 88. Function: score() (…/functions/score.md)
- Lição: `score(instructions, criteria)` com ≥2 descrições a partir do score zero.
- Veredito: APLICÁVEL(§A cliente/tipos) — construtor Rust valida ≥2 entradas.

---

## Páginas principais (itens 89–93)

### 89. Models (https://docs.typesafe.ai/models.md)
- Lição: `jev-1.13.0`; US$ 42/Btok (**US$ 0,042/Mtok**) de entrada e **saída grátis**; 250k tokens/s e 1.200 req/min; aliases `jev-latest`/`jev-preview`; a resposta traz o ID versionado (permite pinar); `GET /v1/models`.
- Veredito: APLICÁVEL(§3 E-016; §6 R-007)
- Ajuste: recomendação de **pinar a versão** quando limiares são calibrados; custo medido em tokens de entrada (§12).

### 90. API reference (https://docs.typesafe.ai/api.md)
- Lição: Choice exige `criteria`; Score ≥2 níveis; Noul aceita `criteria` true/false; erros 401/422/429/529 com backoff e `retry-after`.
- Veredito: APLICÁVEL(§A cliente; §6 R-003)
- Ajuste: mapa de erros fixado (401/422→`Invalid`; 429/529→`RateLimited`; sem retry no caminho quente) (§12).

### 91. Agent skill (https://docs.typesafe.ai/agent-skill.md)
- Lição: manter perguntas **e** limiares num único arquivo revisável; agentes escrevem perguntas mal — esperar edição colaborativa; não usar limiar onde basta o argmax; limiares mal calibrados são a causa comum de roteamento errado.
- Veredito: APLICÁVEL(§3 S-003)
- Ajuste: regra "argmax quando só importa a melhor opção" e revisão humana do catálogo (§12).

### 92. Legal (https://docs.typesafe.ai/legal.md)
- Lição: documentos DPA, MCA e privacidade; compromisso de **não treinar** com dados de cliente; **zero data retention só para enterprise**, mediante contato.
- Veredito: APLICÁVEL(§6 R-004)
- Ajuste: risco de privacidade ganhou a due diligence legal (DPA/não-treino) e a regra de minimização do `state` (§12).
- Citação-chave: "our commitment not to train models on user data"

### 93. Jev 1.13 jaggedness (https://docs.typesafe.ai/model-jaggedness/jev-1.13.md)
- Lição: leitura literal; aritmética/contagem não confiáveis (contar em código); datas lidas como texto; indireção multi-hop piora; state grande e irrelevante degrada; **conteúdo adversarial no state pode direcionar a resposta**; não gera texto. Limites: **64k tokens** (state+perguntas) e **32k** (state + maior pergunta).
- Veredito: APLICÁVEL(§1.3.1 P2; §1.4; §6 R-001)
- Ajuste: tetos 64k/32k e filtragem obrigatória do state registrados; lista "nunca Jev" confirmada (§12).

---

## Cookbooks (itens 94–111)

### 94. Self-consistency: nouls (https://docs.typesafe.ai/cookbooks/consistency_noul_cookbook.md)
- Lição: 14 Nouls, 15 repetições: desvio-padrão médio por pergunta de **0.0102** e ainda assim `covered` oscila **0.43–0.53**, atravessando o limiar 0.5; 0.49 e 0.51 geram ações opostas. A faixa 0.30–0.70 vira `uncertain` → humano, sem nova chamada.
- Veredito: APLICÁVEL(§1.3.1 P5; §1.6 I-10)
- Ajuste: banda de revisão **0.30–0.70** adotada; decisão nunca mora no fio da navalha (§12).
- Citação-chave: "A review band absorbs fluctuation around 0.5 without issuing opposite automatic actions."

### 95. Self-consistency: choices (https://docs.typesafe.ai/cookbooks/consistency_choice_cookbook.md)
- Lição: 8 Choices, 15 repetições: desvio médio **0.0098** contra **0.0245–0.0543** de LLMs (2,5×–5,6× menor); com limiar de topo **0.60**, agreement sobe de 90,8% para **99,2%** com 25,8% `uncertain`. Guarda: "100% repeatability does not imply correctness".
- Veredito: APLICÁVEL(§1.3.1 P5; §3 evidências)
- Ajuste: limiar de topo ≥0.60 para ação automática e instrumentação da taxa de abstenção; registrado como evidência de reprodutibilidade ≠ correção (§12).
- Citação-chave: "100% repeatability does not imply correctness."

### 96. Parallel questions (https://docs.typesafe.ai/cookbooks/parallel_questions.md)
- Lição: 13 perguntas numa chamada = **12,2× mais barato e 10,0× mais rápido** que 13 chamadas, com respostas idênticas; o documento domina o custo de cada request.
- Veredito: APLICÁVEL(§3 S-003)
- Ajuste: mantido "1 requisição por decisão" e prioridade em reduzir o state (§12).

### 97. Re-ranking (https://docs.typesafe.ai/cookbooks/rerank_typesafe.md)
- Lição: 3.565 passagens, 40 queries; BM25 monta shortlist de 30 e o Jev pontua par a par (1.200 chamadas, **1.536.002 tokens de input**, US$ 0,0645): top-1 5%→18%, top-10 38%→62%. Guarda: o re-rank **não adiciona** o que a busca rápida não selecionou.
- Veredito: APLICÁVEL(§1.3.1 P2)
- Ajuste: P2 formaliza que **a shortlist é o teto** do que o Jev pode encontrar; o código monta a shortlist (§12).
- Citação-chave: "It cannot add a passage that fast search did not select."

### 98. Line-by-line search (https://docs.typesafe.ai/cookbooks/semantic_find.md)
- Lição: linhas marcadas com IDs; Choice sobre os IDs ranqueia (criteria pode ser null se o texto está no state); Noul de existência separa "não há resposta" de "sempre há um primeiro"; **máximo 255 opções** por Choice — acima, dois passes (janela → linhas).
- Veredito: APLICÁVEL(§1.3.1 P2)
- Ajuste: P2 exige shortlist em código + Choice ≤255 + Noul de existência, com passe duplo documentado (§12).
- Citação-chave: "A Choice question accepts up to 255 options"

### 99. Structure recovery (https://docs.typesafe.ai/cookbooks/autoformat.md)
- Lição: duas requests, **10.211 tokens**, 0,8 s; pass-1 costura linhas (Noul por par), pass-2 classifica 17 blocos com perguntas-companheiras cujas respostas quase nunca são lidas; o **wording define o limiar** ("mid-sentence" → 17 blocos; "same paragraph" → 12).
- Veredito: APLICÁVEL(§6 riscos; §1.3.1 P3)
- Ajuste: registrado que o state domina os tokens (pergunta extra custa pouco) e que o fraseado das perguntas é parte do contrato revisável (§12).
- Citação-chave: "An extra question adds little, since the state is most of the tokens and is sent once either way"

### 100. Function calling (https://docs.typesafe.ai/cookbooks/function_calling.md)
- Lição: nome da função e argumentos de conjunto fechado viram Choice/Noul; pergunta `stated` decide **omitir** o argumento não mencionado (default da função vale); a confiança devolvida é a do **julgamento mais fraco** do conjunto; texto livre/números/datas não recebem pergunta.
- Veredito: APLICÁVEL(§1.3.1 P1/P5)
- Ajuste: P1/P5 especificados com perguntas por família/argumento, omissão via `stated` e reporte da confiança mais fraca (§12).
- Citação-chave: "An argument whose values come from a fixed list is a closed set."

### 101. Skill suggestion (https://docs.typesafe.ai/cookbooks/skill_suggestion.md)
- Lição: duas requisições (ranquear tudo + gate "precisa de skill?"; reler o top 3 com texto completo e poder rejeitar tudo); limiares **0.30**; cargas erradas **16,8%→7,3%** e desnecessárias **9,8%→4,0%**; o bloco de sugestão precisa ser **ignorável** e "nada se aplica" ainda manda uma frase; roster de 182 skills = 16.089 caracteres.
- Veredito: APLICÁVEL(§1.3.1 P6)
- Ajuste: P6 com desenho de duas fases, texto ignorável e medição de cargas erradas/desnecessárias (§12).
- Citação-chave: "reduce incorrect skill loads by more than half"

### 102. Knowledge graph entity alignment (https://docs.typesafe.ai/cookbooks/entity_alignment.md)
- Lição: 450 pares; um único **Score de três níveis** (unlink / related→curador / sameAs) carrega a decisão, **sem limiar a calibrar**: 8,9% sameAs, 11,1% curador, 80% unlinked; só 9 pares ficam a <0,1 do corte. O nível do meio existe porque o merge errado é o erro caro.
- Veredito: APLICÁVEL(§1.3.1 P5)
- Ajuste: validador pode ser um **Score allow / uncertain→caminho normal / deny** em vez de Noul+limiar (§12).
- Citação-chave: "There is no threshold constant anywhere in this file."

### 103. Classifying RAG passages (https://docs.typesafe.ai/cookbooks/classifying_rag_passages.md)
- Lição: 4 Nouls por par, roteamento em ordem fixa (>0.70 injection, >0.70 contradiz, <0.45 relevância, >0.55 evidência); limiares num **único dict** re-roteável sem API; guarda explícita: injeção é **só um filtro, não fronteira de segurança** — passagem abaixo do corte ainda chega ao prompt.
- Veredito: APLICÁVEL(§6 R-001; §1.3.1 P2)
- Ajuste: registrado que o filtro do Jev **não** é fronteira de segurança e que o consumidor trata todo texto como não confiável (§12).
- Citação-chave: "A passage that scores under the threshold still reaches the prompt, so the generator prompt has to treat every passage as untrusted text."

### 104. Double-checking citations (https://docs.typesafe.ai/cookbooks/citation_check.md)
- Lição: o que é decidível é feito **em código primeiro** (match normalizado marca `fabricated` sem modelo); um Choice de 3 relações decide supports/contradicts/says_nothing; confiança ≥0,8 auto, senão humano; 4 verified ≥0.93, 1 contradicted 0.99, 2 unsupported (0.27, 0.56) → review.
- Veredito: APLICÁVEL(§1.3.1 P2)
- Ajuste: formalizado "primeiro o que é decidível em código, o Jev lê só o contexto"; confiança baixa → revisão, nunca auto-ação (§12).
- Citação-chave: "A quote that is not in the source is fabricated, and no model is needed to find that out."

### 105. Guardrails for LLMs (https://docs.typesafe.ai/cookbooks/llm_guardrails.md)
- Lição: bateria de Nouls por perigo + Score de severidade numa chamada; o roteamento (pass/review/block/support) é política em código, com limiares por produto; precisa rodar na entrada **e** na saída.
- Veredito: NÃO APLICÁVEL como economia — adiciona chamadas por mensagem; é segurança. Registrado como candidato de segurança (não de tokens) no backlog.

### 106. SDE cascade (https://docs.typesafe.ai/cookbooks/sde_cascade.md)
- Lição: rung barato extrai → bateria de Nouls por campo devolve P(errado) → gate `any_flag` >0,7 escala para o modelo de raciocínio; a fronteira Pareto fica "up-and-left" de cada modelo único, com ≈0,81 de qualidade por ≈US$ 0,10/extração; agregar com **`max`, não média**.
- Veredito: APLICÁVEL(§1.3.1 P4/P5)
- Ajuste: Jev como **verificador barato** e escalada de modelo/effort só quando um flag dispara, inclusive no validate de tool call, agregando por `max` (§12).
- Citação-chave: "one confident red flag is enough instead of being averaged into silence"

### 107. Date extraction (https://docs.typesafe.ai/cookbooks/date_extraction_cookbook.md)
- Lição: 7 Choices numa chamada leem as partes; o código resolve (ano inferido, "next Thursday") e valida; a confiança da data é a **menor das partes usadas** (5 auto-aceitas, 1 review a 0.46); datas impossíveis (fev 30, ano fora de 1900–2050) viram review, nunca chute.
- Veredito: APLICÁVEL(§1.3.1 P5)
- Ajuste: validações decompostas em leituras atômicas com escape `none`; aritmética e checagens ficam no Rust; confiança agregada por **mínimo das partes** (§12).
- Citação-chave: "The model reads what the text says and never does the calendar math."

### 108. Pre-parsed value extraction (https://docs.typesafe.ai/cookbooks/pre_parsed_value_extraction_cookbook.md)
- Lição: regex acha candidatos, as opções da Choice são os **spans**, e o código copia verbatim e normaliza; o valor "cannot invent a value or transpose a digit"; Choice ≤255 (acima, estreitar em dois estágios); achar candidatos é o trabalho.
- Veredito: APLICÁVEL(§1.3.1 P2)
- Ajuste: consolidado como padrão dos seams — "candidatos vêm do código, o Jev escolhe"; >255 candidatos ⇒ dois estágios (§12).
- Citação-chave: "TypeSafe only ever chooses among the spans the regex found, the value you get back is one of those spans, copied unchanged."

### 109. Hierarchical classification (https://docs.typesafe.ai/cookbooks/hierarchical_classification.md)
- Lição: percorre a hierarquia com um Choice por nó; **beam K=3 acertou 4/4** folhas esperadas contra **2/4** do greedy; score de caminho = média geométrica (normaliza profundidade); frentes irmãs correm em paralelo.
- Veredito: APLICÁVEL(§backlog — experimento para P6/P1)
- Ajuste: registrado como experimento (não requisito) para escolher família/skill por hierarquia com beam (§12).
- Citação-chave: "Beam search matched 4 of 4 expected leaves; greedy search matched 2 of 4."

### 110. Autoresearch feature discovery (https://docs.typesafe.ai/cookbooks/autoresearch_feature_discovery.md)
- Lição: 38 perguntas propostas por LLM em 5 rondas; RMSE held-out 3,09 (média) → 1,77; ganho ronda 1→5 de −0,097 pontos (IC95% [−0,147, −0,050]); o custo escala com **linhas** (1 request por linha por ronda; 2.000 requests/ronda), não com perguntas.
- Veredito: NÃO APLICÁVEL — não treinamos modelo supervisionado; pontuar cada linha com o Jev é exatamente o padrão net-negative da regra de ouro. Só o "triar antes de pagar" entra como nota de backlog.

### 111. Classification using confidence (https://docs.typesafe.ai/cookbooks/classification_using_confidence.md)
- Lição: 60 10-Ks, 75 grupos numa Choice; confiança ≥0,9 reporta o grupo, senão a **divisão-pai**; forçar grupo dá 39/60, subir um nível dá **48/60 respostas úteis**; "the broad label follows from the narrow one, so there is no second call".
- Veredito: APLICÁVEL(§1.3.1 P6)
- Ajuste: no anúncio de skills, confiança baixa anuncia o skill mais genérico/caminho default em vez de um específico, sem segunda chamada (§12).
- Citação-chave: "The broad label follows from the narrow one, so there is no second call."

---

## Reconciliação

- **Itens no snapshot do índice:** 111. **Entradas neste ledger:** 111. **Diferença:** 0.
- **Leituras com sucesso:** 111/111. **Falhas de leitura:** nenhuma.
- **Vereditos:** **97 aplicáveis** (cada um com ajuste citado) · **14 não aplicáveis** com motivo: itens **19** (índice de demos, sem conteúdo técnico), **22 / 25 / 27 / 29** (SDK Python: específico de linguagem, índice ou duplicata), **51 / 56 / 57 / 72 / 74 / 75 / 82** (stubs ou inferência de tipo JS sem efeito no wire), **105** (guardrails: segurança, não economia), **110** (autoresearch: offline/treino).
- **Onde os ajustes aterrissaram no plano:** §1.3.1 (P1, P2, P3, P5, P6 refinados), §1.6 (novos invariantes I-9 e I-10; pisos 0.60/0.85 e banda 0.30–0.70), §6 (R-001 filtro não é fronteira de segurança; R-004 due diligence legal; R-005 corpos não são redigidos; R-003 divergência deliberada de retry), §12 (changelog da varredura com a tabela item→ajuste), `extensions.docs_scan` no `plan-report.json` (contrato HTTP, mapa de erros, defaults, gate de avaliação com calibração por faixa, evidência por chamada).
- **Contradições com o material congelado:** nenhuma. Duas divergências *deliberadas* em relação aos SDKs, registradas: (a) sem retry no caminho quente (SDKs default `maxRetries=2`); (b) answer kind desconhecido vira `Invalid` (SDKs logam e ignoram).
