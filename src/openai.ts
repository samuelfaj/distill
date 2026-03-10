export interface OpenAIRequest {
  baseUrl: string;
  apiKey: string;
  model: string;
  prompt: string;
  timeoutMs: number;
  fetchImpl?: typeof fetch;
}

export async function requestOpenAI({
  baseUrl,
  apiKey,
  model,
  prompt,
  timeoutMs,
  fetchImpl = fetch
}: OpenAIRequest): Promise<string> {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), timeoutMs);

  try {
    const url = new URL("/v1/chat/completions", `${baseUrl}/`);
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
      throw new Error(`OpenAI request failed with ${response.status}.`);
    }

    const rawText = await response.text();
    let payload: unknown;

    try {
      payload = JSON.parse(rawText);
    } catch {
      throw new Error("OpenAI returned invalid JSON.");
    }

    if (
      typeof payload !== "object" ||
      payload === null ||
      !Array.isArray((payload as { choices?: unknown }).choices) ||
      (payload as { choices: unknown[] }).choices.length === 0
    ) {
      throw new Error("OpenAI returned an invalid response payload.");
    }

    const choice = (payload as { choices: { message?: { content?: string } }[] })
      .choices[0];
    const content = choice?.message?.content?.trim();

    if (!content) {
      throw new Error("OpenAI returned an empty response.");
    }

    return content;
  } finally {
    clearTimeout(timeout);
  }
}
