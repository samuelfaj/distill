export interface OpenAIRequest {
  baseUrl: string;
  apiKey: string;
  model: string;
  prompt: string;
  providerLabel?: string;
  timeoutMs: number;
  fetchImpl?: typeof fetch;
}

function buildChatCompletionsUrl(baseUrl: string): URL {
  const normalized = new URL(baseUrl.endsWith("/") ? baseUrl : `${baseUrl}/`);
  const pathname = normalized.pathname.replace(/\/+$/, "");

  normalized.pathname =
    pathname === "" || pathname === "/" ? "/v1/chat/completions" : `${pathname}/chat/completions`;
  normalized.search = "";
  normalized.hash = "";

  return normalized;
}

export async function requestOpenAI({
  baseUrl,
  apiKey,
  model,
  prompt,
  providerLabel = "OpenAI-compatible provider",
  timeoutMs,
  fetchImpl = fetch
}: OpenAIRequest): Promise<string> {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), timeoutMs);

  try {
    const url = buildChatCompletionsUrl(baseUrl);
    const response = await fetchImpl(url, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${apiKey}`
      },
      body: JSON.stringify({
        model,
        messages: [{ role: "user", content: prompt }],
        temperature: 0.1,
        max_tokens: 200
      }),
      signal: controller.signal
    });

    if (!response.ok) {
      throw new Error(`${providerLabel} request failed with ${response.status}.`);
    }

    const rawText = await response.text();
    let payload: unknown;

    try {
      payload = JSON.parse(rawText);
    } catch {
      throw new Error(`${providerLabel} returned invalid JSON.`);
    }

    if (
      typeof payload !== "object" ||
      payload === null ||
      !Array.isArray((payload as { choices?: unknown }).choices) ||
      (payload as { choices: unknown[] }).choices.length === 0
    ) {
      throw new Error(`${providerLabel} returned an invalid response payload.`);
    }

    const choice = (payload as { choices: { message?: { content?: string } }[] })
      .choices[0];
    const content = choice?.message?.content?.trim();

    if (!content) {
      throw new Error(`${providerLabel} returned an empty response.`);
    }

    return content;
  } finally {
    clearTimeout(timeout);
  }
}
