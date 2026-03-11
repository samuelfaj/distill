import { describe, expect, it } from "bun:test";

import {
  DEFAULT_HOST,
  DEFAULT_JAN_BASE_URL,
  DEFAULT_LMSTUDIO_BASE_URL,
  DEFAULT_MODEL,
  DEFAULT_PROVIDER,
  DEFAULT_TIMEOUT_MS,
  parseCommand,
  resolveRuntimeDefaults,
  UsageError
} from "../src/config";

describe("parseCommand", () => {
  it("parses defaults and joins the question", () => {
    const command = parseCommand(["what", "changed?"], {}, {});

    expect(command).toEqual({
      kind: "run",
      config: {
        question: "what changed?",
        provider: DEFAULT_PROVIDER,
        model: DEFAULT_MODEL,
        host: DEFAULT_HOST,
        apiKey: "",
        timeoutMs: DEFAULT_TIMEOUT_MS,
        thinking: false
      }
    });
  });

  it("supports explicit flags", () => {
    const command = parseCommand(
      [
        "--model",
        "mini",
        "--host=http://example.test",
        "--timeout-ms",
        "10",
        "--thinking",
        "true",
        "summarize"
      ],
      {},
      {}
    );

    expect(command).toEqual({
      kind: "run",
      config: {
        question: "summarize",
        provider: "ollama",
        model: "mini",
        host: "http://example.test",
        apiKey: "",
        timeoutMs: 10,
        thinking: true
      }
    });
  });

  it("supports openai-compatible provider aliases with provider defaults", () => {
    const command = parseCommand(["--provider", "lmstudio", "summarize"], {}, {});

    expect(command).toEqual({
      kind: "run",
      config: {
        question: "summarize",
        provider: "lmstudio",
        model: DEFAULT_MODEL,
        host: DEFAULT_LMSTUDIO_BASE_URL,
        apiKey: "",
        timeoutMs: DEFAULT_TIMEOUT_MS,
        thinking: false
      }
    });
  });

  it("accepts the mlx-lm provider name", () => {
    const command = parseCommand(
      ["--provider", "mlx-lm", "--host", "http://127.0.0.1:8080/v1", "summarize"],
      {},
      {}
    );

    expect(command).toEqual({
      kind: "run",
      config: {
        question: "summarize",
        provider: "mlx-lm",
        model: DEFAULT_MODEL,
        host: "http://127.0.0.1:8080/v1",
        apiKey: "",
        timeoutMs: DEFAULT_TIMEOUT_MS,
        thinking: false
      }
    });
  });

  it("uses persisted defaults when present", () => {
    const command = parseCommand(
      ["summarize"],
      {},
      {
        model: "saved-model",
        host: "http://saved.test",
        timeoutMs: 50,
        thinking: true
      }
    );

    expect(command).toEqual({
      kind: "run",
      config: {
        question: "summarize",
        provider: "ollama",
        model: "saved-model",
        host: "http://saved.test",
        apiKey: "",
        timeoutMs: 50,
        thinking: true
      }
    });
  });

  it("parses config set commands", () => {
    expect(parseCommand(["config", "model", "qwen3.5:2b"], {}, {})).toEqual({
      kind: "configSet",
      key: "model",
      value: "qwen3.5:2b"
    });

    expect(parseCommand(["config", "thinking", "false"], {}, {})).toEqual({
      kind: "configSet",
      key: "thinking",
      value: false
    });
  });

  it("resolves env over persisted defaults", () => {
    expect(
      resolveRuntimeDefaults(
        {
          DISTILL_MODEL: "env-model",
          OLLAMA_HOST: "http://env.test",
          DISTILL_TIMEOUT_MS: "999",
          DISTILL_THINKING: "true"
        },
        {
          model: "saved-model",
          host: "http://saved.test",
          timeoutMs: 5,
          thinking: false
        }
      )
    ).toEqual({
      provider: "ollama",
      model: "env-model",
      host: "http://env.test",
      apiKey: "",
      timeoutMs: 999,
      thinking: true
    });
  });

  it("prefers generic env vars for openai-compatible providers", () => {
    expect(
      resolveRuntimeDefaults(
        {
          DISTILL_PROVIDER: "openai-compatible",
          DISTILL_HOST: "http://127.0.0.1:9000/v1",
          DISTILL_API_KEY: "token"
        },
        {}
      )
    ).toEqual({
      provider: "openai-compatible",
      model: DEFAULT_MODEL,
      host: "http://127.0.0.1:9000/v1",
      apiKey: "token",
      timeoutMs: DEFAULT_TIMEOUT_MS,
      thinking: false
    });
  });

  it("requires an api key for Jan", () => {
    expect(() => parseCommand(["--provider", "jan", "summarize"], {}, {})).toThrow(
      "Jan"
    );

    expect(
      parseCommand(
        ["--provider", "jan", "--api-key", "secret", "summarize"],
        {},
        {}
      )
    ).toEqual({
      kind: "run",
      config: {
        question: "summarize",
        provider: "jan",
        model: DEFAULT_MODEL,
        host: DEFAULT_JAN_BASE_URL,
        apiKey: "secret",
        timeoutMs: DEFAULT_TIMEOUT_MS,
        thinking: false
      }
    });
  });

  it("requires a host for the generic openai-compatible provider", () => {
    expect(() =>
      resolveRuntimeDefaults(
        {
          DISTILL_PROVIDER: "openai-compatible"
        },
        {}
      )
    ).toThrow("host is required");
  });

  it("throws on missing question", () => {
    expect(() => parseCommand([], {}, {})).toThrow(UsageError);
  });
});
