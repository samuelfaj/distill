# Remote-Code

Um harness de código no terminal que roda em **quatro modelos diferentes** ao mesmo tempo, e usa
**Jev** — uma camada de decisão tipada, não um LLM — para escolher qual deles faz cada chamada.

Este README responde uma pergunta só: **onde cada modelo é usado, e quem decide**. Ele descreve o
que está nesta árvore, não o que se pretende construir. O README do upstream (instalação, build,
licença) está em [`README.en.md`](README.en.md).

---

## Os quatro papéis

| papel | quem é | como se configura | o que faz |
|---|---|---|---|
| **hard** | o modelo da sessão | `/model <nome>`, `-m`, `[models].default` | o modelo principal: raciocina, escreve código, edita arquivos. É o padrão de toda chamada — nada o substitui sem uma decisão |
| **light** | o irmão leve do hard (opcional) | `[jev.tiers] light = "codex-luna"` | o mesmo trabalho, mais barato, quando o passo não precisa do hard: mesmo provedor, mesma credencial, **mesma conversa** |
| **cheap** | um modelo barato de verdade, fora do provedor do hard | `[jev.local] model` (cadeia OpenRouter) | tarefas **fechadas**: resumir, extrair, classificar, comprimir texto que já está na mão. Nunca vê a conversa |
| **Jev** | a camada de decisão (não é um LLM de texto) | `[jev]` + `[jev.ladder]` | responde perguntas **tipadas** (`choice`, `score`, `noul`) sobre um `state` pequeno: qual modelo, qual effort, quais linhas, manter ou descartar |

```
                    ┌─────────────────────────────────────────────┐
   passo do turno → │ Jev: "quem faz esta chamada, com qual effort?"│
                    └───────────────┬───────────────┬─────────────┘
                                    │               │
                        ┌───────────▼──┐      ┌─────▼────────┐
                        │ hard (sessão)│      │ light (irmão)│   ← mesma conversa
                        └──────────────┘      └──────────────┘
                                    │
                        ┌───────────▼──────────────────────────┐
                        │ cheap: tarefa fechada, sem conversa   │  ← payload isolado
                        └──────────────────────────────────────┘
```

---

## hard — o modelo da sessão

É o modelo que você escolheu. Roda a conversa inteira: lê arquivos, edita, roda comandos, responde.
Tudo que não for explicitamente roteado para outro degrau acontece nele.

```toml
[models]
default = "grok-4.6"          # o hard, quando nenhum -m/--model é passado
```

Um turno começa e termina no hard, a menos que Jev decida diferente **por chamada**. Duas decisões
podem tirar uma chamada dele: o degrau `light` (mesma conversa, modelo irmão) e a lane `cheap`
(tarefa fechada, sem conversa).

---

## light — o irmão leve do hard

O light **não é outro provedor**: é um segundo modelo do mesmo provedor, com a mesma credencial, para
que ele possa pegar **a mesma conversa** no meio do turno. É o que o harness valida antes de aceitar
o par:

* mesmo `base_url` (mesmo provedor),
* mesmo `api_backend` (mesmo protocolo no fio),
* mesmo `auth_scheme` (uma credencial cobre os dois),
* e a conversa tem que **caber na janela dele** — com a reserva da resposta — senão aquela chamada
  fica no hard. Um round que o irmão não segura seria cortado no meio; isso não é roteamento, é outra
  sessão.

```toml
[jev.tiers]
light = "codex-luna"          # id de uma entrada [model.<id>]
```

Exemplo — o par ChatGPT, onde o hard é `gpt-6-astra` e o light `gpt-5.6-luna`:

```toml
[models]
default = "codex-astra"

[model.codex-astra]
model = "gpt-6-astra"
base_url = "https://chatgpt.com/backend-api/codex"
api_backend = "responses"

[model.codex-luna]            # o irmão: mesmo host, mesmo backend, mesma credencial
model = "gpt-5.6-luna"
base_url = "https://chatgpt.com/backend-api/codex"
api_backend = "responses"

[jev.tiers]
light = "codex-luna"
```

Provedor com um modelo só não tem irmão — é o caso do Grok hoje. Nesse caso a pergunta de degrau
**não é feita**: não há o que escolher, e uma pergunta com uma resposta só custa uma decisão.

**Quem decide:** Jev, uma vez por chamada, na mesma bateria que escolhe o effort (uma ida só, não
duas). A pergunta é `micro_tier` — "qual dos dois modelos faz **esta** chamada?" — com o perfil dos
dois no `state` (nome, janela, o que o dono anotou sobre cada um). Piso de confiança **0,55**, mais
alto que o do effort: um rebaixamento incerto custa qualidade no passo, então incerto = o hard roda.
A decisão é registrada com o que Jev quis e o que foi aplicado.

**Como desligar:** `[jev.ladder] b2_light_model = false` (a pergunta não é feita), ou não configurar
`[jev.tiers] light` (idem).

**Como ver e trocar:** `/tiers` reporta os três degraus na ordem em que uma chamada cai neles e
aceita `/tiers hard <nome>`, `/tiers light <id|clear>` e `/tiers cheap <ids|clear>`. O mesmo relatório
é a linha **Model tiers** da home.

---

## cheap — o modelo barato (OpenRouter)

O cheap é um modelo **de fora do provedor do hard**, alcançado por uma cadeia de fallback do
OpenRouter. Ele nunca recebe a conversa: recebe um **payload fechado** (uma saída de ferramenta, um
trecho de log, uma lista de candidatos) e devolve texto curto, que passa por um **guarda** antes de
ser usado.

```toml
[jev.local]
model = "inclusionai/ling-3.0-flash-vl:free,inclusionai/ling-3.0-flash-vl,qwen/qwen3.7-flash"
max_context_tokens = 262144      # teto do dono para uma rodada inteira no cheap
notes = "cadeia de fallback: o OpenRouter tenta na ordem e cobra só quem responde"
```

* A vírgula é **prioridade**, não lista de opções: o primeiro id é o modelo da requisição e os
  seguintes vão em `models`, a rota de fallback do próprio OpenRouter. O tier grátis vem primeiro
  porque custa zero.
* A chave vem do ambiente (`OPENROUTER_API_KEY`), nunca do arquivo de config.
* O `model` também aceita o **id de uma entrada** `[model.<id>]` — aí o transporte é o da entrada.
* `/cheap-model` mostra o estado e troca a cadeia; `/cheap-model <ids>` grava; `clear` volta para a
  cadeia de fábrica.

### Onde o cheap é usado

**1. Tarefas fechadas (o catálogo).** 93 tarefas de texto, cada uma com um guarda que a resposta tem
que passar:

| tipo | quantas | o que devolve | guarda típico |
|---|---|---|---|
| `Extract` | 31 | itens verbatim (símbolos, campos, linhas) | todo literal do payload tem que reaparecer |
| `Digest` | 22 | o que importa, em linhas curtas | idem |
| `Compress` | 18 | o payload encolhido a ~⅓ | idem |
| `Ask` | 9 | a menor resposta que responde, citando | spans do payload |
| `Pick` | 7 | ids de uma lista de candidatos | só ids que o chamador ofereceu |
| `Classify` | 6 | um rótulo de um conjunto fechado | tem que ser um dos rótulos |

Dos 93 guardas, **71 exigem que todo literal** (caminho, `file:line`, número, mensagem de erro)
sobreviva à resposta, 3 exigem spans citados do payload, e os demais exigem JSON, rótulo do conjunto
fechado ou ids que o chamador ofereceu. Resposta que não passa no guarda é descartada, e o caminho de
sempre segue.

**2. Uma rodada inteira (`b2_local_model`).** Quando Jev julga que o cheap consegue fazer a chamada
**inteira** (capacidade ≥ 0,70 e nenhum aviso ≥ 0,40), a rodada vai para ele — o modelo da sessão não
é chamado. Duas travas: a estimativa da conversa + a reserva tem que caber no teto (`max_context_tokens`
ou a janela do modelo), e o resultado volta com o mesmo guarda das tarefas.

**3. A lane de retenção (`e_retention`).** Saída grande demais para caber no contexto vira N blocos,
e Jev decide o que fica de cada um, **antes** de o texto original ser descartado (store-before-loss,
exceto se houver segredo no payload).

Cada lane tem sua própria chave em `[jev.ladder]`, um contador por turno e um **breaker**: depois de
3 falhas a lane fica de fora pelo resto do turno — um endpoint instável não custa tempo em cada passo.
Toda chamada barata registra uma linha sem conteúdo (lane, tarefa, decisão, modelo, tokens, latência)
no mesmo gravador das outras decisões.

---

## Jev — onde ele decide

O Jev responde perguntas estruturadas sobre um `state` pequeno que **o código montou**: candidatos,
linhas, menus de effort, perfis de modelo. Ele nunca inventa um candidato, nunca gera texto e nunca
amplia permissão — só escolhe entre o que já foi calculado, ou abstém-se.

**O contrato, em uma linha:** o código produz candidatos, o Jev escolhe, o caminho de sempre é o
fallback, e falha/empate/timeout = comportamento antigo.

### Onde ele age hoje (por área)

| área | quando | o que decide |
|---|---|---|
| **permissão** | chamada de ferramenta não rotineira; e **toda** chamada em YOLO, como freio | libera/recusa/escala; no YOLO só recusa (piso 0,85) |
| **conteúdo** | busca de arquivos, leitura grande, log com erro, busca web, memória/`AGENTS.md`, qual teste rodar | o que o hard vai **relê** |
| **esforço** | início do turno | intenção, famílias de ferramentas, dica de delegação, tipo de subagente |
| **degrau e esforço** | **cada chamada**, com `/effort auto` ligado | **qual modelo (hard ou light)** e **qual effort**, na mesma bateria |
| **barato** | resultado de ferramenta, saída grande, rodada inteira | qual das 93 tarefas serve, o que manter, se o cheap pega a rodada |
| **qualidade** | depois de cada edição, falha de build/teste, diff, erros múltiplos | o passo precisa ser refeito? com mais effort? categoria da falha, ordem dos erros |
| **contexto** | antes de compactar, saída ≥ 4 KB, depois de compactar | o que o resumidor precisa ver, o que é inerte, o que volta |
| **escopo** | antes de executar ferramenta | o alvo bate com a intenção? |

Cada ponto é uma **alavanca** em `[jev.ladder]` (`b2_light_model`, `b2_local_model`, `e_retention`,
`c1_premature_stop`, …), com piso próprio. Desligar uma alavanca desliga aquele ponto, não o Jev.

### Configuração

```toml
[jev]
provider = "openrouter_decisions"                 # o transporte das decisões
base_url = "https://openrouter.ai/api"
model = "~typesafe/jev-latest"
api_key_env = "OPENROUTER_API_KEY"
timeout_ms = 20000
effort_auto = true                                # a sessão começa em modo auto

[jev.tiers]
light = "codex-luna"                              # opcional: o irmão leve do hard

[jev.local]
model = "inclusionai/ling-3.0-flash-vl:free,inclusionai/ling-3.0-flash-vl,qwen/qwen3.7-flash"

[jev.ladder]
b2_light_model = true                             # a pergunta de degrau
b2_local_model = true                             # a lane barata
e_retention = true                                # a lane de retenção
```

### Effort auto — o modo que liga a decisão por chamada

`/effort auto` faz Jev escolher **degrau e effort de cada chamada** dentro do turno:

* o `state` leva o nome e o id do modelo, o **menu de efforts que aquele modelo oferece** (com as
  descrições), a fase (`start_of_turn` / `mid_turn_after_tools`), os últimos passos, quantos itens o
  turno já tem e o pedido do usuário;
* a escolha de effort fica **restrita ao menu do modelo** (ele não pode pedir um nível inexistente) e
  existe `keep_session_effort` para ele abster-se; piso **0,40**;
* a escolha de degrau tem piso **0,55** e a resposta `keep_session_model` para abster-se;
* tudo é registrado — o que Jev queria, além do que foi aplicado
  (`effort:low · applied to this call` vs. `defer | wanted low at 0.41 below the floor`);
* `/effort <nível>` desliga o modo para a sessão (nível explícito sempre ganha); `--effort` na linha
  de comando faz o mesmo na largada.

### Como auditar

`GROK_LOG_JEV=1` grava uma linha JSON por decisão em `~/.grok/logs/jev.jsonl` (lane, decisão, motivo,
confiança, latência, modelo). Sem isso não há como saber por que uma chamada foi parar em outro
modelo — foi assim que este harness foi calibrado.

---

## Provedores: qualquer um, ou nenhum

O harness não exige um provedor específico. Cada um tem seu caminho de login, e todos podem estar
desligados ao mesmo tempo:

| provedor | login | onde mora a credencial |
|---|---|---|
| **Grok** | `/login` na TUI, ou `remote-code login` | `auth.json` do harness |
| **ChatGPT** | `remote-code login --chatgpt` (ou `--chatgpt --device-auth` para máquina sem navegador) | `~/.grok/codex-auth.json`, OAuth **do próprio harness**, com refresh |
| **OpenRouter** | a chave em `OPENROUTER_API_KEY` | o ambiente |

A home lista `Log in with Grok`, `Log in with ChatGPT` e `Log in with OpenRouter`; sem sessão Grok,
a tela de gate oferece ChatGPT e OpenRouter também. **Nenhum é obrigatório:** com só modelos locais
configurados (um endpoint OpenAI-compatible em `[model.*]`), o harness roda igual — sem credencial de
decisão o Jev simplesmente não responde, e cada ponto que ele cobriria volta ao comportamento de
antes (fail-open), com as lanes baratas de fora.

---

## O que decide o quê, em uma frase

* **hard** faz o trabalho;
* **light** faz o trabalho quando o passo não precisa do hard, na mesma conversa;
* **cheap** faz o que é texto fechado, sem conversa;
* **Jev** escolhe entre eles — e escolhe o resto das decisões estruturadas do harness — sempre com um
  caminho de sempre se ele falhar.

Detalhe item a item — arquivo, teste e piso de cada decisão — em [`list.md`](list.md).
