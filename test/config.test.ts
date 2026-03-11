import { describe, expect, it } from "bun:test";

import {
  DEFAULT_PROVIDER,
  DEFAULT_HOST,
  DEFAULT_TIMEOUT_MS,
  getDefaultProvider,
  getDefaultModel,
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
        provider: DEFAULT_PROVIDER,
        question: "what changed?",
        model: getDefaultModel(DEFAULT_PROVIDER),
        host: DEFAULT_HOST,
        timeoutMs: DEFAULT_TIMEOUT_MS,
        thinking: false
      }
    });
  });

  it("supports explicit flags", () => {
    const command = parseCommand(
      [
        "--provider",
        "ollama",
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
        provider: "ollama",
        question: "summarize",
        model: "mini",
        host: "http://example.test",
        timeoutMs: 10,
        thinking: true
      }
    });
  });

  it("uses persisted defaults when present", () => {
    const command = parseCommand(
      ["summarize"],
      {},
      {
        provider: "ollama",
        model: "saved-model",
        host: "http://saved.test",
        timeoutMs: 50,
        thinking: true
      }
    );

    expect(command).toEqual({
      kind: "run",
      config: {
        provider: "ollama",
        question: "summarize",
        model: "saved-model",
        host: "http://saved.test",
        timeoutMs: 50,
        thinking: true
      }
    });
  });

  it("parses config set commands", () => {
    expect(parseCommand(["config", "provider", "bitnet"], {}, {})).toEqual({
      kind: "configSet",
      key: "provider",
      value: "bitnet"
    });

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
          DISTILL_PROVIDER: "ollama",
          OLLAMA_HOST: "http://env.test",
          DISTILL_TIMEOUT_MS: "999",
          DISTILL_THINKING: "true"
        },
        {
          provider: "bitnet",
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
      timeoutMs: 999,
      thinking: true
    });
  });

  it("uses provider-specific default models", () => {
    expect(resolveRuntimeDefaults({}, {}, "darwin", "arm64")).toMatchObject({
      provider: "bitnet",
      model: getDefaultModel("bitnet", "darwin", "arm64")
    });

    expect(resolveRuntimeDefaults({}, {}, "linux", "x64")).toMatchObject({
      provider: "bitnet",
      model: getDefaultModel("bitnet", "linux", "x64")
    });

    expect(resolveRuntimeDefaults({}, {}, "darwin", "x64")).toMatchObject({
      provider: "ollama",
      model: getDefaultModel("ollama", "darwin", "x64")
    });
  });

  it("parses the test subcommand", () => {
    expect(parseCommand(["test"], {}, {})).toEqual({
      kind: "test",
      config: {
        provider: "bitnet",
        model: getDefaultModel("bitnet"),
        host: DEFAULT_HOST,
        timeoutMs: DEFAULT_TIMEOUT_MS,
        thinking: false
      }
    });
  });

  it("parses the daemon subcommand", () => {
    expect(parseCommand(["daemon"], {}, {})).toEqual({
      kind: "daemon",
      config: {
        provider: "bitnet",
        model: getDefaultModel("bitnet"),
        host: DEFAULT_HOST,
        timeoutMs: DEFAULT_TIMEOUT_MS,
        thinking: false
      }
    });
  });

  it("rejects positional args for the test subcommand", () => {
    expect(() => parseCommand(["test", "extra"], {}, {})).toThrow(
      "does not accept positional arguments"
    );
  });

  it("does not validate ollama host for bitnet defaults", () => {
    expect(
      resolveRuntimeDefaults(
        {
          OLLAMA_HOST: ""
        },
        {},
        "darwin",
        "arm64"
      )
    ).toMatchObject({
      provider: "bitnet",
      host: DEFAULT_HOST
    });
  });

  it("rejects host with bitnet", () => {
    expect(() =>
      parseCommand(["--provider", "bitnet", "--host", "http://example.test", "q"], {}, {})
    ).toThrow("--host is only supported");
  });

  it("throws on missing question", () => {
    expect(() => parseCommand([], {}, {})).toThrow(UsageError);
  });
});
