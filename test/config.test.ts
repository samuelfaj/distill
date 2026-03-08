import { describe, expect, it } from "bun:test";

import {
  DEFAULT_HOST,
  DEFAULT_MODEL,
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
        model: DEFAULT_MODEL,
        host: DEFAULT_HOST,
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
        model: "saved-model",
        host: "http://saved.test",
        timeoutMs: 50,
        thinking: true
      }
    });
  });

  it("parses config set commands", () => {
    expect(parseCommand(["config", "model", "phi3:mini"], {}, {})).toEqual({
      kind: "configSet",
      key: "model",
      value: "phi3:mini"
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
      model: "env-model",
      host: "http://env.test",
      timeoutMs: 999,
      thinking: true
    });
  });

  it("throws on missing question", () => {
    expect(() => parseCommand([], {}, {})).toThrow(UsageError);
  });
});
