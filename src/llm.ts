import type { RuntimeConfig } from "./config";
import {
  buildBatchPrompt,
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

function buildApiUrl(baseUrl: string, endpointPath: string): URL {
  const normalized = new URL(baseUrl.endsWith("/") ? baseUrl : `${baseUrl}/`);
  const pathname = normalized.pathname.replace(/\/+$/, "");

  normalized.pathname =
    pathname === "" || pathname === "/"
      ? `/v1/${endpointPath}`
      : `${pathname}/${endpointPath}`;
  normalized.search = "";
  normalized.hash = "";

  return normalized;
}

function buildChatCompletionsUrl(baseUrl: string): URL {
  return buildApiUrl(baseUrl, "chat/completions");
}

function buildResponsesUrl(baseUrl: string): URL {
  return buildApiUrl(baseUrl, "responses");
}

function buildMessages(prompt: string | PromptMessages) {
  return typeof prompt === "string"
    ? [{ role: "user", content: prompt }]
    : [
        { role: "system", content: prompt.system },
        { role: "user", content: prompt.user }
      ];
}

function isOfficialOpenAIHost(baseUrl: string): boolean {
  const hostname = new URL(baseUrl).hostname.toLowerCase();

  return hostname === "api.openai.com" || hostname.endsWith(".openai.com");
}

function isOpenAIReasoningModel(model: string): boolean {
  const normalized = model.trim();

  return /^gpt-5(?:[.-]|$)/i.test(normalized) || /^o[1-9](?:[.-]|$)/i.test(normalized);
}

function shouldUseOpenAIResponses(baseUrl: string, model: string): boolean {
  return isOfficialOpenAIHost(baseUrl) && isOpenAIReasoningModel(model);
}

function extractProviderError(rawText: string, status: number): Error {
  const trimmed = rawText.trim();

  if (!trimmed) {
    return new Error(`Request failed with ${status}.`);
  }

  try {
    const payload = JSON.parse(trimmed) as {
      error?: { message?: unknown };
      message?: unknown;
    };
    const message =
      typeof payload.error?.message === "string"
        ? payload.error.message
        : typeof payload.message === "string"
          ? payload.message
          : null;

    if (message) {
      return new Error(`Request failed with ${status}: ${message}`);
    }
  } catch {
    // Fall back to the raw response body below.
  }

  return new Error(`Request failed with ${status}: ${trimmed}`);
}

function extractIncompleteResponseError(payload: unknown): Error | null {
  if (typeof payload !== "object" || payload === null) {
    return null;
  }

  const status = (payload as { status?: unknown }).status;

  if (status !== "incomplete") {
    return null;
  }

  const reason = (payload as {
    incomplete_details?: { reason?: unknown };
  }).incomplete_details?.reason;

  if (typeof reason === "string" && reason.trim()) {
    return new Error(`Provider returned an incomplete response: ${reason.trim()}.`);
  }

  return new Error("Provider returned an incomplete response.");
}

function extractResponsesOutput(payload: unknown): string {
  const incompleteError = extractIncompleteResponseError(payload);

  if (incompleteError) {
    throw incompleteError;
  }

  if (
    typeof payload === "object" &&
    payload !== null &&
    typeof (payload as { output_text?: unknown }).output_text === "string"
  ) {
    const outputText = (payload as { output_text: string }).output_text.trim();

    if (outputText) {
      return outputText;
    }
  }

  if (
    typeof payload !== "object" ||
    payload === null ||
    !Array.isArray((payload as { output?: unknown }).output)
  ) {
    throw new Error("Provider returned an invalid response payload.");
  }

  const output = (payload as {
    output: Array<{
      type?: string;
      role?: string;
      content?: Array<{ type?: string; text?: string }>;
    }>;
  }).output;

  const text = output
    .filter((item) => item.type === "message" && item.role === "assistant")
    .flatMap((item) => item.content ?? [])
    .map((item) => item.text?.trim() ?? "")
    .filter(Boolean)
    .join("\n")
    .trim();

  if (!text) {
    throw new Error("Provider returned an empty response.");
  }

  return text;
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
    const messages = buildMessages(prompt);
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
      throw extractProviderError(await response.text(), response.status);
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

async function responsesCompletion({
  baseUrl,
  apiKey,
  model,
  prompt,
  timeoutMs,
  maxTokens,
  fetchImpl = fetch
}: ChatCompletionRequest): Promise<string> {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), timeoutMs);

  try {
    const response = await fetchImpl(buildResponsesUrl(baseUrl), {
      method: "POST",
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${apiKey}`
      },
      body: JSON.stringify({
        model,
        input: buildMessages(prompt),
        ...(maxTokens ? { max_output_tokens: maxTokens } : {})
      }),
      signal: controller.signal
    });

    if (!response.ok) {
      throw extractProviderError(await response.text(), response.status);
    }

    const rawText = await response.text();
    let payload: unknown;

    try {
      payload = JSON.parse(rawText);
    } catch {
      throw new Error("Provider returned invalid JSON.");
    }

    return extractResponsesOutput(payload);
  } finally {
    clearTimeout(timeout);
  }
}

async function summarize(
  config: RuntimeConfig,
  prompt: PromptMessages,
  fetchImpl?: typeof fetch
): Promise<string> {
  const request: ChatCompletionRequest = {
    baseUrl: config.host,
    apiKey: config.apiKey,
    model: config.model,
    prompt,
    timeoutMs: config.timeoutMs,
    maxTokens: config.maxTokens,
    temperature: 0,
    fetchImpl
  };

  if (shouldUseOpenAIResponses(config.host, config.model)) {
    return responsesCompletion(request);
  }

  return chatCompletion(request);
}

export function summarizeBatch(
  config: RuntimeConfig,
  input: string,
  fetchImpl?: typeof fetch
): Promise<string> {
  return summarize(config, buildBatchPrompt(config.question, input), fetchImpl);
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
