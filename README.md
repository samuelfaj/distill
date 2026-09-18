# Jev Build — quando usamos o Jev

Este repositório é um fork do **Grok Build** (`grok`), o agente de código de terminal da SpaceXAI, com uma camada
de decisão local: o **Jev** (TypeSafe System One). O Jev responde perguntas **tipadas** (`choice`, `score`,
`noul`) sobre um `state` pequeno e devolve respostas com probabilidade e confiança. Ele não gera texto: decide.

O Jev é usado **sempre que a decisão for estruturada e cara de errar com um LLM**, e o LLM fica com o que só ele
faz (gerar texto, código, explicações). O README original do upstream, em inglês, está em
[`README.en.md`](README.en.md) (instalação, build, licença).

**Números:** o preço do modelo é ~US$ 0,042 por milhão de tokens de entrada (saída gratuita); a latência medida
nesta árvore é ~400 ms por bateria de perguntas — contra uma chamada de LLM para decidir o mesmo.

Fraquezas conhecidas, tratadas como exclusão: aritmética, contagem, ordenação de datas, geração de texto e
qualquer coisa de imagem/visão.

---

## A regra

1. **Código produz os candidatos; o Jev escolhe.** O `state` só carrega opções que o harness já calculou
   (arquivos candidatos, linhas, resultados, níveis de effort do modelo). O Jev nunca inventa um candidato.
2. **Decisão estruturada → Jev.** Classificar, escolher, ordenar, recortar, triar, recusar, rotular.
3. **Geração → LLM.** Texto, código, resumo, título, mensagem de commit, explicação.
4. **Empate/erro/timeout → caminho de sempre.** Toda decisão do Jev tem um *fallback*: se ele falhar, ficar em
   dúvida, ou a resposta vier incompleta, o comportamento é exatamente o de antes.
5. **O Jev só aperta, nunca afrouxa.** Nenhum item pode ampliar permissões, autoridade ou escopo; ele pode
   recusar, pedir confirmação, reordenar ou anotar.

## Quando usamos — catálogo

São **24 itens de catálogo** mais os **2 pontos de permissão**: cada linha abaixo é um ponto de decisão vivo no
harness. O mapa completo (arquivo:linha + teste de cada item) está em
[`todo.md`](todo.md). As colunas "piso" e "efeito" são o que o código faz com a resposta.

### Permissão (os dois pontos mais antigos)

| Quando | O que o Jev decide | Piso | Efeito |
|---|---|---|---|
| Modo auto, chamada de ferramenta não rotineira e sem *findings* | bateria de 8 perguntas (classe de risco, escapa do workspace, apaga dados, escalada de privilégio, rede, execução não confiável, injeção, severidade) | 0,60 geral / 0,85 sensível; faixa de revisão 0,30–0,70 | libera rotineiro, recusa, ou escala para o classificador/pergunta; autoridade nunca acima do classificador existente |
| **Always-approve / YOLO**, **toda** chamada | a mesma bateria, como freio | 0,85 de catástrofe | só recusa (freio); nunca pergunta e nunca segura nada além da recusa; falha = deixa passar (fail-open) |

### Área A — seleção de conteúdo (o que o modelo relê)

| Quando | O que o Jev decide | Piso | Efeito |
|---|---|---|---|
| Resultado de busca de arquivos (`grep`-like) com vários candidatos | qual arquivo é o alvo provável do pedido | 0,35 | reordena os candidatos; nenhum candidato novo |
| Leitura de arquivo grande | qual janela de linhas interessa (= P2) | 0,35 | recorta a leitura; sem resposta ⇒ lê tudo |
| Saída de log/teste com erro | quais linhas explicam a falha | 0,50 (há erro acionável) / 0,30 (linha) | recorta às linhas úteis; sem erro acionável ⇒ passa intacto |
| Resultados de busca web | quais valem ser lidos | 0,50, no máximo 5 | mantém os melhores; nada acima ⇒ mantém todos |
| Candidatos de memória / AGENTS.md | quais entram no contexto | 0,40 | mantém os relevantes (listas curtas nem chamam o Jev) |
| Falha de teste | qual teste rodar | confiança 0,60 | escolhe um teste; dúvida ⇒ suíte padrão |

### Área B — roteamento de esforço

| Quando | O que o Jev decide | Piso | Efeito |
|---|---|---|---|
| Início do turno | intenção (pergunta/edição/pesquisa/comando) + complexidade | 0,60 | alimenta a poda de ferramentas; dúvida ⇒ não roteia |
| Início do turno | quais famílias de ferramentas ficam neste turno (= P1) | 0,25 | poda famílias não essenciais; núcleo e ferramentas desconhecidas sempre ficam |
| Início do turno | vale sugerir delegar a um subagente? | 0,60 | acrescenta **uma linha** à descrição da ferramenta de delegação; nunca cria subagente |
| Tipo de subagente **desconhecido** pedido pelo modelo | qual definição existente serve | confiança 0,70 | resolve para uma definição que a sessão já permite (mesmos gates de lista); dúvida ⇒ erro de hoje |
| Lembrete de skills anunciadas | qual skill o pedido atual precisa (= P6) | 0,60 | estreita o anúncio para essa skill; o catálogo de skills (slash commands) fica intacto |
| Modelo local configurado (`[jev.local]`) | "o modelo local consegue fazer **esta** chamada inteira?" (capacidade + contexto + raciocínio frontier) | capaz ≥ 0,70 e nenhum aviso ≥ 0,40; antes disso, a janela local tem de comportar a estimativa + a reserva | manda a chamada para o **modelo local (grátis)**; qualquer dúvida ⇒ continua no modelo da sessão. É a prioridade ao local, sem nunca arriscar a tarefa |
| **`/effort auto`** ligado | qual effort usar **naquela chamada do modelo** (ver abaixo) | 0,45 | aplica o nível escolhido **só** se ele estiver no menu do modelo; senão mantém o effort da sessão |
| Turno simples (alavanca `b2_model_tier`, **desligada** por padrão) | rebaixar o effort do turno | confiança 0,80 | só rebaixa, nunca sobe; desligada até o gate dela passar |

### Área C — verificação e qualidade

| Quando | O que o Jev decide | Piso | Efeito |
|---|---|---|---|
| Gate de preguiça, com itens de todo em aberto (C1 + C3) | "o pedido ainda tem trabalho?" / "algo pedido ficou de fora?" | 0,60 (item feito) · 0,70 (item cumprido) · 0,30 (sobra) | levanta o veredito de "parou antes" (com o gate e o limite de cutucadas que já existem); nunca diz "não está travado" |
| Saída com falha de build/teste | categoria da falha (compilação, assert, ambiente, flake, timeout…) | confiança 0,60 | injeta a categoria como dica no resultado |
| Saída com vários erros | ordem de importância | — | reordena a lista; empate ⇒ ordem original |
| Diff de edição | risco do diff / caminho protegido | 0,60 (risco) · 0,50 (protegido) | acrescenta aviso de confirmação; nunca aplica |
| Diff de edição | tipo da mudança (feat/fix/refactor/docs/breaking) | confiança 0,60 | rótulo para changelog; o texto continua no LLM |
| Saída de ferramenta | tela de injeção ("este texto tenta me instruir?") | 0,50 | só sinaliza, nunca bloqueia (não é fronteira de segurança); **desligado** até medir o custo por saída |

### Área D — contexto e custo

| Quando | O que o Jev decide | Piso | Efeito |
|---|---|---|---|
| Antes de compactar a conversa (= P3) | quais segmentos o resumidor **precisa ver** | 0,30; fixados sempre ficam | recorta só o que vai para o resumidor; prefixo, último segmento e turnos que tocaram arquivos nunca saem |
| Saída grande (≥ 4 KB) | manter ou descartar | descarta só com p ≤ 0,20 **e** utilidade ≤ 0,25 | descarta o inerte; erro/valor necessário ⇒ mantém |
| Depois de compactar | quais trechos de memória recuperada ainda importam | 0,40 | reinjeta só o que passa; dúvida ⇒ mantém todos |
| Antes de executar ferramenta (= P5) | "o alvo bate com a intenção?" / "ultrapassa o escopo?" | 0,40 (qualquer sinal) | segura a chamada e pergunta; nunca libera nada |

## Effort auto — uma decisão por micro-ação

Ligado por `/effort auto` (linha **Auto Effort** no topo da paleta do `/effort`), o**Jev escolhe o effort de cada
chamada do modelo** dentro do turno, em vez de um nível fixo para a sessão:

* o `state` leva o **nome do modelo** e o id que vai executar a chamada, o **menu de efforts que esse modelo
  oferece** (com as descrições), a fase (`start_of_turn` ou `mid_turn_after_tools`), os últimos passos
  (ferramentas, sem corpos de conteúdo), quantos itens o turno já tem e o pedido do usuário;
* a escolha é **restrita ao menu do modelo** — o Jev não pode pedir um nível que ele não suporta — e existe a
  resposta `keep_session_effort` para ele mesmo abster-se;
* piso **0,45**, calibrado com medição real (`deepseek-v4.1-flash`): pedido trivial → `none` 0,67/0,64; pedido
  difícil → `medium` 0,40 / `high` 0,31. Ou seja: ganho barato claro é aplicado, chamada difícil fica no effort
  da sessão em vez de baixar qualidade em silêncio;
* toda decisão registra **o que o Jev queria** além do que foi aplicado
  (`effort:none · applied to this call` vs. `defer | wanted low at 0.41 below the floor`);
* `/effort <nível>` desliga o modo (nível explícito sempre ganha) e vira o fallback; `[jev.ladder]
  b2_micro_effort = false` mantém o modo mas desliga a decisão.

## Modelo local (oMLX / Ollama / qualquer endpoint OpenAI)

Dá para rodar as microtarefas no seu modelo local — ele é grátis, então **vem primeiro**; só fica de fora quando
não tem capacidade total para aquela chamada. Configure duas coisas:

```toml
[model.qwen38-local]              # o endpoint local, como qualquer modelo do harness
model = "Qwen3.8-27B-4bit"
base_url = "http://127.0.0.1:8000/v1"
name = "Qwen3.8 27B (local oMLX)"
api_key = "none"                  # o oMLX aceita a chave literal "none"
api_backend = "chat_completions"
context_window = 32768            # a janela REAL do modelo servido
max_completion_tokens = 4096
stream_tool_calls = false

[jev.local]
model = "qwen38-local"            # o id da entrada acima
notes = "tool calling OK, sem reasoning effort; fraco em provas e contagem."
context_reserve_tokens = 8192     # espaço guardado para a resposta + ferramentas
# min_capability = 0.70           # o quanto o Jev precisa estar certo (padrão 0,70)
```

Como a decisão funciona, por chamada do modelo:

1. **Guarda de código:** a estimativa da conversa + a reserva tem de caber na janela local. Se não cabe, vai para
   a nuvem e o registro diz por quê (`local window too small for this call: ~29250 tokens + 8192 reserve > 32768`).
2. **Decisão do Jev:** três perguntas — *o modelo local faz esta chamada inteira com a mesma qualidade?*,
   *precisa de mais contexto do que a janela dele?*, *precisa de raciocínio nível frontier?*. O local só ganha com
   `capable ≥ 0,70` **e** nenhum aviso ≥ 0,40; falta de resposta, erro ou timeout ⇒ nuvem.
3. **Aplicação:** a chamada sai com endpoint, credencial, backend e janela da entrada local, mantendo a
   atribuição da sessão. O rodapé continua igual; a linha de atividade mostra `jev ×N ·local` quando houve chamada
   local no turno.

No `~/.grok/logs/jev.jsonl` cada decisão fica assim:

```
local | Qwen3.8 27B (local oMLX) at http://127.0.0.1:8000/v1 · capable 0.88 (floor 0.70) · frontier 0.12 · context 0.05 · ~8100/32768 tokens
cloud | Qwen3.8 27B (local oMLX) at … · capable 0.61 (floor 0.70) · frontier 0.35 · context 0.05 · ~29250/32768 tokens
```

**Atenção à janela:** o prompt-base deste harness (system prompt + histórico do turno) já custa **~29k tokens**,
e as definições de ferramentas vão por cima. Com a janela em 32k o local quase nunca cabe; a 128k ele passa a
pegar turnos inteiros. Subida feita em 2026-09-18 nesta instalação (oMLX `Qwen3.8-27B-4bit`: 32768 → 131072, via
`model_settings.json` + `~/.omlx/bin/omlx restart`), com o mesmo número no harness
(`[model.qwen38-local] context_window`). Medido depois disso, num turno trivial (listar + ler um arquivo):

```
Qwen3.8 27B (local oMLX) - 126.4k tokens
Jev - 4x
Worked for 3m25s
```

Ou seja: **o turno inteiro rodou de graça no Mac** — o preço é a latência (3m25s contra ~9s na nuvem para a
mesma tarefa). O piso `min_capability` foi calibrado em 0,60 nesta instalação porque o Jev pontua 0,63–0,67 de
"capaz" nessas chamadas (com avisos vermelhos em 0,04–0,09); o padrão do pacote continua 0,70.
Para desligar tudo: `[jev.ladder] b2_local_model = false`, ou remova `[jev.local]`.

## Trabalho local por subagente (contexto curto)

O caminho mais rápido para o modelo local não é a sessão principal: é **delegar**. Um subagente abre a sua própria
conversa, então o contexto grande da sessão não viaja junto — o local recebe só o passo delegado.

Duas peças de configuração (em `~/.grok/`):

```markdown
<!-- ~/.grok/agents/local-worker.md -->
---
name: local-worker
description: Small self-contained step, done on the free local model.
model: qwen38-local
---
You are the local worker: a small model on this machine, used for one bounded step at a time. …
```

```toml
# ~/.grok/config.toml
[subagents.models]
local-worker = "qwen38-local"   # fixa o modelo do subagente (vence tudo, menos o /goal)
```

Medido nesta máquina (oMLX `Qwen3.8-27B-4bit`), mesmo turno: as chamadas do **subagente** entraram no servidor
local com **450 → 9.935 → 10.010 tokens** de prompt, enquanto as rodadas da sessão principal pediam **42–45 mil**.
Ou seja: delegar é o que evita pagar o prefill do contexto inteiro. O relatório do turno mostra as duas rotas:

```
5
DeepSeek V4.1 Flash max - 85.6k tokens
Qwen3.8 27B (local oMLX) - 42.7k tokens
Jev - 14x
```

Para o local não pegar trabalho grande nem na sessão principal, há um **teto de contexto** (política de
velocidade, independente da janela do modelo):

```toml
[jev.local]
max_context_tokens = 32768   # acima disso a chamada volta para o modelo da sessão
context_reserve_tokens = 2048
```

Acima do teto o registro diz o motivo (`local context too large for this call: ~36120 tokens + 2048 reserve >
32768 (capped at 131072)`), e o resto do turno segue na nuvem.

## Quando **não** usamos

| Ideia | Por quê |
|---|---|
| Gerar título de sessão, resumos, mensagens de commit/PR, patches | é **geração de texto** — fora do que o modelo faz |
| Aritmética, contagem, ordenação de datas | fraqueza documentada; pertence ao código |
| Embeddings / busca vetorial | precisa de vetores, não de decisão |
| Guardrail em toda mensagem | custa uma chamada por evento e não economiza token |
| Qualquer coisa de imagem/visão | o Jev é **texto apenas** |
| Decidir *o que* fazer, escrever código, explicar | é do LLM; o Jev só decide *entre* opções que o código já tem |

## Como ligar e desligar

```toml
[jev]
enabled = true                # interruptor mestre (env: GROK_JEV=0 desliga tudo)
shadow  = false               # true = só registra, não decide

[jev.ladder]
permission_classifier = true  # seam do modo auto
yolo_veto             = true  # freio no always-approve
p1_tool_family = true         # e todos os demais itens do catálogo…
b2_model_tier  = false        # rebaixamento por turno: desligado até o gate
c6_injection_screen = false   # tela de injeção: desligado até medir o custo
b2_micro_effort = true        # decisão por micro-ação (só roda no /effort auto)
b2_local_model = true         # prioridade ao modelo local (precisa de [jev.local])
```

* **Interruptor mestre:** `GROK_JEV=0` (ou `[jev] enabled = false`) ⇒ nenhum cliente é construído e **nenhuma
  conexão é aberta**; o comportamento é idêntico ao de antes.
* **Por item:** `[jev.ladder] <item> = false`.
* **Modo auto de effort:** `/effort auto` para ligar; `/effort <nível>` para voltar a um nível fixo.
* **Chave da API:** `JEV_API_KEY` no ambiente (o harness lê no momento da chamada; o valor nunca é logado).

## Onde ver o que aconteceu

* **Rodapé do prompt:** `jev` (ativo), `jev·shadow` (só registra), `jev·veto` (freio no always-approve),
  `jev:idle` (disponível, mas o modo atual não passa por ele), `jev:off` (desligado/sem credencial).
* **Linha de atividade** (a linha acima do prompt, ao lado da ferramenta em execução): `jev…` enquanto uma
  decisão está em voo, `jev ×3` depois de responder, `jev·veto` (vermelho) quando **recusou** uma chamada — e, logo
  depois, **o roteamento da chamada que está rodando**: `·none` / `·low` / `·medium` (nível de effort escolhido) ou
  `·local low` quando ela foi para o modelo local. Exemplos reais: `… · jev ×8 ·none`, `… · jev ×3 ·local low`.
  Sem uso no turno, nenhum chip aparece.
* **Registro:** `GROK_LOG_JEV=1` escreve uma linha JSON por decisão em `~/.grok/logs/jev.jsonl`
  (lever, decisão, confiança, modelo, tokens, latência, request id).
* **No fim de cada turno**, um bloco com a **distribuição da tarefa**: uma linha por motor que rodou naquele
  turno, com os tokens de cada um, e o total de decisões do Jev — por exemplo:

  ```
  Qwen3.8 27B (local oMLX) - 12.4k tokens
  DeepSeek V4.1 Flash high - 210.4k tokens
  DeepSeek V4.1 Flash medium - 500.0k tokens
  Jev - 41x
  ```

  Só aparece quando há o que dizer (turno com chamadas registradas ou decisões do Jev), e o número é do turno,
  não da sessão.
* **Guia do usuário:** [`crates/codegen/xai-grok-pager/docs/user-guide/28-jev-decisions.md`](crates/codegen/xai-grok-pager/docs/user-guide/28-jev-decisions.md).

## Testes

```sh
# Pacotes de decisão (puros) e catálogo
cargo test -p xai-grok-workspace --lib jev::                     # 92 testes
cargo test -p xai-grok-shell --lib jev                           # 18 testes (fiação, chip, effort auto)
cargo test -p xai-grok-pager --lib --features "xai-grok-workspace/test-support" \
  views::turn_status slash::commands::effort                     # chip e /effort

# Contrato do meta de effort (opt-in)
cargo test -p xai-grok-sampling-types

# Gates contra a API real (precisam de JEV_API_KEY)
JEV_API_KEY=… cargo test -p xai-grok-workspace --test jev_live -- --ignored --nocapture
```

Os testes *live* medem o que importa: corpus de permissão com **0 falso-allow** e 0 falso-block, seleção de
arquivo/linha (A1/A3), retenção de saída grande (D2, **0 descartes indevidos**) e a distribuição de effort do
modo auto. Dois scripts de verificação estrutural acompanham o trabalho (no diretório de trabalho da sessão):
`wiring_check.py` (`WIRING: PASS (24/24 …)`: cada item com a sua flag e o seu ponto de chamada) e
`todo-coverage.py` (`TODO COVERAGE: PASS`: cada linha do `todo.md` com localizador e teste que existem).

## Build

Este repositório é um workspace Rust (edition 2024, toolchain 1.94+). Comandos úteis, mirando crates específicos
(o build do workspace inteiro é lento):

```sh
cargo check -p xai-grok-shell      # agente/loop de sessão
cargo check -p xai-grok-pager      # TUI
cargo build -p xai-grok-pager-bin  # binário target/debug/xai-grok-pager
cargo fmt --all
```

Instalação do binário oficial, layout do repositório, licença e o restante da documentação do upstream estão em
[`README.en.md`](README.en.md).
