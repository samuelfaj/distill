import { describe, expect, it } from "bun:test";

import {
  chatCompletion,
  summarizeBatch,
  summarizeTranslate,
  summarizeWatch
} from "../src/llm";
import type { RuntimeConfig } from "../src/config";

const baseConfig: RuntimeConfig = {
  question: "Did tests pass? Return PASS or FAIL.",
  model: "qwen3.5:2b",
  host: "http://127.0.0.1:11434/v1",
  apiKey: "",
  timeoutMs: 100,
  datasetEnabled: false
};

describe("chatCompletion", () => {
  it("preserves nested base paths", async () => {
    let requestUrl = "";

    const output = await chatCompletion({
      baseUrl: "http://127.0.0.1:12434/engines/v1",
      apiKey: "not-needed",
      model: "ai/llama3.2",
      prompt: "hi",
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

    await chatCompletion({
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

  it("throws when the provider returns a non-2xx status", async () => {
    await expect(
      chatCompletion({
        baseUrl: "http://127.0.0.1:8000",
        apiKey: "",
        model: "qwen",
        prompt: "hi",
        timeoutMs: 100,
        fetchImpl: async () => new Response("boom", { status: 500 })
      })
    ).rejects.toThrow("Request failed with 500.");
  });

  it("throws when the provider returns invalid JSON", async () => {
    await expect(
      chatCompletion({
        baseUrl: "http://127.0.0.1:8000",
        apiKey: "",
        model: "qwen",
        prompt: "hi",
        timeoutMs: 100,
        fetchImpl: async () => new Response("not-json", { status: 200 })
      })
    ).rejects.toThrow("Provider returned invalid JSON.");
  });

  it("throws when the response payload is missing choices", async () => {
    await expect(
      chatCompletion({
        baseUrl: "http://127.0.0.1:8000",
        apiKey: "",
        model: "qwen",
        prompt: "hi",
        timeoutMs: 100,
        fetchImpl: async () =>
          new Response(JSON.stringify({ choices: [] }), { status: 200 })
      })
    ).rejects.toThrow("Provider returned an invalid response payload.");

    await expect(
      chatCompletion({
        baseUrl: "http://127.0.0.1:8000",
        apiKey: "",
        model: "qwen",
        prompt: "hi",
        timeoutMs: 100,
        fetchImpl: async () =>
          new Response(JSON.stringify({}), { status: 200 })
      })
    ).rejects.toThrow("Provider returned an invalid response payload.");
  });

  it("throws when content is empty or whitespace-only", async () => {
    await expect(
      chatCompletion({
        baseUrl: "http://127.0.0.1:8000",
        apiKey: "",
        model: "qwen",
        prompt: "hi",
        timeoutMs: 100,
        fetchImpl: async () =>
          new Response(
            JSON.stringify({
              choices: [{ message: { content: "   " } }]
            }),
            { status: 200 }
          )
      })
    ).rejects.toThrow("Provider returned an empty response.");
  });
});

describe("summarizeBatch", () => {
  it("sends the batch prompt with config-derived params", async () => {
    let requestBody: unknown;

    const output = await summarizeBatch(
      baseConfig,
      "1 passed",
      async (_, init) => {
        requestBody = JSON.parse(String(init?.body ?? "{}"));

        return new Response(
          JSON.stringify({
            choices: [{ message: { content: "PASS" } }]
          }),
          { status: 200 }
        );
      }
    );

    expect(output).toBe("PASS");
    const body = requestBody as {
      model: string;
      messages: Array<{ role: string; content: string }>;
      temperature: number;
      max_tokens: number;
    };
    expect(body.model).toBe("qwen3.5:2b");
    expect(body.temperature).toBe(0);
    expect(body.max_tokens).toBe(512);
    expect(body.messages[0].role).toBe("system");
    expect(body.messages[1].role).toBe("user");
    expect(body.messages[1].content).toContain("1 passed");
    expect(body.messages[1].content).toContain(baseConfig.question);
  });

  it("injects compact DSL memory into the batch system prompt", async () => {
    let requestBody: unknown;

    const output = await summarizeBatch(
      baseConfig,
      "auth failed",
      { dslMemory: "AUTH = authentication fix (alias, project)" },
      async (_, init) => {
        requestBody = JSON.parse(String(init?.body ?? "{}"));

        return new Response(
          JSON.stringify({
            choices: [{ message: { content: "AUTH fixed" } }]
          }),
          { status: 200 }
        );
      }
    );

    const body = requestBody as {
      messages: Array<{ role: string; content: string }>;
    };

    expect(output).toBe("AUTH fixed");
    expect(body.messages[0].content).toContain("Known /distill DSL memory");
    expect(body.messages[0].content).toContain(
      "AUTH = authentication fix (alias, project)"
    );
    expect(body.messages[0].content).toContain("term=#x1");
    expect(body.messages[0].content).toContain("workspace=#w3");
    expect(body.messages[0].content).toContain("Emit Dict+ only");
  });
});

describe("summarizeTranslate", () => {
  it("asks the provider to expand /distill Military English into human language", async () => {
    let systemContent = "";
    let userContent = "";

    const output = await summarizeTranslate(
      baseConfig,
      [
        "Dict: be=backend fe=frontend",
        "Best:",
        "Fix auth bug.",
        "Add failing test first.",
        "No fe change.",
        "Pass: valid user allowed, tests pass.",
        "More aggressive:",
        "Fix be auth only.",
        "Tradeoff:",
        "Less context for reviewer."
      ].join("\n"),
      "en-US",
      async (_, init) => {
        const body = JSON.parse(String(init?.body ?? "{}")) as {
          messages: Array<{ role: string; content: string }>;
        };
        systemContent = body.messages[0].content;
        userContent = body.messages[1].content;

        return new Response(
          JSON.stringify({
            choices: [
              {
                message: {
                  content: "Done because tests passed. Next step: ship it."
                }
              }
            ]
          }),
          { status: 200 }
        );
      }
    );

    expect(output).toBe("Done because tests passed. Next step: ship it.");
    expect(systemContent).toContain("Military English");
    expect(systemContent).toContain("Best");
    expect(systemContent).toContain("Dict");
    expect(systemContent).toContain("Pass");
    expect(userContent).toContain("Dict: be=backend fe=frontend");
    expect(userContent).toContain("No fe change.");
    expect(userContent).toContain("en-US");
  });
});

describe("summarizeWatch", () => {
  it("sends both cycles in the watch prompt", async () => {
    let userContent = "";

    await summarizeWatch(
      baseConfig,
      "failed: 0",
      "failed: 1",
      async (_, init) => {
        const body = JSON.parse(String(init?.body ?? "{}")) as {
          messages: Array<{ role: string; content: string }>;
        };
        userContent = body.messages[1].content;

        return new Response(
          JSON.stringify({
            choices: [{ message: { content: "failure count rose" } }]
          }),
          { status: 200 }
        );
      }
    );

    expect(userContent).toContain("failed: 0");
    expect(userContent).toContain("failed: 1");
  });
});
