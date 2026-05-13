import cliPackage from "../packages/cli/package.json";

export const DISTILL_VERSION = cliPackage.version;

export const DEFAULT_MODEL = "qwen3.5:2b";
export const DEFAULT_HOST = "http://127.0.0.1:11434/v1";
export const DEFAULT_TIMEOUT_MS = 90_000;
export const DEFAULT_IDLE_MS = 1_200;
export const DEFAULT_INTERACTIVE_GAP_MS = 180;
export const DEFAULT_PROGRESS_FRAME_MS = 120;
export const DEFAULT_DATASET_ENABLED = true;
export const DEFAULT_AUTO_LEARN = true;
export const DEFAULT_AUTO_LEARN_SCOPE = "project";
export const DEFAULT_AUTO_LEARN_SOURCE = "output";
export const DEFAULT_AUTO_PROMOTE_SCOPES = true;
export const DEFAULT_MAX_PROMPT_DSL_ENTRIES = 40;

export interface DistillSettings {
  model: string;
  host: string;
  apiKey: string;
  timeoutMs: number;
  datasetEnabled: boolean;
  datasetPath?: string;
  autoLearn?: boolean;
  autoLearnScope?: "project";
  autoLearnSource?: "output";
  autoPromoteScopes?: boolean;
  maxPromptDslEntries?: number;
}

export interface RuntimeConfig extends DistillSettings {
  question: string;
}

export type PersistedConfig = Partial<DistillSettings>;

export type ConfigKey =
  | "model"
  | "host"
  | "api-key"
  | "timeout-ms"
  | "dataset-enabled"
  | "dataset-path"
  | "auto-learn"
  | "auto-promote-scopes"
  | "max-prompt-dsl-entries";

export type Command =
  | { kind: "onboard" }
  | { kind: "help" }
  | { kind: "version" }
  | { kind: "dsl"; args: string[] }
  | { kind: "configShow" }
  | { kind: "configGet"; key: ConfigKey }
  | { kind: "configSet"; key: ConfigKey; value: string | number | boolean }
  | {
      kind: "translate";
      text: string;
      language: string;
      config: RuntimeConfig;
    }
  | { kind: "run"; config: RuntimeConfig };

export class UsageError extends Error {
  readonly exitCode = 2;

  constructor(message: string) {
    super(message);
    this.name = "UsageError";
  }
}

function readFlagValue(
  argv: string[],
  index: number,
  name: string
): { value: string; nextIndex: number } {
  const current = argv[index];
  const inline = current.slice(name.length + 1);

  if (inline.length > 0) {
    return { value: inline, nextIndex: index };
  }

  const next = argv[index + 1];

  if (!next) {
    throw new UsageError(`Missing value for ${name}.`);
  }

  return { value: next, nextIndex: index + 1 };
}

function coerceTimeout(input: string | undefined): number {
  const value = Number(input ?? DEFAULT_TIMEOUT_MS);

  if (!Number.isFinite(value) || value <= 0) {
    throw new UsageError("Timeout must be a positive number.");
  }

  return Math.floor(value);
}

function normalizeHost(input: string | undefined): string {
  const value = (input ?? DEFAULT_HOST).trim();

  if (!value) {
    throw new UsageError("Host cannot be empty.");
  }

  return value.endsWith("/") ? value.slice(0, -1) : value;
}

function coerceBoolean(input: string | boolean | undefined): boolean {
  if (typeof input === "boolean") {
    return input;
  }

  const value = String(input ?? DEFAULT_DATASET_ENABLED).trim().toLowerCase();

  if (["1", "true", "yes", "on"].includes(value)) {
    return true;
  }

  if (["0", "false", "no", "off"].includes(value)) {
    return false;
  }

  throw new UsageError("Boolean values must be true or false.");
}

function coercePositiveInteger(input: string | number | undefined, label: string): number {
  const value = Number(input);

  if (!Number.isInteger(value) || value <= 0) {
    throw new UsageError(`${label} must be a positive integer.`);
  }

  return value;
}

export function resolveRuntimeDefaults(
  env: NodeJS.ProcessEnv,
  persisted: PersistedConfig
): DistillSettings {
  const model = env.DISTILL_MODEL ?? persisted.model ?? DEFAULT_MODEL;
  const host = normalizeHost(
    env.DISTILL_HOST ?? persisted.host ?? DEFAULT_HOST
  );
  const apiKey = env.DISTILL_API_KEY ?? persisted.apiKey ?? "";
  const timeoutMs = coerceTimeout(
    env.DISTILL_TIMEOUT_MS ?? String(persisted.timeoutMs ?? DEFAULT_TIMEOUT_MS)
  );
  const datasetEnabled = coerceBoolean(
    env.DISTILL_DATASET_ENABLED ?? persisted.datasetEnabled
  );
  const datasetPath = env.DISTILL_DATASET_PATH ?? persisted.datasetPath;
  const autoLearn = coerceBoolean(
    env.DISTILL_AUTO_LEARN ?? persisted.autoLearn ?? DEFAULT_AUTO_LEARN
  );
  const autoPromoteScopes = coerceBoolean(
    env.DISTILL_AUTO_PROMOTE_SCOPES ??
      persisted.autoPromoteScopes ??
      DEFAULT_AUTO_PROMOTE_SCOPES
  );
  const maxPromptDslEntries = coercePositiveInteger(
    env.DISTILL_MAX_PROMPT_DSL_ENTRIES ??
      persisted.maxPromptDslEntries ??
      DEFAULT_MAX_PROMPT_DSL_ENTRIES,
    "max-prompt-dsl-entries"
  );

  return {
    model,
    host,
    apiKey,
    timeoutMs,
    datasetEnabled,
    datasetPath,
    autoLearn,
    autoLearnScope: DEFAULT_AUTO_LEARN_SCOPE,
    autoLearnSource: DEFAULT_AUTO_LEARN_SOURCE,
    autoPromoteScopes,
    maxPromptDslEntries
  };
}

function parseConfigCommand(argv: string[]): Command {
  if (argv.length === 1) {
    return { kind: "configShow" };
  }

  const key = argv[1] as ConfigKey;

  if (
    ![
      "model",
      "host",
      "api-key",
      "timeout-ms",
      "dataset-enabled",
      "dataset-path",
      "auto-learn",
      "auto-promote-scopes",
      "max-prompt-dsl-entries"
    ].includes(key)
  ) {
    throw new UsageError(`Unknown config key: ${argv[1]}`);
  }

  if (argv.length === 2) {
    return { kind: "configGet", key };
  }

  const rawValue = argv.slice(2).join(" ").trim();

  if (!rawValue) {
    throw new UsageError(`Missing value for config key ${key}.`);
  }

  if (key === "timeout-ms") {
    return {
      kind: "configSet",
      key,
      value: coerceTimeout(rawValue)
    };
  }

  if (key === "host") {
    return {
      kind: "configSet",
      key,
      value: normalizeHost(rawValue)
    };
  }

  if (key === "dataset-enabled") {
    return {
      kind: "configSet",
      key,
      value: coerceBoolean(rawValue)
    };
  }

  if (key === "auto-learn" || key === "auto-promote-scopes") {
    return {
      kind: "configSet",
      key,
      value: coerceBoolean(rawValue)
    };
  }

  if (key === "max-prompt-dsl-entries") {
    return {
      kind: "configSet",
      key,
      value: coercePositiveInteger(rawValue, key)
    };
  }

  return {
    kind: "configSet",
    key,
    value: rawValue
  };
}

export function parseCommand(
  argv: string[],
  env: NodeJS.ProcessEnv,
  persisted: PersistedConfig = {}
): Command {
  if (argv.length === 0) {
    return { kind: "onboard" };
  }

  if (argv[0] === "config") {
    return parseConfigCommand(argv);
  }

  if (argv[0] === "dsl") {
    return { kind: "dsl", args: argv.slice(1) };
  }

  if (argv.length === 1 && (argv[0] === "--help" || argv[0] === "-h")) {
    return { kind: "help" };
  }

  if (argv.length === 1 && (argv[0] === "--version" || argv[0] === "-v")) {
    return { kind: "version" };
  }

  const defaults = resolveRuntimeDefaults(env, persisted);

  if (argv[0] === "translate") {
    if (!argv[1]?.trim()) {
      throw new UsageError("/distill text is required.");
    }

    if (argv.length > 3) {
      throw new UsageError("Usage: distill translate <text> [language]");
    }

    return {
      kind: "translate",
      text: argv[1],
      language: argv[2] ?? "en-US",
      config: {
        question: "Translate /distill output into human language.",
        model: defaults.model,
        host: defaults.host,
        apiKey: defaults.apiKey,
        timeoutMs: defaults.timeoutMs,
        datasetEnabled: defaults.datasetEnabled,
        datasetPath: defaults.datasetPath,
        autoLearn: defaults.autoLearn,
        autoLearnScope: defaults.autoLearnScope,
        autoLearnSource: defaults.autoLearnSource,
        autoPromoteScopes: defaults.autoPromoteScopes,
        maxPromptDslEntries: defaults.maxPromptDslEntries
      }
    };
  }

  let timeoutMs = defaults.timeoutMs;
  let modelOverride: string | undefined;
  let hostOverride: string | undefined;
  let apiKeyOverride: string | undefined;
  const questionParts: string[] = [];

  for (let index = 0; index < argv.length; index += 1) {
    const token = argv[index];

    if (token === "--") {
      questionParts.push(...argv.slice(index + 1));
      break;
    }

    if (token === "--model" || token.startsWith("--model=")) {
      const parsed = readFlagValue(argv, index, "--model");
      modelOverride = parsed.value;
      index = parsed.nextIndex;
      continue;
    }

    if (token === "--host" || token.startsWith("--host=")) {
      const parsed = readFlagValue(argv, index, "--host");
      hostOverride = parsed.value;
      index = parsed.nextIndex;
      continue;
    }

    if (token === "--api-key" || token.startsWith("--api-key=")) {
      const parsed = readFlagValue(argv, index, "--api-key");
      apiKeyOverride = parsed.value;
      index = parsed.nextIndex;
      continue;
    }

    if (token === "--timeout-ms" || token.startsWith("--timeout-ms=")) {
      const parsed = readFlagValue(argv, index, "--timeout-ms");
      timeoutMs = coerceTimeout(parsed.value);
      index = parsed.nextIndex;
      continue;
    }

    if (token.startsWith("-")) {
      throw new UsageError(`Unknown flag: ${token}`);
    }

    questionParts.push(token);
  }

  const question = questionParts.join(" ").trim();

  if (!question) {
    throw new UsageError("A question is required.");
  }

  const model = modelOverride ?? defaults.model;
  const host = hostOverride ? normalizeHost(hostOverride) : defaults.host;
  const apiKey = apiKeyOverride ?? defaults.apiKey;

  return {
    kind: "run",
    config: {
      question,
      model,
      host,
      apiKey,
      timeoutMs,
      datasetEnabled: defaults.datasetEnabled,
      datasetPath: defaults.datasetPath,
      autoLearn: defaults.autoLearn,
      autoLearnScope: defaults.autoLearnScope,
      autoLearnSource: defaults.autoLearnSource,
      autoPromoteScopes: defaults.autoPromoteScopes,
      maxPromptDslEntries: defaults.maxPromptDslEntries
    }
  };
}

export function formatUsage(): string {
  return [
    "Usage:",
    '  cmd 2>&1 | distill "question"',
    "  distill dsl show",
    "  distill dsl show --candidates",
    '  distill dsl learn --dry-run "Dict+: A1=auth fix"',
    "  distill dsl promote --dry-run",
    '  distill dsl add alias A1 "auth fix" --scope project',
    '  distill translate "Best: Fix auth bug. Pass: tests pass." [language]',
    '  distill config host http://127.0.0.1:11434/v1',
    '  distill config model "qwen3.5:2b"',
    '  distill --host http://127.0.0.1:1234/v1 --model my-model "summarize"',
    "",
    "Options:",
    `  --model <name>        Model name (default: ${DEFAULT_MODEL})`,
    `  --host <url>          OpenAI-compatible base URL (default: ${DEFAULT_HOST})`,
    "  --api-key <key>       API key (env: DISTILL_API_KEY)",
    `  --timeout-ms <ms>     Request timeout in milliseconds (default: ${DEFAULT_TIMEOUT_MS})`,
    "",
    "Local fine-tuning capture (enabled by default):",
    "  Successful batch summaries are appended as JSONL under the config dir",
    "  (input + completion). The file is created with mode 0600.",
    "  DISTILL_DATASET_ENABLED=false  Disable local JSONL dataset capture",
    "  DISTILL_DATASET_PATH=<path>    Override dataset JSONL path",
    "  DISTILL_AUTO_LEARN=false       Disable project-scoped DSL auto-learn",
    "  DISTILL_MAX_PROMPT_DSL_ENTRIES=<n>  Limit DSL entries injected into prompts",
    "  --help                Show usage",
    "  --version             Show version"
  ].join("\n");
}
