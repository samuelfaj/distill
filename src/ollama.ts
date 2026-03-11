export interface OllamaRequest {
  host: string;
  model: string;
  prompt: string;
  timeoutMs: number;
  thinking: boolean;
  fetchImpl?: typeof fetch;
}

export interface OllamaResponse {
  output: string;
  evalCount?: number;
  evalDurationNs?: number;
}

export async function requestOllamaDetailed({
  host,
  model,
  prompt,
  timeoutMs,
  thinking,
  fetchImpl = fetch
}: OllamaRequest): Promise<OllamaResponse> {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), timeoutMs);

  try {
    const url = new URL("/api/generate", `${host}/`);
    const response = await fetchImpl(url, {
      method: "POST",
      headers: {
        "content-type": "application/json"
      },
      body: JSON.stringify({
        model,
        prompt,
        stream: false,
        think: thinking,
        options: {
          temperature: 0.1,
          num_predict: 80
        }
      }),
      signal: controller.signal
    });

    if (!response.ok) {
      throw new Error(`Ollama request failed with ${response.status}.`);
    }

    const rawText = await response.text();
    let payload: unknown;

    try {
      payload = JSON.parse(rawText);
    } catch {
      throw new Error("Ollama returned invalid JSON.");
    }

    if (
      typeof payload !== "object" ||
      payload === null ||
      typeof (payload as { response?: unknown }).response !== "string"
    ) {
      throw new Error("Ollama returned an invalid response payload.");
    }

    const output = (payload as { response: string }).response.trim();

    if (!output) {
      throw new Error("Ollama returned an empty response.");
    }

    const evalCount =
      typeof (payload as { eval_count?: unknown }).eval_count === "number"
        ? (payload as { eval_count: number }).eval_count
        : undefined;
    const evalDurationNs =
      typeof (payload as { eval_duration?: unknown }).eval_duration === "number"
        ? (payload as { eval_duration: number }).eval_duration
        : undefined;

    return {
      output,
      evalCount,
      evalDurationNs
    };
  } finally {
    clearTimeout(timeout);
  }
}

export async function requestOllama(request: OllamaRequest): Promise<string> {
  const response = await requestOllamaDetailed(request);
  return response.output;
}
