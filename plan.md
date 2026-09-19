<!-- Modified for Distill by Samuel Fajreldines, 2026. -->
# plan.md — Jev no harness, servido pelo OpenRouter

Este é o planejamento executado nesta rodada: **não existe mais app**. O token saver
inteiro vive dentro do harness (baseado no Distill). A camada de decisão roda no
**próprio modelo Jev** (`~typesafe/jev-latest`), servido pelo endpoint de decisões do
OpenRouter, e o trabalho barato roda em `qwen/qwen3.7-flash` — também pelo OpenRouter,
com uma chave só, no lugar do LLM local.

## 1. O que muda

| Antes | Agora |
| --- | --- |
| Decisões tipadas só no TypeSafe System One (`POST /v1/systemone`) | O **próprio modelo Jev** (`~typesafe/jev-latest`) pelo endpoint de decisões do OpenRouter (`POST /api/alpha/decisions`) — mesmo corpo, mesma resposta tipada, uma chave só. O TypeSafe direto continua selecionável |
| Micro-ação barata ia para o modelo local (oMLX, `qwen38-local`) | A mesma micro-ação vai para `qwen/qwen3.7-flash` via OpenRouter (chat) |
| Effort chegava na rede como `reasoning_effort`, ou não chegava | O effort que o Jev escolhe chega **na forma que cada modelo anuncia**: `reasoning.effort`, `reasoning.max_tokens` ou nada |
| O effort era o da sessão | **Effort auto por padrão**: o Jev escolhe o effort de cada micro-ação (`[jev] effort_auto`, desligável com `/effort <nível>`) |
| Um selo genérico `·local` na linha de status | O selo nomeia o **modelo** que vai rodar (`·openrouter-qwen37 low`) |

O contrato tipado não muda: as perguntas continuam sendo `noul`/`choice`/`score`, com
probabilidades e confiança; todo limiar, flag por item e a regra de *fail-defer* seguem
exatamente onde estavam. Trocar de provedor é trocar `provider`/`base_url`/`model`/
`api_key_env`.

## 2. Arquitetura

```
                    ┌──────────────────────────── harness ────────────────────────────┐
micro-ação ──► bateria tipada ──► JevClient ──► openrouter_decisions ──► openrouter.ai/api
                    │               │               /alpha/decisions        ~typesafe/jev-latest
                    │               ├── typesafe ──► api.typesafe.ai /v1/systemone
                    │               └── openrouter (chat) ──► /chat/completions
                    │
                    ├── resposta tipada (choice/probabilities/confidence) ──► política de sempre
                    │
                    └── roteamento da rodada ──► modelo barato + effort na forma do modelo
```

Os dois primeiros backends falam o **mesmo envelope** (`{state, model, questions}` →
respostas com `type`), então compartilham corpo e leitura; só o backend de chat precisa
das perguntas renderizadas num prompt.

- `crates/codegen/distill-workspace/src/jev/provider.rs` — os backends: `typesafe`
  (serviço direto), `openrouter_decisions` (o modelo Jev no OpenRouter, mesmo envelope)
  e `openrouter` (chat: renderiza a bateria num prompt estrito, lê o JSON de volta como
  `Answer` tipada e valida cada resposta contra a própria pergunta — opção fora dos
  critérios, score fora da rubrica, resposta faltando ⇒ `Invalid` ⇒ fail-defer). Também
  decide a forma do `reasoning` por modelo (`ReasoningShape`) e o orçamento por nível.
- `crates/codegen/distill-workspace/src/jev/client.rs` — o transporte: um caminho só,
  com pré-checagens (perguntas válidas, teto de bytes do estado, credencial resolvida
  em tempo de chamada), prazo único cobrindo o corpo da resposta, sem retry, sem log de
  corpo ou credencial, e a taxonomia de erro de sempre.
- `crates/codegen/distill-sampling-types/src/types.rs` — `ReasoningShape` e
  `reasoning_budget_tokens` (o vocabulário de rede) mais
  `ChatCompletionRequest::apply_reasoning_shape`.
- `crates/codegen/distill-sampler/src/client.rs` — `apply_defaults` traduz o effort da
  rodada para a forma do modelo antes de serializar.
- `crates/codegen/distill-shell/src/agent/config.rs` — `[model.<id>] reasoning_shape`
  → `ModelInfo` → `SamplerConfig`.
- `crates/codegen/distill-shell/src/session/acp_session_impl/jev_routing.rs` — o
  roteamento por micro-ação (modelo barato + effort) e o selo da linha de status.

## 3. Configuração

Chave no ambiente global (nunca no repositório), já exportada em `~/.zshrc` (modo 600) e
excluída do ambiente das ferramentas:

```
OPENROUTER_API_KEY=sk-or-v1-…     # rotacione: esta chave foi colada em texto claro num chat
```

`~/.grok/config.toml`:

```toml
[jev]
provider = "openrouter_decisions"             # ou "openrouter" (chat) ou "typesafe"
base_url = "https://openrouter.ai/api"        # o endpoint de decisões
model = "~typesafe/jev-latest"                # o modelo Jev
api_key_env = "OPENROUTER_API_KEY"
timeout_ms = 20000
effort_auto = true                            # o Jev escolhe o effort por micro-ação

[jev.local]                                   # o modelo barato das micro-ações
model = "openrouter-qwen37"
max_context_tokens = 262144
min_capability = 0.6

[model.openrouter-qwen37]
model = "qwen/qwen3.7-flash"
base_url = "https://openrouter.ai/api/v1"
env_key = "OPENROUTER_API_KEY"
api_backend = "chat_completions"
context_window = 1000000
max_completion_tokens = 8192
reasoning_shape = "max_tokens"                # sem `reasoning_effort` neste modelo
```

## 4. O que foi implementado

1. **Chave e catálogo** — chave no ambiente global, entrada `[model.openrouter-qwen37]`,
   `[jev]` na camada de decisão e `OPENROUTER_API_KEY` fora do ambiente das ferramentas.
   Verificado com chamadas reais: `GET /v1/key`, `GET /v1/models` (que mostrou que
   `qwen/qwen3.7-flash` anuncia `reasoning` + `max_tokens` e **não** `reasoning_effort`,
   e que um orçamento maior que o teto de completion é rejeitado — daí o clamp) e o
   endpoint de decisões, que recusa o caminho de chat com uma mensagem explícita.
2. **A decisão roda no modelo Jev, pelo OpenRouter** — `~typesafe/jev-latest` não é um
   modelo de chat: é o endpoint `POST /api/alpha/decisions`, com o mesmo envelope do
   System One. O harness o trata como um backend próprio, e não como um modelo de chat
   com as perguntas transformadas em prompt.
3. **Transporte de decisão tipado, com três backends** (TypeSafe direto ainda
   selecionável) com testes de unidade do adaptador e um teste vivo que roda a bateria
   real de permissão contra o OpenRouter — 9 perguntas, todas respondidas com o tipo
   pedido, `usage` 726/755 tokens, decisão composta `Allow`.
4. **Leitura tolerante de um modelo de chat** — o mesmo contrato, lido nas grafias que um
   modelo de chat usa (`probability`/`yes`/`answer`, `score`/`level`/a própria palavra da
   rubrica): a resposta continua sendo a que foi perguntada, e uma opção que a pergunta
   não ofereceu continua sendo recusada.
5. **Effort na forma do modelo** — `ReasoningShape` por modelo, tradução no sampler, e o
   effort que o Jev escolhe viaja com a rodada roteada.
6. **Redução determinística de payload** (`jev/reduce.rs` + `jev_store.rs`), na lane D2: deduplica linhas
   repetidas e colapsa linhas em branco (**nada de único se perde**; o marcador diz em
   qual linha ficou a cópia), e quando ainda é grande elide o meio **depois de gravar o
   original** em `~/.grok/jev/store/<hash>.txt`, com o caminho no marcador — o modelo lê
   de volta com o `read_file` de sempre. O guarda `preserves_literals` recusa qualquer
   redução que perderia um path, `file:line`, número ou erro.
7. **Effort auto por padrão** — a sessão começa com o Jev escolhendo o effort de cada
   micro-ação (`[jev] effort_auto`, sem valor ⇒ ligado), e um `/effort <nível>` fixa um
   nível para a sessão.
8. **Visibilidade** — a linha de status nomeia o modelo da micro-ação e o nível de
   effort; cada decisão vai para `~/.grok/logs/jev.jsonl` com modelo, tokens, latência e
   id da requisição; o relatório do turno mostra a distribuição (modelo, effort, tokens).

### O que ainda **não** está implementado

- **Compressão pelo modelo barato** (o texto grande ser resumido por um modelo de chat
  antes de chegar ao modelo caro) — a lane determinística existe e está ligada à flag
  `d2_big_output_retention`, mas a compressão por modelo, com o mesmo store-before-loss,
  não.
- **Reuso de leitura**: implementado e ligado ao mesmo flag da lane D2 — um payload
  byte-idêntico a um já enviado vira uma nota que aponta para a primeira cópia
  (`crate::jev::note_payload_read` + `reduce::reuse_note`), com teste do índice; o que
  falta é um limite por turno e a medição no caminho vivo.
- **Redução do prompt fixo por turno** (levar ao modelo só os blocos que o turno precisa).

## 5. Como verificar

```bash
# unidade (sem rede)
cargo test -p distill-workspace --lib jev            # 113 testes
cargo test -p distill-sampling-types --lib types::tests
cargo test -p distill-sampler --lib apply_defaults
cargo test -p distill-shell --lib jev                # 30 testes
cargo test -p distill-shell --lib a_fresh_manager_starts_in_auto_effort
cargo check -p distill-workspace -p distill-shell -p distill-pager -p distill-pager-bin

# vivo (usa a chave do ambiente, nunca a imprime)
OPENROUTER_API_KEY=… cargo test -p distill-workspace --test jev_live -- --ignored --nocapture
```

## 6. O que foi verificado nesta rodada

| Verificação | Resultado |
| --- | --- |
| Chamada real ao OpenRouter pelo cliente do harness | ✅ 9 perguntas tipadas, `usage` 726/755, decisão `Allow`; a chave não aparece em nenhum log (`grep -c sk-or-v1` = 0) |
| Corpus inseguro (12 casos) | ✅ 0 liberados, 11 bloqueados, 1 adiado ao caminho de sempre |
| Effort na forma do modelo | ✅ `cargo test -p distill-sampler --lib apply_defaults` + 4 testes de `ReasoningShape`; o level vira budget de tokens e o orçamento nunca passa do teto de completion |
| Config → sampler | ✅ `[model.openrouter-qwen37] reasoning_shape` chega em `SamplerConfig.reasoning_shape` |
| Redução determinística | ✅ 7 testes (log de build, listagem, prosa, diff, guarda de literais, elisão com range exato, hash/reuso) |
| Reuso de leitura | ✅ índice testado (primeira leitura lembra, repetição idêntica aponta, bytes diferentes não colidem); no caminho vivo: payload ≥ 2 KB com o flag D2 ligado |
| Entrada real, duas vezes | ✅ dois turnos completos com resposta correta; 18 e 17 decisões, todas nomeando `qwen/qwen3.7-flash`; o modelo barato roteou uma rodada de sessão (~29k/1M tokens) |
| Kill switch (`GROK_JEV=0`) | ✅ zero registros novos |
| `cargo test -p distill-pager --lib` | ❌ falha **pré-existente** de feature (`WorkspaceOps::for_test` não existe sem a feature `test-support`), reproduzida também com as mudanças guardadas com `git stash` |
| Selo na tela do TUI | ⚠️ não capturado: o driver de pty não está instalado nesta máquina e o `script(1)` não trouxe o desenho da TUI no fluxo capturado (o log de decisões e o relatório do turno são a evidência de que o caminho rodou) |
| Terceiro turno vivo (leitura de arquivo) | ⚠️ 18 decisões, 16 aplicadas; 2 caíram no fail-defer: uma resposta sem JSON (`reply carried no JSON object`) e um `choice` respondido como `None` — os dois viram o caminho de sempre em vez de palpite, e ficam registrados com o motivo |
| O modelo Jev pelo OpenRouter (cliente do harness) | ✅ `typesafe/jev-1.13-20260917`, 9 perguntas tipadas, 1183/181 tokens, **1,2 s**, id do corpo; `Block` para `rm -rf ~/Documents` e `Escalate` para uma ação incerta |
| Turno real já com o modelo Jev na decisão | ✅ 21 decisões, **todas** nomeando `typesafe/jev-1.13-20260917`, **zero erros e zero timeouts** (antes: 3 erros e 8 timeouts com o modelo de chat), latência de 0,7–1,2 s por decisão |
| Effort auto por padrão | ✅ `b2_micro_effort` decidiu `effort:none` para duas micro-ações triviais (`applied to this call · model DeepSeek V4.1 Flash`); teste `a_fresh_manager_starts_in_auto_effort` cobre o padrão ligado e a chave desligando |

## 7. Riscos

- **Chave exposta**: foi colada em texto claro; deve ser rotacionada no OpenRouter.
- **Modelos mudam de dialeto**: um modelo que troque o parâmetro de reasoning passa a
  recusar a requisição; por isso a forma é configuração por modelo, e não uma suposição.
- **Decisão em modelo pequeno**: um `choice` fora das opções ou um score fora da rubrica
  é recusado e a ação cai no caminho de sempre (fail-defer) — nunca vira palpite.
- **Latência**: a decisão agora é gerada token a token (2–4 s por item medidos, 8 s para a
  bateria de permissão). Um turno com muitos itens ativos sente isso; o orçamento por
  item (`item_budget_ms`) e o esforço da própria decisão (`reasoning_effort`) são os
  botões para trocar latência por decisões mais completas.
