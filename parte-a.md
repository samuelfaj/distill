Parte A — O que o app macOS (remote-code/macos-app) faz com o modelo local para poupar tokens

A.0 O encanamento (onde o local entra)

┌────────────────┬────────────────────────────────────────────────────────────┬───────────────────────────────────────────────────────────┐
│ Peça           │ Onde                                                       │ O que é                                                   │
├────────────────┼────────────────────────────────────────────────────────────┼───────────────────────────────────────────────────────────┤
│ Sidecar de     │ Sources/RemoteCodeDistillService/, DistillCore/            │ serviço local (127.0.0.1) que comprime antes de o         │
│ compressão     │ DistillService.swift                                       │ provider ver o payload                                    │
├────────────────┼────────────────────────────────────────────────────────────┼───────────────────────────────────────────────────────────┤
│ Hook no        │ docs/token-saver-grok-post-tool-use.md +                   │ substitui o resultado da tool via PostToolUse do Grok (   │
│ harness        │ ManagedDistillCLI.hookPostToolUseGrok()                    │ shapes: read_file, list_dir, web_fetch); shell vai por    │
│                │                                                            │ wrapper de PATH/pipe no PreToolUse                        │
├────────────────┼────────────────────────────────────────────────────────────┼───────────────────────────────────────────────────────────┤
│ Modelo local   │ OpenAICompatibleDistillRuntime.swift ("distill generate    │ é literalmente o seu oMLX como motor de compressão        │
│                │ via OpenAI-compatible HTTP endpoint (oMLX, llama.cpp       │                                                           │
│                │ server)")                                                  │                                                           │
├────────────────┼────────────────────────────────────────────────────────────┼───────────────────────────────────────────────────────────┤
│ Modelos        │ CoreModel/TokenSaverApprovedModel.swift                    │ samuelfaj/distill-1.7B-4bit-MLX e samuelfaj/distill-E4B-  │
│ aprovados      │                                                            │ it-4-bit-MLX                                              │
├────────────────┼────────────────────────────────────────────────────────────┼───────────────────────────────────────────────────────────┤
│ Encoder local  │ MLXBertClassifierBackend.swift ("Real BERT sequence/token  │ BERT/embedding/NLI no Mac, sem nuvem                      │
│                │ classification on MLX (I8.2 NLI, I8.4 NER)")               │                                                           │
├────────────────┼────────────────────────────────────────────────────────────┼───────────────────────────────────────────────────────────┤
│ Serialização   │ MLXGenerationLane.swift                                    │ uma geração por vez no Metal                              │
│ de GPU         │                                                            │                                                           │
└────────────────┴────────────────────────────────────────────────────────────┴───────────────────────────────────────────────────────────┘

A.1 As ações do modelo local (por família, com locator)

1) Compressão generativa local — 6 modos (DistillCore/DistillPrompt.swift, DistillMode)
• commandOutput — comprime saída de comando preservando comandos, paths, file:line, erros, números, IDs e status;
• longText — comprime texto longo ("target at most one third of the source length");
• watchSummary — resumo de observação/watch;
• wireEncode — reescreve a tarefa humana em um brief semântico compacto em inglês para o agente;
• wireRenderEnglish — renderiza frames internos em prosa fluente (economia de protocolo);
• naturalLanguage — normalização em linguagem natural.

2) Encoder-first extrativo (EncoderFirstRuntime.swift)
• o BERT local escolhe as linhas mais relevantes para a pergunta ANTES de tocar o LLM ("selects the most query-relevant lines BEFORE the wrapped runtime's expensive path"), e funciona mesmo quando o LLM local não carregou.

3) Classificação (DistillContentRouter.swift, EmbeddingPrototypeClassifier.swift, MLXBertClassifierBackend.swift)
• classifica o payload em 11 tipos (json, diff, log, search_results, build_output, stack_trace, test_report, source_code, html, mixed, prose) com confiança, para escolher o compressor;
• zero-shot por protótipos de embedding; NER por BERT.

4) Roteamento lossy recuperável (CommandOutputLossyRouter.swift, ToolOutputCompressionPolicy.swift, TokenSaverRoutingPolicy.swift)
• decide se uma saída pode ser comprimida com perda (store-before-loss obrigatório) e por qual estágio; follow-up nunca vai ao LLM local.

5) Crushers tipados determinísticos (CoreModel/TokenSaverTypedCrushers.swift, CrusherKind)
• pytest, stacktrace, diff, search, adaptive_json — mesma família de economia, sem LLM (default OFF com release seal).

6) Score de importância por linha (DistillImportanceScorer.swift)
• mantém verbatim erros/warnings, file:line, paths citados e as últimas N linhas; elide o miolo com … N lines elided …, recuperável via CCR.

7) Intenção da saída de comando (DistillCore.swift:54, CommandOutputIntent)
• 7 intenções: cwdAndFiles, cwdOnly, tokenVerdict, pathsOnly, existsMissing, statusCode, jsonOnly — o seam existe e o runtime passa nil (hoje cai em heurística de palavras).

8) Recuperação/retrieval (CCRSearchIndex.swift, ManagedRetrieveMCP.swift, EmbeddingCache.swift, EmbeddingNearDuplicateNominator.swift, QueryAwareChunkRanker.swift)
• índice de busca sobre os originais guardados; MCP local para o agente buscar os bytes exatos em vez de recebê-los no prompt; cache de embedding por hash; dedup por quase-duplicata.

9) Dedup e reuso de leitura (CrossTurnDedupPolicy.swift, ReadLifecycleMasker.swift)
• ledger por leitura (realpath, faixa de linhas, hash, bytes, handle) → leitura repetida é servida por handle, leitura obsoleta é mascarada.

10) Estabilidade de cache do provider (CacheAlignerVolatileTokenDetector.swift)
• conta tokens voláteis (UUID, ISO-8601, JWT, hex) que quebram o prompt cache do provider — cada quebra custa prefill cheio.

11) Gates de fidelidade/qualidade (antes de aceitar o que o local produziu) FaithfulnessGate, NLIFaithfulnessGate (NLI, SummaC), SemanticSimilarityGate (cosseno, "necessary, NOT sufficient"), NetPositiveGate (comprime ou não o argv do turno), LiteralPreservation ("correctness gate for any LOSSY compression"), TagAndMarkerProtector (protege tags/marcadores com placeholders e restaura), SeededCorruptionCanary (corrompe de propósito para testar o gate), ShadowEvaluator (harness sombra + dano-proxy), MaskingABHarness (A/B), TokenSaverBestOfThreeSelector (escolhe entre 3 envelopes), LatencyPercentileTracker (p50/p95).

12) Quando trabalhar (custo zero de LLM) — DistillIdlePolicy, DistillSkipAndYield (pula o que sabidamente não comprime), DeadlineDistillRuntime (deadline por chamada, fail-open), TokenSaverCircuitBreaker, MLXGenerationLane.

13) Antecipação por turno — TokenSaverTurnBatchRuntime / TurnBatchPrefetch / QueueCoalesce / DiscoveryPrefetchRuntime (Wave C: batch do turno + prefetch de descoberta, default OFF).

14) Mídia — Librarian/LibrarianMediaOps.swift (Vision/Speech/CoreGraphics: OCR, fala, imagem no Mac) + LibrarianGenerateRuntime/Handlers/Catalog.

15) Autoridade/segurança — TokenSaverAuthorityPlane (release seal por binding), ProviderSafetyABI, ShellDistillPipeAdmission, ProviderProxyPolicy, store-before-loss + read-back, ledger content-free, fail-open em todo caminho.

A.2 Os números medidos (do próprio doc)

• Em 2 dias: 45,6 MB de resultados de tool; read_file 48 %, web_fetch 10 %, grep 10 %, list_dir 2,5 %; só a classe shell era alcançável pelo wrapper.
• CLI: 6.125 B → 298 B (4,9 %, rota determinística); na sessão viva: 1.670 B → 72,7 % de redução.
• Limites do hook: substituição ≤ 64 K chars; shape inválido ⇒ mantém o original (no-op silencioso).
• Exclusões explícitas: Bash (byte-exact), GrepSearch, EditsApplied — e o plan-jev/PLANO.md §3.9 manda não mover compressão/ranking/gates locais para o Jev (regra local-first).

───

Parte B — O que o harness (jev-build) já tem hoje, para calibrar

✅ 26 alavancas fiadas: permissão (H1/H2), A1 arquivo, A2/P2 janela de leitura, A3 linhas, A4 web, A5 memória, A6 teste, B1 intenção, B2 tier (off), B2m effort por micro-ação, B2l modelo local por chamada (32k de teto), B3 subagente, B4/P1 famílias de tools, B5/P6 skill, B6 delegação, C1/C3 parada/conclusão, C2 triagem, C4 revisão do diff + redo com effort maior, C5 ordem de erros, C6 injeção (off), C7 tipo de mudança, D1/P3 recorte da compaction, D2 retenção de saída grande, D3 pós-compaction, D4/P5 validação de chamada. E o subagente local-worker no modelo local (contexto curto: 450 → 10k tokens medidos, contra 29k do prompt-base).

O que não existe no harness (e é onde está o dinheiro): compressão (nada encolhe o que entra no contexto), dedup, retrieval local, uso de modelo local para geração, e o prompt-base de ~29k que vai inteiro em todo turno.

───

Parte C — Todas as oportunidades, por micro-ação

Legenda: ✅ fiado · 🟡 parcial · 🔴 falta. "Quem decide": J = Jev (decisão tipada), L = modelo local (geração/embedding), C = código determinístico.

C.1 Contexto que entra no prompt (o maior filão)

┌────┬─────────────────────────┬─────────────────┬──────────────────────────────────┬──────────┬─────────────────┬────────────────────────┐
│ #  │ Micro-ação              │ Seam no harness │ O que fazer                      │ Quem     │ Ganho           │ Guardas                │
├────┼─────────────────────────┼─────────────────┼──────────────────────────────────┼──────────┼─────────────────┼────────────────────────┤
│ 1  │ Recortar o prompt-base  │ montagem do     │ decisão por bloco: quais seções  │ J        │ o maior de      │ whitelist de blocos    │
│    │ por turno (system       │ prompt da       │ o turno precisa (a técnica skill │          │ todos: cortar   │ obrigatórios (regras   │
│    │ prompt + AGENTS.md +    │ sessão; hoje    │ suggestion/classifying passages  │          │ 10-20k tokens   │ de segurança/permissão │
│    │ skills + docs de tools  │ B5/P6 só sugere │ do app)                          │          │ por chamada     │ nunca saem); piso      │
│    │ ≈ 29k)                  │ skill           │                                  │          │                 │ alto; registrar o que  │
│    │                         │                 │                                  │          │                 │ saiu                   │
├────┼─────────────────────────┼─────────────────┼──────────────────────────────────┼──────────┼─────────────────┼────────────────────────┤
│ 2  │ Classificar o payload   │ jev_post_       │ tipo → crusher/dispatch; decide  │ J        │ evita mandar    │ tipo com confiança ≥   │
│    │ antes de injetar (11    │ process_tool_   │ se vale enviar inteiro           │          │ blob "prose"    │ piso; senão passa      │
│    │ tipos do app)           │ result (hoje só │                                  │          │ /html/json      │                        │
│    │                         │ anota)          │                                  │          │ inteiro         │                        │
├────┼─────────────────────────┼─────────────────┼──────────────────────────────────┼──────────┼─────────────────┼────────────────────────┤
│ 3  │ Compressão generativa   │ jev_post_       │ rota longText/commandOutput no   │ L        │ app mediu 72,7  │ store-before-loss +    │
│    │ local de saída grande   │ process_tool_   │ oMLX, ≤1/3 do tamanho, com       │          │ % numa sessão   │ read-back; gates de    │
│    │                         │ result (D2 só   │ prompt próprio                   │          │ viva            │ fidelidade; fail-open  │
│    │                         │ decide manter/  │                                  │          │                 │                        │
│    │                         │ descartar)      │                                  │          │                 │                        │
├────┼─────────────────────────┼─────────────────┼──────────────────────────────────┼──────────┼─────────────────┼────────────────────────┤
│ 4  │ Seleção extrativa por   │ A3 existe só    │ estender para todo output        │ L (      │ barato e seguro │ piso de relevância;    │
│    │ encoder (BERT-first)    │ para log/teste; │ grande: linhas relevantes à      │ encoder) │ (não gera)      │ âncora nas últimas N   │
│    │                         │ A2 só janela    │ pergunta antes de qualquer LLM   │ + C      │                 │ linhas                 │
├────┼─────────────────────────┼─────────────────┼──────────────────────────────────┼──────────┼─────────────────┼────────────────────────┤
│ 5  │ Score de importância    │ idem            │ adotar a heurística do           │ C        │ alto ganho,     │ elide só o miolo;      │
│    │ por linha (             │                 │ DistillImportanceScorer (erros,  │          │ custo zero, sem │ marcador … N lines     │
│    │ determinístico)         │                 │ file:line, paths, últimas N)     │          │ LLM             │ elided …               │
├────┼─────────────────────────┼─────────────────┼──────────────────────────────────┼──────────┼─────────────────┼────────────────────────┤
│ 6  │ Dedup cross-turno /     │ não existe: o   │ ledger de leituras (path+range+  │ J + C    │ duplicatas      │ hash igual ⇒ oferece   │
│    │ reuso de leitura        │ mesmo arquivo   │ hash) e "isto já está no         │          │ inteiras        │ handle/referência      │
│    │                         │ lido 2× vai 2×  │ contexto?"                       │          │                 │                        │
├────┼─────────────────────────┼─────────────────┼──────────────────────────────────┼──────────┼─────────────────┼────────────────────────┤
│ 7  │ Detector de token       │ não existe      │ contar UUID/timestamp/JWT/hex no │ C        │ cada quebra de  │ só diagnóstico/ação de │
│    │ volátil (cache do       │                 │ payload e alinhar o prefixo      │          │ cache = prefill │ ordenação              │
│    │ provider)               │                 │ estável                          │          │ cheio; é        │                        │
│    │                         │                 │                                  │          │ economia        │                        │
│    │                         │                 │                                  │          │ silenciosa      │                        │
│    │                         │                 │                                  │          │ grande          │                        │
├────┼─────────────────────────┼─────────────────┼──────────────────────────────────┼──────────┼─────────────────┼────────────────────────┤
│ 8  │ Recuperação sob demanda │ a sessão re-lê  │ handle + ferramenta de expandir  │ C        │ corta entrada   │ read-back byte-exact   │
│    │ (MCP local)             │ arquivos        │ o original em vez de mandar tudo │          │ com saída sob   │                        │
│    │                         │                 │                                  │          │ demanda         │                        │
├────┼─────────────────────────┼─────────────────┼──────────────────────────────────┼──────────┼─────────────────┼────────────────────────┤
│ 9  │ Passages/guardrail de   │ C6 está off     │ ligar C6 com a precedência do    │ J        │ segurança +     │ C6 hoje off por custo; │
│    │ conteúdo não confiável  │                 │ app (support > block > review >  │          │ corte de        │ medir por saída        │
│    │                         │                 │ pass) e aplicá-lo a web/memória  │          │ contexto        │                        │
│    │                         │                 │ /MCP                             │          │                 │                        │
├────┼─────────────────────────┼─────────────────┼──────────────────────────────────┼──────────┼─────────────────┼────────────────────────┤
│ 10 │ Slim de schema de tools │ B4/P1 poda      │ segundo nível: podar parâmetros  │ J        │ 2-5k por        │ nunca remover          │
│    │ por turno               │ famílias        │ raros por tool                   │          │ chamada         │ parâmetro obrigatório  │
└────┴─────────────────────────┴─────────────────┴──────────────────────────────────┴──────────┴─────────────────┴────────────────────────┘

C.2 Trabalho que hoje gasta o modelo caro e poderia rodar local

┌────┬─────────────────────┬────────────────────┬─────────────────────────────────┬──────┬───────────────────────┬────────────────────────┐
│ #  │ Micro-ação          │ Seam               │ O que fazer                     │ Quem │ Ganho                 │ Guardas                │
├────┼─────────────────────┼────────────────────┼─────────────────────────────────┼──────┼───────────────────────┼────────────────────────┤
│ 11 │ Resumo da           │ session/compaction │ modo longText local com o mesmo │ L    │ 1 chamada grande por  │ verificar com gates;   │
│    │ compactação         │ .rs (hoje o modelo │ contrato (prefixo/último        │      │ compactação sai do    │ cair para o frontier   │
│    │                     │ da sessão resume)  │ segmento fixos, D1)             │      │ frontier              │ se o local falhar      │
├────┼─────────────────────┼────────────────────┼─────────────────────────────────┼──────┼───────────────────────┼────────────────────────┤
│ 12 │ Título/resumo de    │ hoje frontier      │ longText/watchSummary local     │ L    │ pequeno porém         │ nada de segurança      │
│    │ sessão, changelog,  │                    │                                 │      │ constante             │ envolvido              │
│    │ mensagem de commit  │                    │                                 │      │                       │                        │
├────┼─────────────────────┼────────────────────┼─────────────────────────────────┼──────┼───────────────────────┼────────────────────────┤
│ 13 │ Imagens/screenshots │ hoje o frontier    │ OCR/descrição locais (Vision/   │ L    │ imagens são           │ perda de detalhe       │
│    │ /anexos             │ recebe a imagem    │ Speech do app) e mandar texto   │      │ caríssimas no         │ visual: usar só quando │
│    │                     │                    │                                 │      │ frontier              │ a pergunta é textual   │
├────┼─────────────────────┼────────────────────┼─────────────────────────────────┼──────┼───────────────────────┼────────────────────────┤
│ 14 │ Extração de dados   │ C2/C5/C7 já        │ mover a extração inteira (      │ L +  │ evita turno do        │ determinístico         │
│    │ estruturados de     │ rotulam            │ paths, PASS/FAIL, JSON, status) │ J    │ frontier só para "    │ primeiro               │
│    │ saída               │                    │ para local + Jev decide a       │      │ leia isso e me diga o │                        │
│    │                     │                    │ intenção (7 intenções do app)   │      │ status"               │                        │
├────┼─────────────────────┼────────────────────┼─────────────────────────────────┼──────┼───────────────────────┼────────────────────────┤
│ 15 │ Pré-computar o que  │ não existe         │ Wave C do app: batch do turno + │ C    │ antecipa cache; reduz │ só leitura, nunca      │
│    │ o próximo turno vai │                    │ prefetch de descoberta          │      │ latência e rounds     │ escrita                │
│    │ pedir (prefetch)    │                    │                                 │      │                       │                        │
└────┴─────────────────────┴────────────────────┴─────────────────────────────────┴──────┴───────────────────────┴────────────────────────┘

C.3 Decisões que ainda são do LLM caro e cabem no Jev

┌────┬─────────────────────────────────────┬───────────────────────────┬──────────────────────────────────────┬───────────────────────────┐
│ #  │ Micro-ação                          │ Seam                      │ O que fazer                          │ Ganho                     │
├────┼─────────────────────────────────────┼───────────────────────────┼──────────────────────────────────────┼───────────────────────────┤
│ 16 │ "Isso que eu li responde à          │ 🔴 não existe (A5/A4      │ 4 nouls (≥2/3 excluídos, padrão do   │ corta contexto e evita    │
│    │ pergunta?" por trecho               │ rankeiam, não julgam      │ app)                                 │ rounds de re-leitura      │
│    │                                     │ suficiência)              │                                      │                           │
├────┼─────────────────────────────────────┼───────────────────────────┼──────────────────────────────────────┼───────────────────────────┤
│ 17 │ "Preciso ler mais um arquivo ou já  │ 🔴                        │ noul por candidato antes de decidir  │ cada rodada de leitura    │
│    │ sei o suficiente?"                  │                           │ ler                                  │ evitada é um round +      │
│    │                                     │                           │                                      │ prefill                   │
├────┼─────────────────────────────────────┼───────────────────────────┼──────────────────────────────────────┼───────────────────────────┤
│ 18 │ "Esta saída é confiável/usável?" (  │ 🟡 C2/C4 cobrem falha e   │ noul "isto parece resultado real,    │ evita o frontier          │
│    │ ShadowEvaluator do app)             │ diff                      │ não placeholder/CoT?"                │ raciocinar sobre lixo     │
├────┼─────────────────────────────────────┼───────────────────────────┼──────────────────────────────────────┼───────────────────────────┤
│ 19 │ Plan mode / próximos passos         │ 🔴 (o plano é do LLM)     │ Jev escolhe a classe do próximo      │ reduz turnos              │
│    │                                     │                           │ passo (código/teste/docs/investigar) │ exploratórios             │
│    │                                     │                           │ e o LLM executa                      │                           │
├────┼─────────────────────────────────────┼───────────────────────────┼──────────────────────────────────────┼───────────────────────────┤
│ 20 │ Prioridade de contexto sob pressão  │ 🟡 D1/D2                  │ choice por bloco: "o que sai         │ evita compactação forçada │
│    │ (o que soltar quando o contexto     │                           │ primeiro" (com o app: .losslessOnly  │ (=retrabalho)             │
│    │ aperta)                             │                           │ antes de lossy)                      │                           │
├────┼─────────────────────────────────────┼───────────────────────────┼──────────────────────────────────────┼───────────────────────────┤
│ 21 │ Escolher entre 3 saídas locais (    │ 🔴                        │ gerar 3 localmente e o Jev escolher  │ qualidade sem chamada     │
│    │ BestOfThree do app)                 │                           │ a fiel                               │ frontier                  │
├────┼─────────────────────────────────────┼───────────────────────────┼──────────────────────────────────────┼───────────────────────────┤
│ 22 │ "O turno terminou?" / "faltou       │ ✅ C1/C3                  │ já fiado — candidato a rodar com     │ —                         │
│    │ algo?"                              │                           │ encoder local em vez de Jev (mais    │                           │
│    │                                     │                           │ barato ainda)                        │                           │
└────┴─────────────────────────────────────┴───────────────────────────┴──────────────────────────────────────┴───────────────────────────┘

C.4 Coordenação local (custo/precisão do próprio caminho local)

┌────┬─────────────────────────────────────────────────────────┬──────────────────────────────────────────────────────────────────────────┐
│ #  │ Oportunidade                                            │ Por quê (do app)                                                         │
├────┼─────────────────────────────────────────────────────────┼──────────────────────────────────────────────────────────────────────────┤
│ 23 │ Coalescer as decisões do turno em 1 request             │ invariante do app: "1 request por ponto de decisão, nunca 1 chamada por  │
│    │                                                         │ pergunta"; hoje o harness faz de 5 a 20 requests Jev por turno           │
├────┼─────────────────────────────────────────────────────────┼──────────────────────────────────────────────────────────────────────────┤
│ 24 │ Idle/pressão/thermal (DistillIdlePolicy,                │ o harness chama Jev sempre; o app pula quando não vale e trabalha em     │
│    │ DistillSkipAndYield)                                    │ janelas ociosas                                                          │
├────┼─────────────────────────────────────────────────────────┼──────────────────────────────────────────────────────────────────────────┤
│ 25 │ Deadline por chamada + circuit breaker (                │ o harness tem orçamento de 4 s e fail-defer, mas não tem breaker por     │
│    │ DeadlineDistillRuntime, TokenSaverCircuitBreaker)       │ lane após falhas repetidas                                               │
├────┼─────────────────────────────────────────────────────────┼──────────────────────────────────────────────────────────────────────────┤
│ 26 │ Skip-set do que sabidamente não comprime                │ evita gastar tempo local com payload incomprimível                       │
├────┼─────────────────────────────────────────────────────────┼──────────────────────────────────────────────────────────────────────────┤
│ 27 │ Um lugar só para o caminho local serializar (           │ EVITA pressure no GPU; o harness hoje pode disparar várias chamadas      │
│    │ MLXGenerationLane)                                      │ locais concorrentes (subagente + roteamento)                             │
└────┴─────────────────────────────────────────────────────────┴──────────────────────────────────────────────────────────────────────────┘

C.5 Onde o harness é melhor que o app (e dá para exportar de volta)

• Reversibilidade nativa: o app inventou store-before-loss + handle + read-back; no harness o original já está no transcript/arquivo, e o modelo pode re-ler → a lane lossy é mais simples e mais segura lá.
• Jev como autoridade de decisão em vez de heurística de palavras (o app tem vários seams com nil/substring: CommandOutputIntentClassifier:1856, LibrarianHandlers:514/736).
• Gate de revisão de diff (C4) e redo com effort maior não existem no app — é um mecanismo que o app poderia importar.

───

Parte D — Disciplinas do app a copiar em qualquer lane lossy do harness

1. Store-before-loss + read-back byte-exact antes de usar qualquer saída comprimida.
2. Fail-open em todo caminho (o app: "provider turns fail open to verbatim input").
3. Reader antes do writer; rollback = writer off → drain → verbatim.
4. Release seal por binding (nada de lane ligada por hardcode) — no harness equivale à flag por item que já usamos.
5. Ledger content-free (IDs, contagens, timings; nunca conteúdo/prompt).
6. Sombra antes de autoridade com taxa de concordância revisada.
7. Gates de fidelidade para compressão: preservação literal (paths/file:line/números/erros), NLI/cosseno, canário de corrupção semeada.
8. Orçamento e pressão: deadline por chamada, idle, circuit breaker, uma geração por vez na GPU.

───

Parte E — Prioridade sugerida (maior economia por risco)

1. #1 recorte do prompt-base por turno (J) — vale ~10-20k tokens em toda chamada; risco médio, mitigável por whitelist.
2. #3/#4/#5 compressão+extração local de saída grande (L/C) — ataca o que o app mediu (48 % em read_file), com regra de recuperação nativa do harness.
3. #6/#7 dedup e estabilidade de cache (J/C) — economia silenciosa e grande (duplicatas + prefill).
4. #11 resumo da compactação no local (L) — tira uma chamada grande por compactação do frontier.
5. #23/#25 coalescer decisões e circuit breaker (C) — economiza o próprio Jev (5-20 requests/turno hoje) e evita wedges.
6. #15/#14 prefetch e extração estruturada (L/C); depois #16/#17 decisões novas (J).
7. #9/#13 guardrails e mídia — melhoram segurança e tiram imagem do frontier.

Duas regras que eu manteria do app: local-first para compressão/ranking/gates (não empurrar isso para o Jev, que não gera) e Jev só onde há julgamento semântico fechado. E uma diferença importante de contexto: o app comprime bytes do provider; o harness pode fazer melhor — cortar antes do prompt ser montado, e usar o próprio transcript/arquivo como store de recuperação.