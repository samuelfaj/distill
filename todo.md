# Distill: menor custo monetário por tarefa concluída

Estudo e plano de execução — 21/09/2026. **Status em 22/09/2026: checkpoints funcionais aceitos; T01–T21 continuam abertos e nenhum ganho financeiro foi validado.**

> **Status atual — checkpoints funcionais aceitos:** `/goal` recupera overflow
> de contexto dentro dos budgets (`b36b39e2`); compressão utility → worker e
> limite de falhas opcionais estão em (`7f5403a1`, `f40f1d63`); o ledger
> preserva custos free conhecidos (`44a5d951`); as tentativas auxiliares de
> `title_refresh` são rastreáveis (`b725d112`); e os checkpoints anteriores de
> skill e PDF permanecem registrados (`f508eeb7`, `5b99fd6c`). `8aabfc50` fica
> identificado somente como baseline histórico de read; a invalidação atual de
> read/memo está em (`d80df06e`). A captura de custo USD em Responses
> está em (`2895b95b`) e o teste com endpoint local simulado passou com custo/modelo; este
> checkpoint também fecha a preservação de `response.id` como
> `ConversationResponse.message_id`, com a prova focada de `resp_openrouter`
> aprovada.
>
> **Prova ainda aberta:** T01–T21 não estão concluídos. Não há prova de custo
> runtime matched/pareado nem revisão independente; roteamento econômico,
> effort e calibração continuam pendentes. Os smokes anteriores permanecem
> não pareados: Distill inconclusivo, Pi aceito apenas com custo tarifário
> estimado e billing real desconhecido (HTTP 404). Baseline, aceitação
> financeira e comparação econômica continuam pendentes. A auditoria de billing
> do `initialSummaryGenerator`/title continua aberta: há chamada direta no
> cliente, sem ledger óbvio confirmado. E2 display aux e E3 Task Output
> permanecem dirty/em progresso e não são aceitos.

> **Anotação de steering — overflow e economia de saída (22/09/2026):** a
> captura de tela mostra um sintoma de overflow no `/goal` e rótulos de modelos
> na UI; isso não prova qual modelo/endpoint respondeu. A rota efetiva precisa
> ser registrada pelo request/client metadata. Para conteúdo útil que possa ser
> reduzido, a ordem exigida é utility elegível → worker configurado e limitado
> (preservando pins) → saída necessária fiel/original recuperável se ambos
> falharem. Contar cada chamada, rejeição, fallback e reabertura uma vez; não
> usar um fallback pago de reasoning só para resumir.

## 1. Objetivo e critério de sucesso

Fazer o Distill concluir as mesmas tarefas, com a mesma qualidade exigida, gastando menos dinheiro. Usar código determinístico, Jev, utility models e workers para evitar chamadas caras ou diminuir seu contexto e esforço. Reservar o reasoning model para as partes em que ele reduz o custo total esperado, inclusive evitando retrabalho.

Terceirizar mais é um meio. A métrica principal será:

```text
custo por tarefa aceita = custo de TODAS as execuções da amostra
                         / número de tarefas concluídas e aceitas
```

O numerador inclui execuções fracassadas, Jev, utility models, workers, reasoning, verificadores, subagentes, compactação, chamadas auxiliares, tentativas rejeitadas e trabalho especulativo descartado. Reportar também taxa de sucesso, custo por tarefa iniciada, custo por tarefa individual e latência p50/p95. Uma redução de custo acompanhada de tarefas abandonadas ou incorretas não comprova eficiência.

**Aceite final:** custo por tarefa aceita menor que o Distill de referência, preservando os requisitos e a qualidade no conjunto de avaliação congelado. Para afirmar superioridade ao Pi, comprovar também essa comparação com tarefas, capacidades e condições equivalentes. Fixar a margem de não inferioridade de qualidade antes de executar a avaliação; se os dados forem insuficientes, registrar resultado inconclusivo.

Não há uma porcentagem de economia prometida. Os maiores candidatos a ganho são: evitar raciocínio caro desnecessário; delegar tarefas com contexto pequeno; reduzir contexto recorrente; evitar chamadas e tentativas sem retorno econômico.

## 2. Base examinada e limites

| Componente | Base do estudo | Cobertura e limite |
| --- | --- | --- |
| Distill | Checkout `jev-build`, HEAD `2c87833e5bb47baf68e45496e777318444d7d5f4`, incluindo alterações locais presentes durante a leitura | Caminhos de prompt, ferramentas, skills, Jev, modelos/esforço, workers, utility tasks, saídas, memória, compactação, cache, contabilização, retries e testes associados. O checkout estava em edição concorrente: o HEAD sozinho não reproduz os arquivos locais examinados. |
| Pi | [Commit `1a584a7a56eb5e7b4ff8ccbd46430f1533282eed`](https://github.com/earendil-works/pi/tree/1a584a7a56eb5e7b4ff8ccbd46430f1533282eed), clone de leitura | Núcleo de `coding-agent`, loop de `agent`, adaptadores e custos de `ai`, documentação de extensões e mecanismo de avaliações. |
| Jev | Cliente e integração Rust deste repositório; documentação pública oficial consultada em 21/09/2026 | Primitivas, estado, batching, confiança, limites, modelos, seleção de skills e cascatas. Não há acesso neste estudo aos pesos, treinamento ou implementação privada do serviço. |
| Benchmark da imagem | [HarnessTax: metodologia e resultados](https://raw.githubusercontent.com/HarnessTax/HarnessTax.github.io/main/data/blog/harness-x-model.0af74054a7.json) e [dados de contexto](https://raw.githubusercontent.com/HarnessTax/HarnessTax.github.io/main/data/charts/system-agent-context-swe.06f521bdb1.json) | Evidência externa, com versões/configurações próprias. Não é uma medição deste Distill nem uma ablação que atribua causalmente a economia a cada recurso do Pi. |

“100%” aqui é a cobertura das superfícies que participam do custo, explicitadas na matriz abaixo. **Não é uma alegação de leitura de cada linha dos projetos, auditoria integral de segurança ou validação de todos os caminhos em execução.** A economia efetiva, os preços da conta, a disponibilidade dos endpoints e a distribuição real das tarefas continuam por medir.

Fontes locais históricas: [list.md](list.md), [plano anterior](plan/plan.md), [medição anterior do Jev](plan/jev-measurement-report.md), [token saver](docs/token-saver.md) e [roteamento](docs/jev-routing.md). Foram usadas como orientação e confrontadas com implementação. Uma flag, função, teste unitário ou item marcado como implementado não comprova que o caminho esteja ativo no payload final nem que economize dinheiro. Resultados sintéticos antigos não são resultados financeiros atuais.

Este arquivo preserva o plano, o histórico e os critérios de aceite originais. Nenhuma alteração foi feita em `list.md`, `plan/` ou outros documentos nesta atualização. Na execução futura, congelar primeiro a revisão e a configuração reais usadas em cada experimento.

## 3. O que o Pi ensina e onde o Distill pode ir além

O Pi apresenta um núcleo pequeno ao modelo e deixa boa parte da especialização para ferramentas e conteúdo carregado quando necessário. No benchmark citado, o primeiro contexto médio do SWE era aproximadamente 1.972 tokens no Pi, 11.308 no Codex e 27.011 no Claude Code; isso inclui o conteúdo inicial enviado, não apenas o system prompt. As versões do benchmark eram Pi 0.85.1, Codex 0.146.0 e Claude Code 2.1.224, diferentes do snapshot atual examinado. O gráfico mostra uma oportunidade, não equivalência garantida de qualidade em toda tarefa.

| Superfície | Pi: comportamento observado | Distill: estado observado e oportunidade | Ações |
| --- | --- | --- | --- |
| Prompt base | Quatro ferramentas padrão (`read`, `bash`, `edit`, `write`); regras condicionadas às ferramentas e contexto do projeto separado. [P1] | Templates maiores e várias fontes de contexto. Medir o prompt renderizado: `prompt.md` e `apply_patch_prompt.md` são alternativas, não dois custos a somar. [D1] | T01, T05 |
| Schemas e descoberta | Núcleo pequeno; extensões podem acrescentar ferramentas. Isso não torna toda instalação do Pi pequena. [P1], [P5] | MCP já fica fora da lista inicial de schemas por `tool_definitions_builtins_only`; há `search_tool`. A poda Jev atual não pode economizar novamente schemas já ausentes. [D2], [D3] | T06, T07 |
| Skills | Lista nome, descrição e caminho das skills visíveis; lê o corpo quando necessário. Não encontrei poda semântica automática do índice nesse caminho padrão. [P1] | P6 sugere uma skill entre as primeiras 40, com descrições limitadas a 120 caracteres; não remove o catálogo completo. [D4] | T06 |
| Leitura e pesquisa | Leitura por intervalo; truncamento com limite de linhas/bytes e indicação de continuação. [P2] | Já há limites, seleção de parágrafos de documentos, redutores e armazenamento da saída original. Otimizar seleção e paginação antes de pedir resumos. [D8], [D9] | T12, T13 |
| Resultado de ferramentas | Bash truncado aponta para arquivo; edição/escrita devolvem confirmação curta; detalhes de UI ficam separados do conteúdo enviado ao modelo. [P2], [P3] | Alguns resultados, como `SearchReplace`, já separam detalhes de apresentação. Evitar recomprimir ou reintroduzir diffs completos nesses caminhos. [D10] | T12 |
| Contexto entre rodadas | `transformContext` é uma extensão possível, não uma compressão semântica universal já ligada. [P4] | Reuso de leituras, redução de saídas e seleção para compactação já existem. Garantir referências recuperáveis e invalidar estado obsoleto. [D8], [D9] | T12–T14 |
| Compactação | Preserva trecho recente, resume o anterior e rastreia arquivos; no snapshot, reserva 16.384 e mantém cerca de 20.000 tokens recentes. Serialização para resumo limita tool results a 2.000 caracteres. [P6] | Já há estratégias, recorte Jev e compactação em duas passagens com prefire. A oportunidade é custo/modelo/necessidade e fidelidade, não outro compactador. [D11] | T14 |
| Cache | Adaptadores tratam cache conforme o provedor; custos distinguem input, output, cache read/write e tiers. [P7] | Chamadas auxiliares já alinham prefixo, ferramentas e chave ao pai. Alterar schema/modelo/prefixo pode custar mais que manter tokens em cache. [D12] | T01, T05, T09, T15 |
| Modelo e effort | Hooks permitem ajustar modelo/thinking por turno ou request. Isso não constitui um roteador econômico automático padrão. [P4] | Worker + effort já são escolhidos juntos; uma decisão posterior para utility/local pode substituir essa rota. Consolidar quando o mesmo estado permitir, respeitando políticas por candidato. [D4] | T07, T09, T10 |
| Delegação | Extensibilidade permite implementar fluxos adicionais; comparar o núcleo do Pi com todo recurso do Distill pode favorecer um lado por diferença de escopo. [P5] | Troca de executor de uma rodada carrega a conversa; utility task direta carrega payload limitado. Há infraestrutura de subagentes e restrições para sessões retomadas. [D4], [D7], [D13] | T08, T11 |
| Utility models | Não há, nos caminhos padrão examinados, a cascata econômica Jev + nossos utility models. | Cliente de tarefas fechadas, catálogo, guardas e modelos de fallback já existem. Catálogo extenso não equivale a uso validado. [D6], [D7] | T04, T08 |
| Encerramento | Uma ferramenta pode solicitar término; o loop só encerra assim se todos os resultados do lote forem terminais. É capacidade de extensão. [P4], [P5] | Evitar uma rodada adicional em ações determinísticas conclusivas pode economizar; tarefas de código continuam exigindo aceite e verificação. | T17 |
| Retries e concorrência | Políticas de retry, cancelamento e ferramentas serializadas/paralelas fazem parte do custo. [P4], [P6] | Há limites em várias camadas; utility generations compartilham mutex global e seus contadores não impõem limite por lane. Evitar falhas repetidas e trabalho descartado. [D7], [D14] | T16, T18 |
| Imagens e documentos | Redimensionamento e limites específicos de imagens. [P2] | Distill já possui transcode/limites e PDF em texto ou páginas. Jev é textual; não substitui percepção visual. [D15], [J2] | T19 |
| Contabilização | Estatísticas somam chamadas do agente, uso reportado pelas ferramentas, compactações e resumos de branches. [P8] | `UsageLedger` já é rico; ledger Jev e logs auxiliares têm escopo diferente. Reconciliar todos os gastos sem duplicar. [D5], [D7], [D12] | T01 |
| Avaliação | Infra de evals registra variantes, repetições e execuções ausentes; não tratar dado faltante como zero. [P9] | Reaproveitar testes e logs existentes; acrescentar só o comparador necessário para o custo real. | T02, T20, T21 |

### Achados que afetam diretamente o plano

1. **A seleção de ferramentas precisa chegar ao payload real.** O caminho normal começa com builtins; `tool_family_questions` só encontra famílias podáveis presentes. Confirmar quais exposições ainda custam tokens — inclusive descrições de servidores e índice de descoberta — antes de ampliar P1. [D2], [D3]
2. **Worker e utility são trabalhos diferentes.** O worker que substitui uma rodada recebe contexto amplo; a tarefa utility direta recebe instrução e payload restritos. O segundo mecanismo tende a ter menor custo de handoff, hipótese a medir. O primeiro pode continuar útil quando aproveita contexto/cache e ferramentas. [D4], [D7], [D13]
3. **Os validadores atuais não bastam para ligar todo o catálogo.** `ClosedSet` aceita substring na primeira linha; `JsonObject` só exige objeto JSON; `Spans` confere os spans encontrados, sem exigir que existam. Uma resposta sintaticamente válida pode estar errada. [D6]
4. **Compressão generativa não cobre qualquer saída grande.** O caminho observado está limitado a aproximadamente 24–32 KiB, exclui certos documentos/saídas exatas e exige validação de literais e Jev. Já P2 atua em leitura integral de `.md`/`.txt`, não em poda geral de código. [D8]
5. **Alguns estados Jev são grandes justamente quando a economia seria útil.** O recorte de compactação serializa texto de segmentos e pode desistir ao exceder 16 KiB. Enviar índices/excertos limitados permite avaliar relevância sem enviar novamente todo o material. Isso não autoriza remover fatos obrigatórios. [D11]
6. **A telemetria existente é aproveitável, mas precisa de conciliação.** `utility_usage` ocorre antes de rejeitar uma resposta, o que é correto para registrar gasto; essa linha de log não prova incorporação ao custo exibido da tarefa. Recaps/títulos/resumos têm chamadas e logs próprios. [D5], [D7], [D12]

**Defaults não equivalem a execução:** no `harness_default` examinado, roteamento worker/local, effort automático, sugestão de skills e compressão barata estão ligados; `e_cheap_task`, `e_cheap_agent`, `e_lane_choice`, `e_prompt_blocks` e `e_retention` estão desligados. Configuração, credenciais e guardas ainda determinam quais caminhos realmente executam. Registrar a configuração efetiva no baseline; não ativar o catálogo inteiro para presumir economia. [D18]

## 4. Papel exato do Jev

**Sim: usar Jev para escolher quais ferramentas, skills, memórias e trechos merecem ser apresentados é uma das aplicações centrais.** Há um [cookbook oficial de seleção de skills][J4]. Ele faz seleção em etapas e mantém o índice original estável para cache; remover o índice, como propomos experimentar, é uma adaptação adicional que precisa de medição de economia, recall e cache.

| Trabalho | Quem executa primeiro | Jev pode ajudar com | O que permanece em código ou em outro modelo |
| --- | --- | --- | --- |
| Descoberta de capacidades | Índice existente + candidatos pelo contexto da tarefa | Relevância das famílias, skills e ferramentas; aceitar múltiplas ou nenhuma | Carregar schema/corpo; dependências; permissões; recuperação do catálogo completo |
| Arquivos, logs, pesquisa e memória | Busca e filtros determinísticos | Ordenar candidatos, relevância, suficiência da evidência, prioridade de erros | Leitura exata, números de linha, paths, hashes, comandos, fatos completos |
| Rota da próxima ação | Candidatos configurados e compatíveis | Adequação, ambiguidade, necessidade de contexto/raciocínio e sinais de risco | Preço, orçamento, compatibilidade, cálculo econômico e política do usuário |
| Extração e classificação | Parser quando possível; Jev para classes semânticas | Escolher classe/ID; validar campos contra fontes; detectar ausência de suporte | Utility gera texto/JSON quando necessário; schema e valores verificáveis são conferidos em código |
| Resumos | Utility model sobre conteúdo limitado | Se faltam fatos relevantes e quais fontes precisam ser reabertas | Produzir o resumo; manter fonte; comparar evidência; escalar quando necessário |
| Falhas e progresso | Exit codes, erros tipados, diff e testes | Categorizar erro não estruturado, detectar repetição sem progresso ou ambiguidade | Decidir retry idempotente, executar testes e comprovar conclusão |
| Verificação de mudanças | Ferramentas e testes | Priorizar revisão e indicar trechos que merecem análise mais forte | Verificar requisito e comportamento; Jev não substitui o aceite da tarefa |

Jev retorna decisões estruturadas, não texto livre para resumir ou escrever código. As perguntas de um lote são avaliadas independentemente sobre o mesmo estado; só combinar perguntas cuja resposta não dependa da outra. Um segundo estágio continua necessário quando as opções dependem do resultado anterior. [J1], [J3]

`confidence` de Choice/Score mede concentração da distribuição; não é a probabilidade observada de o worker concluir a tarefa. Noul tem probabilidade, sem esse campo de confiança. Calibrar por pergunta, domínio, idioma, endpoint e versão; não reaproveitar um limiar entre primitivas ou wrappers como se fossem equivalentes. [J5]

Na documentação consultada, Jev 1.13 custa US$ 0,042 por milhão de tokens de entrada, com saída sem cobrança, e aceita apenas texto. Os limites publicados são por tokens e distinguem estado + maior pergunta do total de perguntas. Os limites em bytes do nosso cliente são outro contrato. Usar preços/limites do endpoint efetivamente utilizado e registrar a versão respondente; aliases podem mudar. [J2]

O próprio fornecedor aponta limitações em cálculo numérico, estados longos e interpretação literal. **Jev julga critérios atômicos; Rust faz a conta.** Não solicitar ao Jev que calcule o modelo monetariamente ótimo a partir de uma tabela de preços. Critérios e delimitação de dados ajudam, mas não tornam o estado imune a instruções adversariais. [J6]

## 5. Política proposta de execução

### Escolher pela próxima unidade de trabalho

Não classificar toda a conversa como “fácil” ou “difícil”. Uma tarefa complexa pode alternar diagnóstico com raciocínio forte, edição mecânica barata, testes determinísticos e revisão localizada. A classificação da unidade seguinte usa objetivo atual, estágio, evidências recentes, arquivos envolvidos e critério de aceite — não somente o último pedido humano.

```text
unidade de trabalho
  → código determinístico, quando resolve completamente
  → candidatos elegíveis: utility direto | worker limitado | reasoning
  → sinais Jev, somente quando mudam uma decisão útil
  → escolha econômica em código, com modelo + effort + contexto
  → execução e verificação proporcional
  → resultado aceito OU nova ação/escalada com evidência da falha
```

Isso não exige passar sequencialmente por todos os modelos. Pode ser mais barato começar no reasoning quando já há evidência de que um worker falhará, ou manter um modelo caro com prefixo quente por uma rodada curta.

| Tipo de trabalho | Rota inicial candidata | Effort inicial a avaliar | Critério de escalada |
| --- | --- | --- | --- |
| Contar, filtrar JSON, diff, deduplicar, executar comando, avaliar exit code | Código/ferramenta | Nenhum LLM | Entrada não estruturada ou requisito não resolvido pelo algoritmo |
| Escolher skill, ferramenta, documento, classe ou candidato | Código + Jev se houver ambiguidade semântica | Não usar generative reasoning para uma escolha fechada sem necessidade | Evidência insuficiente ou baixa qualidade medida do seletor |
| Extrair campos, resumir log limitado, produzir título | Utility direto, sem harness de agente | `none`/mínimo realmente suportado | Falha de schema, fatos ausentes, truncamento ou falta de suporte na fonte |
| Patch localizado com contrato e referências suficientes | Worker com ferramentas limitadas ao trabalho | Baixo; médio se o resultado medido justificar | Falha relevante, dependência descoberta, requisito ambíguo ou ausência de progresso |
| Diagnóstico difícil, decisão arquitetural necessária, mudança ampla | Reasoning para a decisão delimitada | Menor nível que passa nesse tipo de tarefa; alto sob evidência | Acrescentar contexto/esforço apenas se resolver a incerteza detectada |
| Testes, formatadores e validação objetiva | Ferramentas determinísticas | Sem modelo para reinterpretar sucesso óbvio | Falha que exija diagnóstico; priorização pode usar Jev |
| Revisão e resposta final | Critérios objetivos + modelo conforme risco/complexidade | Sem ronda forte obrigatória após toda utility task | Aceite não demonstrado ou questão semântica material pendente |

Os nomes de effort não são equivalentes entre modelos. `none`, parâmetro ausente e reasoning desabilitado podem produzir comportamentos diferentes. Não atribuir a um modelo suporte a um nível apenas por sua família ou nome. Reutilizar menus e adaptadores existentes, validar o request efetivamente transmitido e respeitar efforts fixados pelo usuário. [D4], [D16]

### Conta econômica

Para cada rota elegível `r`, estimar:

```text
E[custo_restante(r)] = custo_decisão
                    + custo_execução(r, modelo, effort, contexto, cache)
                    + custo_verificação(r)
                    + prob_falha_observada(r, tipo_de_trabalho)
                      × custo_esperado_de_recuperação
                    + custo_adicional_de_handoff_e_releitura
```

Os termos devem ser disjuntos: não contar novamente o prefixo frio em “handoff” se já entrou na execução. A recuperação inclui escaladas e verificação subsequentes; a expectativa deve ser atualizada quando há nova evidência. Até haver dados confiáveis, usar uma política pequena de rotas validadas e abstenção, não inventar probabilidades a partir de `confidence`.

Comparar com a alternativa disponível, inclusive continuar no modelo atual. Um gate determinístico pode dispensar Jev se não houver candidatos alternativos ou economia plausível. Não gastar uma classificação paga só para confirmar uma escolha já imposta pela configuração.

Para estimar custo de um request quando o provedor não informa o valor:

```text
input_sem_cache = input_total - cache_read - cache_write
custo = input_sem_cache × tarifa_input
      + cache_read × tarifa_cache_read
      + cache_write × tarifa_cache_write
      + output_cobrado × tarifa_output
      + outras_cobranças_documentadas
```

Normalizar conforme cada adaptador: no contrato atual de `TokenUsage`, input já inclui os buckets de cache. Reasoning não deve ser somado novamente se já estiver em output. Diferenciar custo reportado pelo provedor de estimativa com tabela e data; preservar desconhecido quando faltarem dados. Assinaturas, créditos, endpoints gratuitos e modelos locais precisam de relatórios separados de custo marginal/alocado, sem tratar gasto desconhecido como zero. [D5], [D16]

Para compressão, aplicar a mesma regra: só vale pagar por resumo + validação + eventual releitura se isso custar menos que transportar o conteúdo original nas chamadas futuras, considerando cache e o número provável de reutilizações. Evitar compressão de um pequeno resultado que só será usado uma vez.

## 6. TODO priorizado

Cada item abaixo é trabalho futuro. Checkboxes permanecem abertas; constatar que uma função existe não conclui a ação. Dependências indicam o mínimo para implementar/validar o item, não justificam construir toda a infraestrutura antecipadamente.

### P0 — Medir corretamente e tornar a terceirização verificável

- [ ] **T01 — Fechar a conta financeira de uma tarefa, reaproveitando `UsageLedger`.**
  - **Dependências:** nenhuma. **Arquivos:** [D5], [D7], [D12], [D11], adaptadores de usage em [D16].
  - Associar cada tentativa faturável a tarefa/turno/request, papel, modelo respondente, endpoint, effort solicitado/aplicado, cache, status e custo. Integrar Jev, utility rejeitada, subagentes, títulos, recap, compactação/prefire e retries uma única vez; contabilizar geração paga mesmo descartada ou cancelada. Diferenciar falha sem cobrança comprovada de usage ausente.
  - Estender os dados/fluxos atuais somente onde faltar informação; não criar um segundo ledger. Separar custo reportado, estimado e ausente. Reconciliar o ledger Jev com o geral sem duplicar chamadas já incorporadas.
  - **Aceite/evidência:** uma execução conhecida com chamada principal + auxiliary + utility rejeitada + subagente fecha a soma por request; ausência/duplicidade é detectada; fixtures existentes de usage e subagent folding cobrem os caminhos aplicáveis. Comparar uma pequena amostra com registros do provedor antes de anunciar economia.

- [ ] **T02 — Congelar referências e produzir um baseline reproduzível.**
  - **Dependências:** T01. **Base:** [P9], testes existentes do Distill, protocolo da seção 7.
  - Registrar revisão limpa/patch local identificado, configuração efetiva, modelos, endpoint, preços com data, ferramentas, esforços e orçamento de avaliação. Usar tarefas reais representativas e cópias isoladas dos mesmos repositórios.
  - Medir Distill atual, Distill com Jev/utility desligados pelo mecanismo existente e Pi equivalente. O desligado é uma ablação, não substitui o produto atual como baseline. Evitar um framework novo: um runner pequeno que reutilize CLIs, logs e verificadores basta.
  - **Aceite/evidência:** execuções planejadas e recebidas conciliadas, custo completo por tarefa, falhas incluídas, versões reproduzíveis e critérios de sucesso independentes do roteador. Resultado inicial é exploratório até o conjunto final congelado.

- [ ] **T03 — Completar os fatos dos candidatos e os limites de elegibilidade.**
  - **Dependências:** T01. **Arquivos:** [D4], [D17], [D16].
  - Aproveitar o trabalho já presente em `jev_model_facts.rs` e `openrouter_models.rs`: preço, cache, janela, output, ferramentas, imagens, níveis de effort, endpoint, autenticação e atualização. Não duplicar esse catálogo. Benchmarks gerais são pistas de capacidade, não taxa de sucesso do nosso workload ou prova de um effort melhor.
  - Usar modelo/endpoint realmente atendente e política da conta. Tratar a lista utility atual — Ling Flash VL gratuito, variante paga e Qwen Flash — como fallback configurado, não uma ordenação econômica validada. Nenhum modelo desconhecido ganha capacidade/preço por inferência do nome.
  - **Aceite/evidência:** candidato incompatível não é oferecido; dados vencidos/ausentes ficam explícitos; as credenciais e o backend pertencem ao candidato correto; seleção manual e restrições de subagente continuam respeitadas. Para `/goal` repetido, a elegibilidade usa a janela/capacidade do endpoint efetivamente roteado, reserva de output e margem antes do envio, com fixture de worker de janela menor versus modelo exibido e caso-limite input+reserva; metadata ausente conserva rota segura explícita, sem inferir pelo nome. Cobrir com testes existentes de configuração/roteamento e fixtures pequenas das lacunas.

- [ ] **T04 — Fechar os contratos das utility tasks antes de ampliar uso.**
  - **Dependências:** nenhuma. **Arquivos:** [D6], [D7], [D8].
  - Substituir aceitação por substring por parsing exato nos conjuntos fechados; validar campos/tipos obrigatórios no JSON; exigir evidência quando a tarefa pede spans; exigir candidatos válidos quando pede IDs. Um resultado vazio deve ser uma abstenção explícita, não sucesso acidental.
  - Para resumos, preservar fatos necessários ao consumidor, com fonte recuperável. O guarda global de “todos os literais” pode ser conservador demais para resumir; definir contrato por tarefa sem abrir espaço para fabricar paths, comandos, valores ou resultados. Não exigir Jev quando parser, fonte e regra objetiva já bastam.
  - **Aceite/evidência:** casos mínimos que hoje podem passar como `not PASS`, JSON sem campos exigidos e resposta sem spans são rejeitados conforme seu contrato; uma resposta correta passa. Retorno rejeitado conserva o original e seu custo continua contado. Não habilitar o catálogo inteiro em bloco.

### P1 — Reduzir o custo recorrente e delegar unidades pequenas

- [ ] **T05 — Enxugar prompt e descrições mantendo um prefixo estável.**
  - **Dependências:** T01, T02. **Arquivos:** [D1], [D2], [P1].
  - Medir bytes/tokens do request renderizado por seção: sistema, schemas, índice de skills/MCP, instruções do projeto, lembretes, histórico e resultados. Usar `/context` como ponto de partida, sem confundir estimativa de tamanho com cobrança.
  - Remover duplicação de instruções do próprio harness; condicionar explicações a ferramentas/recursos ativos; mover exemplos longos opcionais para descoberta sob demanda. Preservar instruções aplicáveis do usuário/projeto e contratos necessários. Fixar ordem e representação das seções estáveis.
  - **Aceite/evidência:** menor custo de entrada medido em sequências curtas e longas, com e sem cache; tarefas que dependem das regras preservadas continuam corretas. Não escolher a variante só pela menor contagem bruta de tokens.

- [ ] **T06 — Descoberta progressiva de ferramentas, skills e outros recursos com Jev.**
  - **Dependências:** T02, T05. **Arquivos:** [D2], [D3], [D4], descoberta de skills existente; referência [J4].
  - Manter um núcleo operacional pequeno e a ferramenta de descoberta acessível. Reaproveitar os índices atuais para obter candidatos; Jev seleciona os relevantes para o estágio atual. Carregar schemas e `SKILL.md` conforme uso. Não substituir instruções obrigatórias de uma skill por um resumo do utility model.
  - Eliminar o viés das primeiras 40: procurar em todo o catálogo, recuperar candidatos com recall suficiente e permitir múltiplas skills ou nenhuma. Uma skill explicitamente pedida, capacidade já em uso e dependência necessária ficam fixadas. Ao faltar algo, ampliar a busca e tornar o catálogo completo recuperável.
  - Evitar reenviar todos os schemas ao Jev: descritores curtos e IDs estáveis bastam para a seleção inicial. Comparar índice completo em cache versus shortlist estável por fase; não trocar todo o catálogo a cada rodada.
  - **Aceite/evidência:** skill relevante além da posição 40 é encontrada, pedidos com múltiplas capacidades funcionam, recuperação de uma omissão funciona e o request ao modelo principal realmente diminui. Medir custo da seleção/descoberta e cache, não apenas quantidade de ferramentas ocultadas.

- [ ] **T07 — Reduzir o próprio custo e a redundância das decisões Jev.**
  - **Dependências:** T01, T03. **Arquivos:** [D4], [D18], [D11]; referências [J1], [J3], [J6].
  - Reusar `ask_items` e memoização existentes. Agrupar perguntas independentes sobre o mesmo estado: adequação dos candidatos e effort por candidato, por exemplo. A decisão utility/local posterior não deve pagar novamente pelo mesmo estado se puder integrar esse lote. Não juntar perguntas causalmente dependentes.
  - Criar estados pequenos específicos à decisão: objetivo, etapa, metadados, erro relevante, orçamento e candidatos. Descartar repetições e filtrar deterministicamente antes de chamar. Usar limites reais de bytes/tokens; não apenas aumentar o teto para acomodar conversas inteiras.
  - Reusar decisão enquanto estado, conteúdo, catálogo, modelo, critérios e versão forem os mesmos. Invalidar em mudança relevante, inclusive compactação que remova o contexto necessário. Se não houver alternativa elegível ou potencial de economia, decidir em código sem chamada.
  - **Aceite/evidência:** requests e tokens Jev menores para o mesmo resultado; fallback simples em indisponibilidade; nenhuma decisão reutilizada após alteração da evidência. A redução aparece no custo total e não esconde erros de seleção.

- [ ] **T08 — Colocar utility models em tarefas fechadas de alto retorno.**
  - **Dependências:** T01, T03, T04, T07. **Arquivos:** [D6], [D7], [D8], [D12].
  - Priorizar extração estruturada de saída não parseável, seleção de trechos apoiada em fonte, resumo de logs volumosos e títulos/resumos de apresentação. Classificação pura pode ir direto a Jev; parsing simples fica em código. Conectar somente tarefas com consumidor real e verificador suficiente.
  - Enviar apenas a instrução e o payload necessário, sem conversa, catálogo, ferramentas de edição ou loop de agente. Limitar resposta ao contrato; selecionar um utility compatível pela evidência de custo/sucesso. A chain gratuita→paga deve registrar qual modelo respondeu e o custo de esperas/falhas.
  - Começar sem reasoning quando suportado e comprovado suficiente. Não executar `best_of`/múltiplas amostras por padrão; uma nova tentativa precisa melhorar o custo esperado.
  - **Aceite/evidência:** para cada tarefa ligada, registrar elegível→chamada→aceita/rejeitada→usada, custo e chamadas principais evitadas. Quando houver conteúdo útil elegível, provar a ordem utility primeiro → worker configurado limitado em indisponibilidade/rejeição/unsupported → saída necessária fiel ou original recuperável se ambos falharem, preservando model/effort/auth pins e sem fallback pago de reasoning apenas para resumir. Cada chamada, rejeição, fallback e reabertura entra uma vez no ledger; manter ligada somente se a economia líquida e a qualidade forem demonstradas.

- [ ] **T09 — Escolher rota pelo custo restante esperado, em um único ponto existente.**
  - **Dependências:** T01, T02, T03, T04, T07. **Arquivos:** [D4], [D14], catálogo de routing em [D18].
  - Integrar a escolha entre reasoning, worker e utility ao fluxo atual, com adequação sinalizada pelo Jev e decisão numérica em Rust. Evitar uma sequência de escolhas que se sobrescrevem e ocultam por que o executor final venceu.
  - Usar contexto/cache reais e previsões medidas de tokens, sucesso e recuperação por tipo de unidade. Começar com uma tabela pequena de políticas comprovadas; desconhecido conserva a rota segura vigente. Evitar oscilação de modelo por diferenças pequenas e incertas de custo.
  - Distinguir trocar o modelo da rodada, executar uma utility task e delegar um subagente. Custos e capacidades são distintos; a rota final precisa registrar sua justificativa verificável. Compatibilidade de histórico/protocolo e autenticação continuam obrigatórias.
  - **Aceite/evidência:** replay de decisões explica a escolha e inclui todos os custos; um worker barato que causa retrabalho deixa de vencer; uma rota barata competente é efetivamente usada. Avaliação pareada supera baseline por tarefa aceita, não somente por chamada.

- [ ] **T10 — Calibrar effort por modelo e tipo de trabalho.**
  - **Dependências:** T02, T03, T09. **Arquivos:** [D4], [D16], [D17].
  - Comparar os níveis realmente suportados para utility, edição localizada, diagnóstico e revisão. Selecionar o menor custo total entre os níveis que atendam ao requisito de qualidade; baixo effort não significa automaticamente menor custo após retries.
  - Manter o effort fixado pelo usuário em cada papel e heranças explícitas de subagentes. Em `auto`, reservar aumento para ambiguidade/falha relevante; um diagnóstico resolvido não obriga a manter alto effort nas etapas mecânicas seguintes.
  - Tratar limite de output separadamente do effort: impedir respostas desnecessariamente longas sem truncar tool calls, patches ou JSON e provocar recuperação mais cara. Confirmar que o adaptador transmite o nível escolhido.
  - **Aceite/evidência:** custo/qualidade por par modelo×effort com versão e endpoint; saída necessária completa; níveis não suportados nunca são enviados. Em `/goal` repetido, input + reserva de output + margem devem caber na capacidade efetiva antes de enviar e ser reavaliados após compactação/rota; pins de effort/output do usuário permanecem. Não concluir que benchmark geral do modelo calibrado em outro effort vale para todos os níveis.

- [ ] **T11 — Delegar workers com contexto delimitado e evitar viagens ao pai.**
  - **Dependências:** T03, T04, T09, T10. **Arquivos:** [D13], [D4], infraestrutura de spawn existente.
  - Reusar o worker/subagente existente para receber objetivo único, critérios de aceite, arquivos/trechos relevantes, estado das mudanças, testes necessários e instruções aplicáveis. Deixar evidência adicional acessível por referência. Não copiar toda a conversa por padrão quando uma tarefa isolada basta.
  - O reasoning model pode definir a solução; o worker executa o bloco até um checkpoint verificável. Devolver patch/resultado, testes, pendências e referências, não uma transcrição completa. Evitar o pai caro reavaliando cada comando simples.
  - Manter a troca de executor com conversa ampla quando necessária ou economicamente melhor. Não forçar isolamento para apagar contexto indispensável nem quebrar retomada, tool-call/result pairs, blocks de reasoning ou política de modelo/credencial.
  - **Aceite/evidência:** tarefa localizada concluída com menos contexto/handoffs e custo total menor; contexto faltante leva a recuperação/escalada controlada; instruções de arquivos e testes continuam atendidas. Verificar também worker inadequado e retomada de subagente.

### P2 — Evitar transportar, resumir e repetir trabalho desnecessário

- [ ] **T12 — Fazer ferramentas devolverem o menor resultado suficiente.**
  - **Dependências:** T01, T02; T04/T08 para a parte generativa. **Arquivos:** [D8], [D9], [D10]; referências [P2], [P3].
  - Preferir busca com trechos, paginação e leitura de intervalo; extrair erros de builds/testes com parsers e redutores existentes. Preservar exit code, comandos, paths e informação necessária ao diagnóstico. Edições devolvem confirmação curta e evidência acessível; não reenviar o arquivo inteiro.
  - Separar conteúdo da UI do conteúdo do modelo; permitir lotes de operações independentes e pequenas alterações quando o contrato atual suportar. A saída original permanece recuperável por handle/arquivo e com intervalo claro.
  - Ampliar compressão fora de 24–32 KiB somente com evidência econômica: primeiro reduzir/segmentar deterministicamente, depois resumir a parte útil. Não resumir toda saída nem aplicar um teto cego que esconda o erro.
  - **Aceite/evidência:** custo de leitura futuro cai, reconstrução da evidência funciona, casos de saída exata/documentos de instrução permanecem exatos e o ganho não é consumido por reabrir o original em quase toda tarefa. Em cada producer→model seam aplicável, provar utility primeiro → worker configurado limitado quando utility falha/não é elegível → evidência necessária fiel/original recuperável quando ambos falham, preservando exit status, erros, failed/skipped/not-run, contagens, paths/linhas, comando e handle; sem declarar sucesso por resumo. Fallbacks e reaberturas são contabilizados uma vez.

- [ ] **T13 — Recuperar contexto e memória por relevância, com reuso válido.**
  - **Dependências:** T01, T02, T07. **Arquivos:** [D9], [D19], busca/índices já existentes.
  - Usar busca lexical/símbolos/grafo existente para candidatos; Jev reranqueia somente quando isso ajuda. Manter contexto do objetivo, decisões confirmadas, arquivos modificados e evidência recente. Não introduzir vector database ou indexador novo sem lacuna comprovada.
  - Aproveitar o reuso de leituras exatas por hash. Antes de substituir conteúdo por referência, garantir que o modelo ainda tem esse conteúdo ou consegue recuperá-lo; arquivo persistido no disco não significa conteúdo ainda conhecido após compactação. Invalidar por edição/versão e isolamento de sessão.
  - Recuperar a memória relevante para a tarefa, sem adicionar todo o histórico e sem promovê-la a instrução acima da mensagem do usuário.
  - **Aceite/evidência:** menos leitura/contexto total, mesma recuperação dos fatos necessários, teste de arquivo alterado e de compactação sem reutilizar referência “já lida” cujo conteúdo foi esquecido.

- [ ] **T14 — Compactar por necessidade e custo, usando utility quando suficiente.**
  - **Dependências:** T01, T02, T04, T07, T08. **Arquivos:** [D11], [D12], [P6].
  - Preservar objetivo, restrições, decisões, estado de arquivos, testes executados, evidências, pendências e referências recuperáveis. Manter prefixos/instruções obrigatórios e pares de protocolo válidos. Usar seleção determinística/Jev sobre descritores limitados antes de gerar resumo.
  - Comparar o summarizer atual com utility sobre segmentos limitados e com redução sem LLM. Ajustar o gatilho dentro da margem segura de contexto conforme economia futura esperada. Um resumo cedo demais custa uma geração e pode destruir cache sem benefício.
  - Medir prefire aplicado, invalidado e nunca utilizado; não confundir latência escondida com custo economizado. Validar resumo contra fontes e reter/recuperar o original em falha. Não exigir segunda geração ou rodada forte se a validação objetiva já resolveu.
  - **Aceite/evidência:** tarefas longas retomam corretamente após compactação, inclusive branches/rewind/subagentes; tokens pagos e releituras caem; compactações descartadas continuam no ledger. Em `/goal` repetido, recovery/compactação deve ser limitado, reavaliar input + reserva na rota efetiva e não repetir a mesma requisição impossível; objetivo, pins e fatos necessários permanecem. Não copiar os limiares absolutos do Pi sem calibrar janela e workload.

- [ ] **T15 — Baratear títulos, recaps e resumos auxiliares.**
  - **Dependências:** T01, T02, T03, T08. **Arquivos:** [D12].
  - Identificar quais gerações ocorrem sem consumidor ou com informação inalterada. Reusar resultado válido e gerar sob necessidade real. Para conteúdo de apresentação simples, comparar utility com entrada curta versus modelo pai com prefixo em cache.
  - Não remover cegamente tools/prefixo de `parent_cached_request`: eles podem ser responsáveis pelo cache. A opção vencedora depende do custo efetivamente cobrado e da fidelidade necessária. Conteúdo descartado por geração obsoleta também custou dinheiro.
  - **Aceite/evidência:** menos chamadas auxiliares ou menor gasto líquido; títulos/recaps corretos, sem estado antigo sobrescrevendo o atual; custo auxiliar aparece separado e no total.

- [ ] **T16 — Parar retries e otimizações que não se pagam.**
  - **Dependências:** T01, T03, T09. **Arquivos:** [D7], [D14], políticas existentes de retry/cancelamento.
  - Conciliar limites de retries no sampler, transporte e fallback para evitar multiplicação silenciosa. Distinguir rate limit/erro transitório, credencial, contexto excedido, output truncado, resultado inválido e falta de capacidade; cada causa exige reação diferente.
  - Contadores utility hoje não bloqueiam a lane: propor supressão temporária de uma otimização opcional comprovadamente improdutiva, com chave por endpoint/tarefa e recuperação posterior. Começar pela evidência determinística; Jev só classifica erros não estruturados quando acrescenta valor. Respeitar políticas e budgets explicitamente definidos pelo usuário.
  - Calcular se nova tentativa barata compensa mais que escalada direta. Não repetir automaticamente ferramenta com efeito externo cujo resultado ficou incerto. Não transformar economia em abandono silencioso da tarefa.
  - **Aceite/evidência:** endpoint indisponível e utility sempre rejeitada não adicionam chamadas a cada rodada; caminhos recuperáveis continuam funcionando; tentativas faturadas e canceladas aparecem no custo. Para overflow repetido de `/goal`, não reenviar a mesma combinação input+reserva após falha: reavaliar/compactar/rotear uma vez dentro dos limites e então preservar falha e evidência recuperável, sem abandono silencioso. Validar com testes de backoff/falhas existentes e o menor caso faltante.

- [ ] **T17 — Verificar e encerrar com o menor gasto suficiente.**
  - **Dependências:** T02, T04, T09. **Arquivos:** catálogo verify/routing em [D18], execução de ferramentas e testes existentes.
  - Sucesso de comando estruturado, schema, patch aplicado e testes devem ser consumidos deterministicamente. Jev pode priorizar teste/revisão ou apontar falta de evidência; não converter sua probabilidade em aprovação do requisito.
  - Encerrar sem uma chamada adicional somente quando a ação solicitada tiver um resultado terminal suficiente e o usuário receber a resposta necessária. Em mudanças semânticas, revisão forte é condicional ao risco e evidência; não obrigatória após toda tarefa barata, nem eliminada para economizar.
  - Nunca usar `test_verdict` generativo para substituir exit code ou pular testes exigidos. Estagnação pode motivar escalada; não é prova de conclusão.
  - **Aceite/evidência:** comandos simples economizam uma rodada quando aplicável, mudanças continuam verificadas e o sistema não declara tarefa concluída com falha/pendência. Custo de verificação entra na comparação.

- [ ] **T18 — Reduzir rodadas e desperdício com lotes e concorrência delimitada.**
  - **Dependências:** T01, T08, T11, T16. **Arquivos:** [D7], [D13], execução de ferramentas existente.
  - Agrupar buscas/leituras/execuções independentes quando isso reduz viagens ao LLM. Não paralelizar operações com dependência nem lançar vários workers para a mesma tarefa por padrão.
  - Medir a fila utility global: preservar serialização para um recurso local único; considerar limite por endpoint somente se bloqueio entre recursos independentes gerar tempo perdido ou retries relevantes. Antes de novo request, cancelar trabalho pendente obsoleto quando possível.
  - **Aceite/evidência:** menor número de rodadas ou menor gasto por retrabalho, sem conflitos e com todos os custos contados. Melhorar apenas latência não deve ser reportado como economia monetária.

- [ ] **T19 — Aplicar orçamento de contexto a imagens, PDF e snapshots.**
  - **Dependências:** T01, T02, T03. **Arquivos:** [D15], ferramentas de navegador/computador existentes.
  - Reusar redimensionamento e limites já implementados. Preferir texto/PDF extraído, árvore de acessibilidade ou trecho quando preservarem o requisito; enviar páginas/imagens necessárias e não repetir material visual sem mudança útil. Resolução deve continuar suficiente para texto pequeno, layout ou comparação visual solicitada.
  - Jev pode avaliar metadados/texto para seleção; não inspeciona pixels. Utility visual só entra entre candidatos se suportado e validado. Comparar custo por unidade visual do provedor, não somente comprimento de texto.
  - **Aceite/evidência:** tarefa visual mantém fidelidade e sucesso com menor cobrança; uma mudança relevante da tela/página força atualização. Sem benefício no workload, manter o comportamento existente.

### P3 — Calibrar, comparar e promover somente o que funciona

- [ ] **T20 — Calibrar decisões com resultados reais, sem criar um sistema de ML desnecessário.**
  - **Dependências:** T01, T02 e a ação sob avaliação. Calibrar roteamento/effort após T09/T10; os ganhos de prompt e contexto podem ser avaliados e promovidos antes disso.
  - Começar com agregados de logs: tipo de tarefa, modelo/endpoint/versão, effort, tamanho de contexto, cache, custo, aceite, rejeição, escalada e recuperação. Separar custo/sucesso de classe prevista pelo Jev e resultado medido; incluir tarefas em português e inglês.
  - Ajustar tabelas de rota e limiares por pergunta com amostra de calibração. Preservar conjunto final sem ajuste. Mudanças de versão/preço/endpoint invalidam conclusões pertinentes; não treinar roteador novo ou bandit online antes de demonstrar necessidade.
  - Considerar viés de seleção: não concluir que modelo só recebeu tarefas fáceis e por isso serve para todas. Usar execuções pareadas/controladas para comparar candidatos; não experimentar rotas novas ocultamente em tarefas reais para coletar dados.
  - **Aceite/evidência:** curvas custo×qualidade por classe e capacidade de explicar uma escalada errada; confiança do Jev não é tratada como taxa de sucesso; nenhuma alegação baseada apenas em taxa de offload.

- [ ] **T21 — Validar cada ganho, promover gradualmente e atualizar o inventário.**
  - **Dependências:** T02, T20 e os itens selecionados para cada lote. Não é obrigatório implementar todo P2 para promover um ganho já demonstrado.
  - Fazer ablações por grupo: prompt/descoberta; roteamento/effort; utility; contexto/compactação; auxiliares/retries. Comparar custos completos com qualidade, incluindo cache frio e quente. Usar flags existentes para voltar ao comportamento anterior; não criar um segundo modo/harness paralelo.
  - Atualizar `list.md`, documentos e defaults somente após provar caminho ativo e resultado. Distinguir “implementado”, “atingível”, “habilitado”, “medido” e “economia aprovada”. Remover apenas experimentos/complexidade introduzidos nesta execução que não demonstrem benefício.
  - **Aceite/evidência:** relatório reproduzível mostra custo por tarefa aceita inferior ao baseline com qualidade preservada; alegação contra Pi usa comparação correspondente. Se houver publicação futura, verificar também o fluxo afetado após deploy; CI/build e benchmark local são evidências diferentes.

## 7. Protocolo mínimo de avaliação

**Amostra inicial:** tarefas reais que cubram pergunta simples, pesquisa em repo, patch mecânico, bug localizado, diagnóstico difícil, mudança com vários arquivos, logs grandes, descoberta de skills/MCP, sessão longa/compactação e tarefa visual quando relevante. Começar com uma amostra exploratória administrável, por exemplo 20–30 tarefas com repetições, e dimensionar o conjunto final pela variabilidade observada e margem de qualidade — não tratar esse número como prova estatística suficiente.

**Dois comparativos diferentes:**

1. **Efeito do harness:** Pi e Distill com mesmo modelo, effort efetivamente equivalente, tarefa, ferramentas necessárias, ambiente e orçamento. Mede a sobrecarga e as estratégias de contexto; não usar um Pi sem capacidade requerida como concorrente artificialmente barato.
2. **Melhor política econômica:** Pi de referência configurado de forma competente versus Distill com Jev/utility/worker. Permitir composição de modelos como parte da proposta, mas contar todos os gastos e publicar a diferença de configuração. Uma terceira variante Pi com extensão equivalente só é necessária se a alegação for superioridade sobre essa composição específica.

**Controles:** congelar revisões, provider/model IDs respondentes, prompts, índices, effort, preços, critérios, ambiente, budgets e ordem de execuções. Alternar ordem e separar sessões para evitar contaminação; avaliar sequências com prefixo quente e primeira execução fria. Fixar acesso a rede/dados e preservar snapshots de entradas variáveis quando possível.

**Aceite:** testes e verificação de comportamento definidos antes da execução; quando necessário, avaliação humana independente ou avaliador separado e com seu custo identificado. Não permitir que o mesmo roteador que escolhe o worker seja o único juiz de que a tarefa ficou correta. Preservar validações requeridas pelo usuário/projeto.

**Relatório por variante:** número planejado/executado/aceito/falhado/inconclusivo, custo total e por tarefa aceita, custos por papel, input sem cache/read/write, output e reasoning sem dupla contagem, chamadas, tokens por seção estimados, escaladas, releituras, rejeições, retries, compactações aplicadas/descartadas, latência p50/p95 e completude da cobrança. Guardar requests/artefatos somente conforme a política existente, sem credenciais.

Dados faltantes não são zero. Falha do agente é resultado; falha de infraestrutura e custo ausente precisam ficar explícitos e não podem desaparecer da amostra para melhorar o headline. Repetições e intervalos de incerteza devem impedir conclusão de equivalência baseada apenas em “não vimos diferença”. Se qualidade piorar, corrigir a causa ou rejeitar a otimização, mesmo que a conta nominal diminua.

## 8. Ordem prática e critérios para não aumentar a complexidade

1. **Primeiro lote:** T01, T04 e T03; então T02. Descobrir onde o dinheiro vai e impedir aceitação indevida antes de ampliar utility tasks.
2. **Segundo lote:** T05–T08. Remover contexto/decisões redundantes e ligar poucas tarefas fechadas com evidência de retorno.
3. **Terceiro lote:** T09–T11. Escolher modelo + effort pela economia observada e delegar blocos completos com contexto pequeno.
4. **Lotes seguintes:** escolher entre T12–T19 pelo gasto observado, não pelo número de features. Calibrar em T20 e promover cada lote com T21.

Não construir outro orchestrator, catálogo, vector database, plataforma de benchmark ou sistema de aprendizagem por antecipação. Reusar os módulos citados. Não habilitar dezenas de flags para “ter mais Jev”, nem adicionar Jev a cada tool call: uma otimização deve reduzir chamadas/tokens pagos ou retrabalho em quantidade que cubra seu próprio custo.

Não podar instruções obrigatórias, autorizações ou evidência necessária. Jev escolhe relevância/adequação dentro do espaço permitido; a política de permissões continua no harness. Não considerar modelos gratuitos/locais automaticamente vencedores, cache sempre vantajoso, esforço mínimo sempre barato ou menos tokens sempre menor conta.

**Verificação durante a implementação:** rodar primeiro os testes existentes dos módulos alterados; acrescentar apenas a regressão pequena que falta. Para este estudo não foram executados benchmarks pagos, testes de produto ou deploy. A entrega deve ser validada por integridade das referências, consistência das dependências e revisão do diff deste arquivo.

## 9. Índice de evidências

### Distill — implementação local examinada

- **D1:** [renderização do prompt](crates/codegen/distill-agent/src/prompt/context.rs), [template padrão](crates/codegen/distill-agent/templates/prompt.md), [template apply_patch](crates/codegen/distill-agent/templates/apply_patch_prompt.md).
- **D2:** [preparação de schemas e sampling](crates/codegen/distill-shell/src/session/acp_session_impl/sampler_turn.rs), [filtro Jev](crates/codegen/distill-shell/src/session/acp_session_impl/jev_tool_subset.rs), [snapshot do contexto](crates/codegen/distill-shell/src/session/acp_session_impl/context_snapshot.rs).
- **D3:** [registro de ferramentas](crates/codegen/distill-tools/src/registry/types.rs), [bridge](crates/codegen/distill-tools/src/bridge.rs), [search_tool](crates/codegen/distill-tools/src/implementations/search_tool/mod.rs).
- **D4:** [escolha de executor, effort e sugestão de skills](crates/codegen/distill-shell/src/session/acp_session_impl/jev_routing.rs), [setup e anúncios](crates/codegen/distill-shell/src/session/acp_session_impl/session_setup.rs).
- **D5:** [UsageLedger](crates/codegen/distill-chat-state/src/usage.rs), [arquivo de usage](crates/codegen/distill-shell/src/session/usage_file.rs), [ledger Jev](crates/codegen/distill-shell/src/session/acp_session_impl/jev_ledger.rs), [testes de folding](crates/codegen/distill-shell/src/session/acp_session_tests/subagent_usage_fold_tests.rs).
- **D6:** [contratos, guards e execução de tarefas utility](crates/codegen/distill-workspace/src/jev/tasks.rs).
- **D7:** [cliente utility e usage anterior à validação](crates/codegen/distill-workspace/src/jev/cheap.rs), [política, fila e contadores](crates/codegen/distill-shell/src/jev_cheap.rs).
- **D8:** [tratamento de tool results](crates/codegen/distill-shell/src/session/acp_session_impl/jev_tool_result.rs).
- **D9:** [redução e reuso](crates/codegen/distill-shell/src/jev_lanes.rs), [redutores](crates/codegen/distill-workspace/src/jev/reduce.rs), [crushers](crates/codegen/distill-workspace/src/jev/crushers.rs).
- **D10:** [conteúdo dos resultados](crates/codegen/distill-tools/src/types/output.rs), [leitura de arquivos](crates/codegen/distill-tools/src/implementations/read_file/mod.rs).
- **D11:** [compactação da sessão](crates/codegen/distill-shell/src/session/compaction.rs), [prefire/configuração](crates/codegen/distill-shell/src/session/compaction_config.rs), [seleção Jev](crates/codegen/distill-shell/src/session/acp_session_impl/jev_compaction.rs), [transcript reduzido](crates/codegen/distill-compaction-transcript/src/lib.rs), [compactadores existentes](crates/common/distill-compaction/src).
- **D12:** [side calls e prefixo de cache](crates/codegen/distill-shell/src/session/acp_session_impl/side_call.rs), [recap](crates/codegen/distill-shell/src/session/acp_session_impl/recap.rs), [títulos](crates/codegen/distill-shell/src/session/acp_session_impl/title_refresh.rs), [resumo de turno](crates/codegen/distill-shell/src/session/acp_session_impl/turn_summary.rs).
- **D13:** [subagentes](crates/codegen/distill-shell/src/agent/subagent), [limite de contexto na retomada](crates/codegen/distill-shell/src/agent/subagent/resume_window.rs).
- **D14:** [roteamento final e recuperação no sampler](crates/codegen/distill-shell/src/session/acp_session_impl/sampler_turn.rs).
- **D15:** [processamento de imagens](crates/codegen/distill-image/src/lib.rs), [PDF/imagens na leitura](crates/codegen/distill-tools/src/implementations/read_file/mod.rs).
- **D16:** [semântica normalizada de TokenUsage](crates/codegen/distill-sampling-types/src/conversation.rs), [formatos de reasoning e provedores Jev/utility](crates/codegen/distill-workspace/src/jev/provider.rs).
- **D17:** [fatos atuais dos modelos, arquivo local em andamento](crates/codegen/distill-shell/src/jev_model_facts.rs), [metadados OpenRouter, arquivo local em andamento](crates/codegen/distill-shell/src/openrouter_models.rs).
- **D18:** [cliente Jev](crates/codegen/distill-workspace/src/jev/client.rs), [integração/batching](crates/codegen/distill-shell/src/jev.rs), [catálogo](crates/codegen/distill-workspace/src/jev/catalog), [flags e defaults](crates/codegen/distill-workspace/src/jev/flags.rs), [tipos de respostas](crates/codegen/distill-workspace/src/jev/types.rs).
- **D19:** [seleção de memória](crates/codegen/distill-shell/src/session/acp_session_impl/jev_memory.rs), [busca de memória](crates/codegen/distill-tools/src/implementations/memory/search_tool.rs), [grafo de código existente](crates/codegen/distill-codebase-graph).

[D1]: crates/codegen/distill-agent/src/prompt/context.rs
[D2]: crates/codegen/distill-shell/src/session/acp_session_impl/sampler_turn.rs
[D3]: crates/codegen/distill-tools/src/registry/types.rs
[D4]: crates/codegen/distill-shell/src/session/acp_session_impl/jev_routing.rs
[D5]: crates/codegen/distill-chat-state/src/usage.rs
[D6]: crates/codegen/distill-workspace/src/jev/tasks.rs
[D7]: crates/codegen/distill-workspace/src/jev/cheap.rs
[D8]: crates/codegen/distill-shell/src/session/acp_session_impl/jev_tool_result.rs
[D9]: crates/codegen/distill-shell/src/jev_lanes.rs
[D10]: crates/codegen/distill-tools/src/types/output.rs
[D11]: crates/codegen/distill-shell/src/session/compaction.rs
[D12]: crates/codegen/distill-shell/src/session/acp_session_impl/side_call.rs
[D13]: crates/codegen/distill-shell/src/agent/subagent
[D14]: crates/codegen/distill-shell/src/session/acp_session_impl/sampler_turn.rs
[D15]: crates/codegen/distill-image/src/lib.rs
[D16]: crates/codegen/distill-sampling-types/src/conversation.rs
[D17]: crates/codegen/distill-shell/src/jev_model_facts.rs
[D18]: crates/codegen/distill-workspace/src/jev/client.rs
[D19]: crates/codegen/distill-shell/src/session/acp_session_impl/jev_memory.rs

### Pi — fontes fixadas no commit examinado

[P1]: https://github.com/earendil-works/pi/blob/1a584a7a56eb5e7b4ff8ccbd46430f1533282eed/packages/coding-agent/src/core/system-prompt.ts
[P2]: https://github.com/earendil-works/pi/tree/1a584a7a56eb5e7b4ff8ccbd46430f1533282eed/packages/coding-agent/src/core/tools
[P3]: https://github.com/earendil-works/pi/blob/1a584a7a56eb5e7b4ff8ccbd46430f1533282eed/packages/coding-agent/src/core/tools/edit.ts
[P4]: https://github.com/earendil-works/pi/blob/1a584a7a56eb5e7b4ff8ccbd46430f1533282eed/packages/agent/src/agent-loop.ts
[P5]: https://github.com/earendil-works/pi/blob/1a584a7a56eb5e7b4ff8ccbd46430f1533282eed/packages/coding-agent/docs/extensions.md
[P6]: https://github.com/earendil-works/pi/blob/1a584a7a56eb5e7b4ff8ccbd46430f1533282eed/packages/coding-agent/src/core/compaction/compaction.ts
[P7]: https://github.com/earendil-works/pi/blob/1a584a7a56eb5e7b4ff8ccbd46430f1533282eed/packages/ai/src/models.ts
[P8]: https://github.com/earendil-works/pi/blob/1a584a7a56eb5e7b4ff8ccbd46430f1533282eed/packages/coding-agent/src/core/agent-session.ts
[P9]: https://github.com/earendil-works/pi/blob/1a584a7a56eb5e7b4ff8ccbd46430f1533282eed/packages/evals/README.md

- **P1:** [system-prompt.ts][P1], [skills.ts](https://github.com/earendil-works/pi/blob/1a584a7a56eb5e7b4ff8ccbd46430f1533282eed/packages/coding-agent/src/core/skills.ts): núcleo, regras condicionais, catálogo e carregamento de skills.
- **P2/P3:** `read.ts`, `bash.ts`, `truncate.ts`, `write.ts`, `edit.ts`: limites, paginação, armazenamento, confirmação e detalhes de UI.
- **P4/P5:** hooks de contexto/modelo, preparação de requests, ferramentas concorrentes e resultado terminal; distinguir extensão disponível de comportamento padrão.
- **P6:** [compaction.ts][P6], [utils.ts](https://github.com/earendil-works/pi/blob/1a584a7a56eb5e7b4ff8ccbd46430f1533282eed/packages/coding-agent/src/core/compaction/utils.ts): gatilho, trecho recente, resumo, limite de tool results, tracking de arquivos e política de cache da geração de resumo.
- **P7:** [models.ts][P7], [anthropic-messages.ts](https://github.com/earendil-works/pi/blob/1a584a7a56eb5e7b4ff8ccbd46430f1533282eed/packages/ai/src/api/anthropic-messages.ts), [openai-responses.ts](https://github.com/earendil-works/pi/blob/1a584a7a56eb5e7b4ff8ccbd46430f1533282eed/packages/ai/src/api/openai-responses.ts), [simple-options.ts](https://github.com/earendil-works/pi/blob/1a584a7a56eb5e7b4ff8ccbd46430f1533282eed/packages/ai/src/api/simple-options.ts): custos, cache e effort/thinking por provedor.
- **P8/P9:** usage completo de sessão e avaliações pareadas/repetidas com registro de execuções ausentes.

### Jev — documentação oficial consultada

[J1]: https://docs.typesafe.ai/introduction
[J2]: https://docs.typesafe.ai/models
[J3]: https://docs.typesafe.ai/primitives
[J4]: https://docs.typesafe.ai/cookbooks/skill_suggestion
[J5]: https://docs.typesafe.ai/confidence
[J6]: https://docs.typesafe.ai/model-jaggedness/jev-1.13

- [Primitivas e composição][J1], [paralelismo das perguntas][J3], [modelos, preços e limites][J2].
- [Seleção de skills][J4], [semântica de confiança][J5], [limitações publicadas do Jev 1.13][J6].
- [Exemplo de perguntas agrupadas](https://docs.typesafe.ai/cookbooks/parallel_questions) e [cascata de extração/verificação/escalada](https://docs.typesafe.ai/cookbooks/sde_cascade). Os números desses exemplos não são uma previsão de economia do Distill.
