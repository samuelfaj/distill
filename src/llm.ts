import type { RuntimeConfig } from "./config";
import {
  buildBatchPrompt,
  buildDslPromotionPrompt,
  buildThreadLearnPrompt,
  buildTranslatePrompt,
  buildWatchPrompt,
  type PromptMessages
} from "./prompt";

export interface ChatCompletionRequest {
  baseUrl: string;
  apiKey: string;
  model: string;
  prompt: string | PromptMessages;
  timeoutMs: number;
  maxTokens?: number;
  temperature?: number;
  fetchImpl?: typeof fetch;
}

function buildChatCompletionsUrl(baseUrl: string): URL {
  const normalized = new URL(baseUrl.endsWith("/") ? baseUrl : `${baseUrl}/`);
  const pathname = normalized.pathname.replace(/\/+$/, "");

  normalized.pathname =
    pathname === "" || pathname === "/"
      ? "/v1/chat/completions"
      : `${pathname}/chat/completions`;
  normalized.search = "";
  normalized.hash = "";

  return normalized;
}

export async function chatCompletion({
  baseUrl,
  apiKey,
  model,
  prompt,
  timeoutMs,
  maxTokens,
  temperature,
  fetchImpl = fetch
}: ChatCompletionRequest): Promise<string> {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), timeoutMs);

  try {
    const url = buildChatCompletionsUrl(baseUrl);
    const messages =
      typeof prompt === "string"
        ? [{ role: "user", content: prompt }]
        : [
            { role: "system", content: prompt.system },
            { role: "user", content: prompt.user }
          ];
    const response = await fetchImpl(url, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${apiKey}`
      },
      body: JSON.stringify({
        model,
        messages,
        temperature: temperature ?? 0,
        ...(maxTokens ? { max_tokens: maxTokens } : {})
      }),
      signal: controller.signal
    });

    if (!response.ok) {
      throw new Error(`Request failed with ${response.status}.`);
    }

    const rawText = await response.text();
    let payload: unknown;

    try {
      payload = JSON.parse(rawText);
    } catch {
      throw new Error("Provider returned invalid JSON.");
    }

    if (
      typeof payload !== "object" ||
      payload === null ||
      !Array.isArray((payload as { choices?: unknown }).choices) ||
      (payload as { choices: unknown[] }).choices.length === 0
    ) {
      throw new Error("Provider returned an invalid response payload.");
    }

    const choice = (payload as {
      choices: Array<{ message?: { content?: string } }>;
    }).choices[0];
    const content = choice?.message?.content?.trim();

    if (!content) {
      throw new Error("Provider returned an empty response.");
    }

    return content;
  } finally {
    clearTimeout(timeout);
  }
}

function summarize(
  config: RuntimeConfig,
  prompt: PromptMessages,
  fetchImpl?: typeof fetch
): Promise<string> {
  return chatCompletion({
    baseUrl: config.host,
    apiKey: config.apiKey,
    model: config.model,
    prompt,
    timeoutMs: config.timeoutMs,
    temperature: 0,
    maxTokens: 512,
    fetchImpl
  });
}

export function summarizeBatch(
  config: RuntimeConfig,
  input: string,
  optionsOrFetchImpl: { dslMemory?: string } | typeof fetch = {},
  fetchImpl?: typeof fetch
): Promise<string> {
  const options =
    typeof optionsOrFetchImpl === "function" ? {} : optionsOrFetchImpl;
  const resolvedFetchImpl =
    typeof optionsOrFetchImpl === "function" ? optionsOrFetchImpl : fetchImpl;

  return summarize(
    config,
    buildBatchPrompt(config.question, input, options),
    resolvedFetchImpl
  );
}

export function summarizeTranslate(
  config: RuntimeConfig,
  text: string,
  language: string,
  fetchImpl?: typeof fetch
): Promise<string> {
  return summarize(config, buildTranslatePrompt(text, language), fetchImpl);
}

export function summarizeWatch(
  config: RuntimeConfig,
  previousCycle: string,
  currentCycle: string,
  fetchImpl?: typeof fetch
): Promise<string> {
  return summarize(
    config,
    buildWatchPrompt(config.question, previousCycle, currentCycle),
    fetchImpl
  );
}

export function summarizeDslPromotion(
  config: RuntimeConfig,
  entries: string,
  fetchImpl?: typeof fetch
): Promise<string> {
  return summarize(config, buildDslPromotionPrompt(entries), fetchImpl);
}

export function summarizeThreadLearn(
  config: RuntimeConfig,
  transcript: string,
  candidates: Parameters<typeof buildThreadLearnPrompt>[1],
  dslMemory: string,
  fetchImpl?: typeof fetch
): Promise<string> {
  return summarize(
    config,
    buildThreadLearnPrompt(transcript, candidates, dslMemory),
    fetchImpl
  );
}
