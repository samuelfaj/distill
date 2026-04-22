import { describe, expect, it } from "bun:test";

import {
  DEFAULT_HOST,
  DEFAULT_MODEL,
  DEFAULT_TIMEOUT_MS,
  UsageError,
  parseCommand,
  resolveRuntimeDefaults
} from "../src/config";

describe("parseCommand", () => {
  it("parses defaults and joins the question", () => {
    const command = parseCommand(["what", "changed?"], {}, {});

    expect(command).toEqual({
      kind: "run",
      config: {
        question: "what changed?",
        model: DEFAULT_MODEL,
        host: DEFAULT_HOST,
        apiKey: "",
        timeoutMs: DEFAULT_TIMEOUT_MS
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
        "--api-key",
        "secret",
        "summarize"
      ],
      {},
      {}
    );

    expect(command).toEqual({
      kind: "run",
      config: {
        question: "summarize",
        model: "mini",
        host: "http://example.test",
        apiKey: "secret",
        timeoutMs: 10
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
        apiKey: "saved-key",
        timeoutMs: 50
      }
    );

    expect(command).toEqual({
      kind: "run",
      config: {
        question: "summarize",
        model: "saved-model",
        host: "http://saved.test",
        apiKey: "saved-key",
        timeoutMs: 50
      }
    });
  });

  it("prefers env over persisted defaults", () => {
    expect(
      resolveRuntimeDefaults(
        {
          DISTILL_MODEL: "env-model",
          DISTILL_HOST: "http://env.test",
          DISTILL_API_KEY: "env-key",
          DISTILL_TIMEOUT_MS: "999"
        },
        {
          model: "saved-model",
          host: "http://saved.test",
          apiKey: "saved-key",
          timeoutMs: 5
        }
      )
    ).toEqual({
      model: "env-model",
      host: "http://env.test",
      apiKey: "env-key",
      timeoutMs: 999
    });
  });

  it("parses config set commands", () => {
    expect(parseCommand(["config", "model", "my-model"], {}, {})).toEqual({
      kind: "configSet",
      key: "model",
      value: "my-model"
    });

    expect(
      parseCommand(["config", "host", "http://127.0.0.1:8010/v1"], {}, {})
    ).toEqual({
      kind: "configSet",
      key: "host",
      value: "http://127.0.0.1:8010/v1"
    });

    expect(parseCommand(["config", "timeout-ms", "30000"], {}, {})).toEqual({
      kind: "configSet",
      key: "timeout-ms",
      value: 30000
    });
  });

  it("rejects unknown config keys", () => {
    expect(() => parseCommand(["config", "provider", "openai"], {}, {})).toThrow(
      UsageError
    );
  });

  it("normalizes trailing slash on host", () => {
    expect(
      resolveRuntimeDefaults(
        { DISTILL_HOST: "http://example.test/v1/" },
        {}
      ).host
    ).toBe("http://example.test/v1");
  });

  it("throws on missing question", () => {
    expect(() => parseCommand([], {}, {})).toThrow(UsageError);
  });

  it("throws on unknown flag", () => {
    expect(() => parseCommand(["--provider", "openai", "q"], {}, {})).toThrow(
      UsageError
    );
  });
});
