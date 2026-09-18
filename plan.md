# plan.md — Jev no harness, servido pelo OpenRouter

Este é o planejamento executado nesta rodada: **não existe mais app**. O token saver
inteiro vive dentro do harness (baseado no grok-build), e o modelo que serve tanto a
camada de decisão quanto o trabalho barato é o **modelo barato do OpenRouter**
(`qwen/qwen3.7-flash`), no lugar do LLM local.

## 1. O que muda

| Antes | Agora |
| --- | --- |
| Decisões tipadas só no TypeSafe System One (`POST /v1/systemone`) | Mesmo contrato tipado, servido também por um endpoint OpenAI-compatible (`POST /chat/completions`) — OpenRouter |
| Micro-ação barata ia para o modelo local (oMLX, `qwen38-local`) | A mesma micro-ação vai para `qwen/qwen3.7-flash` via OpenRouter |
| Effort chegava na rede como `reasoning_effort`, ou não chegava | O effort que o Jev escolhe chega **na forma que cada modelo anuncia**: `reasoning.effort`, `reasoning.max_tokens` ou nada |
| Um selo genérico `·local` na linha de status | O selo nomeia o **modelo** que vai rodar (`·openrouter-qwen37 low`) |

O contrato tipado não muda: as perguntas continuam sendo `noul`/`choice`/`score`, com
probabilidades e confiança; todo limiar, flag por item e a regra de *fail-defer* seguem
exatamente onde estavam. Trocar de provedor é trocar `provider`/`base_url`/`model`/
`api_key_env`.

## 2. Arquitetura

```
                    ┌──────────────────────────── harness ────────────────────────────┐
micro-ação ──► Jev (typed questions) ──► JevClient ──► provider = openrouter ──► OpenRouter
                    │                        │              /chat/completions      qwen/qwen3.7-flash
                    │                        └── provider = typesafe ──► api.typesafe.ai /v1/systemone
                    │
                    ├── resposta tipada (choice/probabilities/confidence) ──► política de sempre
                    │
                    └── roteamento da rodada ──► modelo barato + effort na forma do modelo
```

- `crates/codegen/xai-grok-workspace/src/jev/provider.rs` — o adaptador: renderiza as
  perguntas tipadas num prompt estrito, lê o JSON de volta como `Answer` tipada e
  valida cada resposta contra a própria pergunta (opção fora dos critérios, score fora
  da rubrica, resposta faltando ⇒ `Invalid` ⇒ fail-defer). Também decide a forma do
  `reasoning` por modelo (`ReasoningShape`) e o orçamento de pensamento por nível.
- `crates/codegen/xai-grok-workspace/src/jev/client.rs` — o transporte: um caminho só,
  com pré-checagens (perguntas válidas, teto de bytes do estado, credencial resolvida
  em tempo de chamada), prazo único cobrindo o corpo da resposta, sem retry, sem log de
  corpo ou credencial, e a taxonomia de erro de sempre.
- `crates/codegen/xai-grok-sampling-types/src/types.rs` — `ReasoningShape` e
  `reasoning_budget_tokens` (o vocabulário de rede) mais
  `ChatCompletionRequest::apply_reasoning_shape`.
- `crates/codegen/xai-grok-sampler/src/client.rs` — `apply_defaults` traduz o effort da
  rodada para a forma do modelo antes de serializar.
- `crates/codegen/xai-grok-shell/src/agent/config.rs` — `[model.<id>] reasoning_shape`
  → `ModelInfo` → `SamplerConfig`.
- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/jev_routing.rs` — o
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
provider = "openrouter"                       # ou "typesafe"
base_url = "https://openrouter.ai/api/v1"
model = "qwen/qwen3.7-flash"
api_key_env = "OPENROUTER_API_KEY"
timeout_ms = 20000
reasoning_shape = "max_tokens"                # o que este modelo anuncia
reasoning_effort = "low"                      # quanto a própria decisão pensa
max_completion_tokens = 2048

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
   `[jev]` apontando para o OpenRouter, e `OPENROUTER_API_KEY` fora do ambiente das
   ferramentas. Verificado com uma chamada real (`GET /v1/key`, `GET /v1/models`, uma
   completion) — foi essa chamada que mostrou que `qwen/qwen3.7-flash` anuncia
   `reasoning` + `max_tokens` e **não** `reasoning_effort` (e que um orçamento maior que
   o teto de completion é rejeitado: daí o clamp).
2. **Transporte de decisão via OpenRouter** (mesmo contrato tipado, TypeSafe ainda
   selecionável) com testes de unidade do adaptador e um teste vivo que roda a bateria
   real de permissão contra o OpenRouter — 9 perguntas, todas respondidas com o tipo
   pedido, `usage` 726/755 tokens, decisão composta `Allow`.
3. **Leitura tolerante do modelo pequeno** — o mesmo contrato, lido nas grafias que um
   modelo de chat usa (`probability`/`yes`/`answer`, `score`/`level`/a própria palavra da
   rubrica): a resposta continua sendo a que foi perguntada, e uma opção que a pergunta
   não ofereceu continua sendo recusada.
4. **Effort na forma do modelo** — `ReasoningShape` por modelo, tradução no sampler, e o
   effort que o Jev escolhe viaja com a rodada roteada.
5. **Redução determinística de payload** (`jev/reduce.rs` + `jev_store.rs`), na lane D2: deduplica linhas
   repetidas e colapsa linhas em branco (**nada de único se perde**; o marcador diz em
   qual linha ficou a cópia), e quando ainda é grande elide o meio **depois de gravar o
   original** em `~/.grok/jev/store/<hash>.txt`, com o caminho no marcador — o modelo lê
   de volta com o `read_file` de sempre. O guarda `preserves_literals` recusa qualquer
   redução que perderia um path, `file:line`, número ou erro.
6. **Visibilidade** — a linha de status nomeia o modelo da micro-ação e o nível de
   effort; cada decisão vai para `~/.grok/logs/jev.jsonl` com modelo, tokens, latência e
   id da requisição; o relatório do turno mostra a distribuição (modelo, effort, tokens).

### O que ainda **não** está implementado

- **Compressão pelo modelo barato** (o texto grande ser resumido pelo `qwen3.7-flash`
  antes de chegar ao modelo caro) — a lane determinística existe e está ligada à flag
  `d2_big_output_retention`, mas a compressão por modelo, com o mesmo store-before-loss,
  não.
- **Reuso de leitura entre turnos** (um `read_file` byte-idêntico ao anterior virar uma
  nota apontando para a leitura anterior): as peças puras existem e estão testadas
  (`content_hash`, `reuse_note`), mas o índice e o ponto de inserção no caminho do
  `read_file` não.
- **Redução do prompt fixo por turno** (levar ao modelo só os blocos que o turno precisa).

## 5. Como verificar

```bash
# unidade (sem rede)
cargo test -p xai-grok-workspace --lib jev            # 110 testes
cargo test -p xai-grok-sampling-types --lib types::tests
cargo test -p xai-grok-sampler --lib apply_defaults
cargo test -p xai-grok-shell --lib jev                # 29 testes
cargo check -p xai-grok-workspace -p xai-grok-shell -p xai-grok-pager -p xai-grok-pager-bin

# vivo (usa a chave do ambiente, nunca a imprime)
OPENROUTER_API_KEY=… cargo test -p xai-grok-workspace --test jev_live -- --ignored --nocapture
```

## 6. O que foi verificado nesta rodada

| Verificação | Resultado |
| --- | --- |
| Chamada real ao OpenRouter pelo cliente do harness | ✅ 9 perguntas tipadas, `usage` 726/755, decisão `Allow`; a chave não aparece em nenhum log (`grep -c sk-or-v1` = 0) |
| Corpus inseguro (12 casos) | ✅ 0 liberados, 11 bloqueados, 1 adiado ao caminho de sempre |
| Effort na forma do modelo | ✅ `cargo test -p xai-grok-sampler --lib apply_defaults` + 4 testes de `ReasoningShape`; o level vira budget de tokens e o orçamento nunca passa do teto de completion |
| Config → sampler | ✅ `[model.openrouter-qwen37] reasoning_shape` chega em `SamplerConfig.reasoning_shape` |
| Redução determinística | ✅ 7 testes (log de build, listagem, prosa, diff, guarda de literais, elisão com range exato, hash/reuso) |
| Entrada real, duas vezes | ✅ dois turnos completos com resposta correta; 18 e 17 decisões, todas nomeando `qwen/qwen3.7-flash`; o modelo barato roteou uma rodada de sessão (~29k/1M tokens) |
| Kill switch (`GROK_JEV=0`) | ✅ zero registros novos |
| `cargo test -p xai-grok-pager --lib` | ❌ falha **pré-existente** de feature (`WorkspaceOps::for_test` não existe sem a feature `test-support`), reproduzida também com as mudanças guardadas com `git stash` |
| Selo na tela do TUI | ⚠️ não capturado: o driver de pty não está instalado nesta máquina e o `script(1)` não trouxe o desenho da TUI no fluxo capturado (o log de decisões e o relatório do turno são a evidência de que o caminho rodou) |

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
