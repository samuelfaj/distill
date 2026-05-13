import { describe, expect, it } from "bun:test";

import {
  DEFAULT_AUTO_LEARN,
  DEFAULT_AUTO_LEARN_SCOPE,
  DEFAULT_AUTO_LEARN_SOURCE,
  DEFAULT_AUTO_PROMOTE_SCOPES,
  DEFAULT_HOST,
  DEFAULT_MAX_PROMPT_DSL_ENTRIES,
  DEFAULT_MODEL,
  DEFAULT_TIMEOUT_MS,
  UsageError,
  parseCommand,
  resolveRuntimeDefaults
} from "../src/config";

const defaultAutoLearnConfig = {
  autoLearn: DEFAULT_AUTO_LEARN,
  autoLearnScope: DEFAULT_AUTO_LEARN_SCOPE,
  autoLearnSource: DEFAULT_AUTO_LEARN_SOURCE,
  autoPromoteScopes: DEFAULT_AUTO_PROMOTE_SCOPES,
  maxPromptDslEntries: DEFAULT_MAX_PROMPT_DSL_ENTRIES
};

describe("parseCommand", () => {
  it("parses no arguments as onboarding", () => {
    expect(parseCommand([], {}, {})).toEqual({ kind: "onboard" });
  });

  it("parses dsl commands", () => {
    expect(parseCommand(["dsl", "show", "--scope", "global"], {}, {})).toEqual({
      kind: "dsl",
      args: ["show", "--scope", "global"]
    });
  });

  it("parses defaults and joins the question", () => {
    const command = parseCommand(["what", "changed?"], {}, {});

    expect(command).toEqual({
      kind: "run",
      config: {
        question: "what changed?",
        model: DEFAULT_MODEL,
        host: DEFAULT_HOST,
        apiKey: "",
        timeoutMs: DEFAULT_TIMEOUT_MS,
        datasetEnabled: true,
        datasetPath: undefined,
        ...defaultAutoLearnConfig
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
        timeoutMs: 10,
        datasetEnabled: true,
        datasetPath: undefined,
        ...defaultAutoLearnConfig
      }
    });
  });

  it("parses translate command with the default human language", () => {
    expect(parseCommand(["translate", "Best:\nFix auth bug.\nPass: tests pass"], {}, {})).toEqual({
      kind: "translate",
      text: "Best:\nFix auth bug.\nPass: tests pass",
      language: "en-US",
      config: {
        question: "Translate /distill output into human language.",
        model: DEFAULT_MODEL,
        host: DEFAULT_HOST,
        apiKey: "",
        timeoutMs: DEFAULT_TIMEOUT_MS,
        datasetEnabled: true,
        datasetPath: undefined,
        ...defaultAutoLearnConfig
      }
    });
  });

  it("parses translate command with an explicit human language", () => {
    expect(parseCommand(["translate", "Dict: be=backend\nDo: patch be", "pt-BR"], {}, {})).toEqual({
      kind: "translate",
      text: "Dict: be=backend\nDo: patch be",
      language: "pt-BR",
      config: {
        question: "Translate /distill output into human language.",
        model: DEFAULT_MODEL,
        host: DEFAULT_HOST,
        apiKey: "",
        timeoutMs: DEFAULT_TIMEOUT_MS,
        datasetEnabled: true,
        datasetPath: undefined,
        ...defaultAutoLearnConfig
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
        timeoutMs: 50,
        datasetEnabled: false,
        datasetPath: "/tmp/distill.jsonl"
      }
    );

    expect(command).toEqual({
      kind: "run",
      config: {
        question: "summarize",
        model: "saved-model",
        host: "http://saved.test",
        apiKey: "saved-key",
        timeoutMs: 50,
        datasetEnabled: false,
        datasetPath: "/tmp/distill.jsonl",
        ...defaultAutoLearnConfig
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
      DISTILL_TIMEOUT_MS: "999",
      DISTILL_DATASET_ENABLED: "false",
      DISTILL_DATASET_PATH: "/tmp/env-distill.jsonl",
      DISTILL_AUTO_LEARN: "false",
      DISTILL_AUTO_PROMOTE_SCOPES: "false",
      DISTILL_MAX_PROMPT_DSL_ENTRIES: "12"
        },
        {
          model: "saved-model",
          host: "http://saved.test",
          apiKey: "saved-key",
          timeoutMs: 5,
          datasetEnabled: true,
          datasetPath: "/tmp/saved-distill.jsonl"
        }
      )
    ).toEqual({
      model: "env-model",
      host: "http://env.test",
      apiKey: "env-key",
      timeoutMs: 999,
      datasetEnabled: false,
      datasetPath: "/tmp/env-distill.jsonl",
      autoLearn: false,
      autoLearnScope: "project",
      autoLearnSource: "output",
      autoPromoteScopes: false,
      maxPromptDslEntries: 12
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

    expect(parseCommand(["config", "dataset-enabled", "false"], {}, {})).toEqual({
      kind: "configSet",
      key: "dataset-enabled",
      value: false
    });

    expect(
      parseCommand(["config", "dataset-path", "/tmp/distill.jsonl"], {}, {})
    ).toEqual({
      kind: "configSet",
      key: "dataset-path",
      value: "/tmp/distill.jsonl"
    });

    expect(parseCommand(["config", "auto-learn", "false"], {}, {})).toEqual({
      kind: "configSet",
      key: "auto-learn",
      value: false
    });

    expect(parseCommand(["config", "auto-promote-scopes", "false"], {}, {})).toEqual({
      kind: "configSet",
      key: "auto-promote-scopes",
      value: false
    });

    expect(parseCommand(["config", "max-prompt-dsl-entries", "12"], {}, {})).toEqual({
      kind: "configSet",
      key: "max-prompt-dsl-entries",
      value: 12
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

  it("throws on missing translate text", () => {
    expect(() => parseCommand(["translate"], {}, {})).toThrow(UsageError);
  });

  it("throws on extra translate arguments", () => {
    expect(() =>
      parseCommand(["translate", "Best:\nDone.", "pt-BR", "extra"], {}, {})
    ).toThrow(UsageError);
  });

  it("throws on unknown flag", () => {
    expect(() => parseCommand(["--provider", "openai", "q"], {}, {})).toThrow(
      UsageError
    );
  });
});
