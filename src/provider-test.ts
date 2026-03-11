import type { RuntimeConfig } from "./config";
import { runBitnetTest } from "./bitnet";
import {
  requestDistillDaemon,
  tryRequestDistillDaemon,
} from "./daemon";
import { requestOllamaDetailed } from "./ollama";
import {
  DISTILL_TEST_DISPLAY_PROMPT,
  DISTILL_TEST_PROMPT
} from "./provider-test-fixture";

export interface ProviderTestResult {
  ok: boolean;
  lines: string[];
}

function estimateTokens(input: string): number {
  return Math.max(1, Math.ceil(input.trim().length / 4));
}

function formatSavedPercent(prompt: string, response: string): number {
  const promptTokens = estimateTokens(prompt);
  const responseTokens = estimateTokens(response);
  const saved = ((promptTokens - responseTokens) / promptTokens) * 100;
  return Math.max(0, Math.round(saved));
}

function formatTokensPerSecond(
  tokenPerSecond: number | undefined,
  tokenCount: number | undefined,
  generationMs: number | undefined
): string {
  if (typeof tokenPerSecond === "number" && Number.isFinite(tokenPerSecond)) {
    return tokenPerSecond.toFixed(1);
  }

  if (!tokenCount || !generationMs || generationMs <= 0) {
    return "n/a";
  }

  const tokensPerSecond = tokenCount / (generationMs / 1000);
  return tokensPerSecond.toFixed(1);
}

function buildReport(options: {
  prompt: string;
  displayPrompt?: string;
  response?: string;
  tokenPerSecond?: number;
  tokenCount?: number;
  generationMs?: number;
  lines: string[];
  ok: boolean;
}): ProviderTestResult {
  const report = [
    "Original prompt:",
    options.displayPrompt ?? options.prompt,
    "",
    "Final response:",
    options.response ?? "",
    "",
    `Saved ${formatSavedPercent(options.prompt, options.response ?? "")}% tokens.`,
    "",
    `token/s: ${formatTokensPerSecond(
      options.tokenPerSecond,
      options.tokenCount,
      options.generationMs
    )}`,
    "",
    ...options.lines
  ];

  return {
    ok: options.ok,
    lines: report
  };
}

export async function runProviderTest(
  config: Omit<RuntimeConfig, "question">
): Promise<ProviderTestResult> {
  if (config.provider === "bitnet") {
    try {
      const daemonProbe = await tryRequestDistillDaemon(
        { type: "ping" },
        250,
        process.env
      );

      const canReuseDaemon =
        daemonProbe?.ok &&
        daemonProbe.status === "ready" &&
        daemonProbe.provider === config.provider &&
        daemonProbe.model === config.model;

      if (!canReuseDaemon) {
        return runProviderTestDirect(config);
      }

      const daemonResponse = await requestDistillDaemon(
        {
          type: "summarize",
          config: {
            ...config,
            question: DISTILL_TEST_DISPLAY_PROMPT
          },
          prompt: DISTILL_TEST_PROMPT
        },
        config.timeoutMs
      );

      if (daemonResponse.ok) {
        return buildReport({
          prompt: DISTILL_TEST_PROMPT,
          displayPrompt: DISTILL_TEST_DISPLAY_PROMPT,
          response: daemonResponse.output?.trim(),
          lines: [
            "provider: bitnet",
            `model: ${config.model}`,
            "daemon: ok",
            "generate: ok"
          ],
          ok: true
        });
      }

      return {
        ok: false,
        lines: [
          "daemon: failed",
          `error: ${daemonResponse.ok ? "Missing test result." : daemonResponse.error}`
        ]
      };
    } catch (error) {
      return runProviderTestDirect(config);
    }
  }

  return runProviderTestDirect(config);
}

export async function runProviderTestDirect(
  config: Omit<RuntimeConfig, "question">
): Promise<ProviderTestResult> {
  if (config.provider === "bitnet") {
    const result = await runBitnetTest(config);
    return buildReport({
      prompt: DISTILL_TEST_PROMPT,
      displayPrompt: DISTILL_TEST_DISPLAY_PROMPT,
      response: result.response,
      tokenPerSecond: result.tokenPerSecond,
      tokenCount: result.tokenCount,
      generationMs: result.generationMs,
      lines: result.lines,
      ok: result.ok
    });
  }

  const lines = [
    "provider: ollama",
    `host: ${config.host}`,
    `model: ${config.model}`
  ];
  try {
    const response = await requestOllamaDetailed({
      host: config.host,
      model: config.model,
      prompt: DISTILL_TEST_PROMPT,
      timeoutMs: config.timeoutMs,
      thinking: config.thinking
    });
    lines.push("generate: ok");
    return buildReport({
      prompt: DISTILL_TEST_PROMPT,
      displayPrompt: DISTILL_TEST_DISPLAY_PROMPT,
      response: response.output,
      tokenCount: response.evalCount,
      generationMs:
        response.evalDurationNs === undefined
          ? undefined
          : response.evalDurationNs / 1_000_000,
      lines,
      ok: true
    });
  } catch (error) {
    lines.push("generate: failed");
    lines.push(
      `error: ${error instanceof Error ? error.message : "Unknown Ollama error."}`
    );
    return buildReport({
      prompt: DISTILL_TEST_PROMPT,
      displayPrompt: DISTILL_TEST_DISPLAY_PROMPT,
      lines,
      ok: false
    });
  }
}
