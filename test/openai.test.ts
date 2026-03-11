import { describe, expect, it } from "bun:test";

import { requestOpenAI } from "../src/openai";

describe("requestOpenAI", () => {
  it("preserves provider-specific base paths", async () => {
    let requestUrl = "";

    const output = await requestOpenAI({
      baseUrl: "http://127.0.0.1:12434/engines/v1",
      apiKey: "not-needed",
      model: "ai/llama3.2",
      prompt: "hi",
      providerLabel: "Docker Model Runner",
      timeoutMs: 100,
      fetchImpl: async (input) => {
        requestUrl = String(input);

        return new Response(
          JSON.stringify({
            choices: [{ message: { content: "  concise  " } }]
          }),
          { status: 200 }
        );
      }
    });

    expect(requestUrl).toBe("http://127.0.0.1:12434/engines/v1/chat/completions");
    expect(output).toBe("concise");
  });

  it("adds /v1 when the base URL does not include an API prefix", async () => {
    let requestUrl = "";

    await requestOpenAI({
      baseUrl: "http://127.0.0.1:8000",
      apiKey: "",
      model: "qwen",
      prompt: "hi",
      timeoutMs: 100,
      fetchImpl: async (input) => {
        requestUrl = String(input);

        return new Response(
          JSON.stringify({
            choices: [{ message: { content: "ok" } }]
          }),
          { status: 200 }
        );
      }
    });

    expect(requestUrl).toBe("http://127.0.0.1:8000/v1/chat/completions");
  });
});
