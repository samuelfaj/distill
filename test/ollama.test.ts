import { describe, expect, it } from "bun:test";

import { requestOllama } from "../src/ollama";

describe("requestOllama", () => {
  it("disables thinking in the Ollama request body", async () => {
    let requestBody: Record<string, unknown> | null = null;

    await requestOllama({
      host: "http://127.0.0.1:11434",
      model: "phi3:mini",
      prompt: "hi",
      timeoutMs: 100,
      thinking: false,
      fetchImpl: async (_input, init) => {
        requestBody = JSON.parse(String(init?.body));

        return new Response(JSON.stringify({ response: "concise" }), {
          status: 200
        });
      }
    });

    expect(requestBody).toMatchObject({
      model: "phi3:mini",
      prompt: "hi",
      stream: false,
      think: false
    });
  });

  it("returns the trimmed response", async () => {
    const output = await requestOllama({
      host: "http://127.0.0.1:11434",
      model: "phi3:mini",
      prompt: "hi",
      timeoutMs: 100,
      thinking: false,
      fetchImpl: async () =>
        new Response(JSON.stringify({ response: "  concise  " }), {
          status: 200
        })
    });

    expect(output).toBe("concise");
  });

  it("throws on non-200 responses", async () => {
    await expect(
      requestOllama({
        host: "http://127.0.0.1:11434",
        model: "phi3:mini",
        prompt: "hi",
        timeoutMs: 100,
        thinking: false,
        fetchImpl: async () => new Response("boom", { status: 500 })
      })
    ).rejects.toThrow("500");
  });

  it("throws on invalid JSON", async () => {
    await expect(
      requestOllama({
        host: "http://127.0.0.1:11434",
        model: "phi3:mini",
        prompt: "hi",
        timeoutMs: 100,
        thinking: false,
        fetchImpl: async () => new Response("nope", { status: 200 })
      })
    ).rejects.toThrow("invalid JSON");
  });
});
