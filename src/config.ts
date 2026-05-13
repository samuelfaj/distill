import cliPackage from "../packages/cli/package.json";

export const DISTILL_VERSION = cliPackage.version;

export const DEFAULT_MODEL = "qwen3.5:2b";
export const DEFAULT_HOST = "http://127.0.0.1:11434/v1";
export const DEFAULT_TIMEOUT_MS = 90_000;
export const DEFAULT_MAX_TOKENS = 512;
export const DEFAULT_IDLE_MS = 1_200;
export const DEFAULT_INTERACTIVE_GAP_MS = 180;
export const DEFAULT_PROGRESS_FRAME_MS = 120;
export const DEFAULT_DATASET_ENABLED = true;

export interface DistillSettings {
  model: string;
  host: string;
  apiKey: string;
  timeoutMs: number;
  maxTokens: number;
  datasetEnabled: boolean;
  datasetPath?: string;
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
  | "max-tokens"
  | "dataset-enabled"
  | "dataset-path";

export type Command =
  | { kind: "onboard" }
  | { kind: "help" }
  | { kind: "version" }
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

function coerceMaxTokens(input: string | number | undefined): number {
  const value = Number(input ?? DEFAULT_MAX_TOKENS);

  if (!Number.isFinite(value) || value <= 0) {
    throw new UsageError("Max tokens must be a positive number.");
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
  const maxTokens = coerceMaxTokens(
    env.DISTILL_MAX_TOKENS ?? persisted.maxTokens ?? DEFAULT_MAX_TOKENS
  );
  const datasetEnabled = coerceBoolean(
    env.DISTILL_DATASET_ENABLED ?? persisted.datasetEnabled
  );
  const datasetPath = env.DISTILL_DATASET_PATH ?? persisted.datasetPath;

  return {
    model,
    host,
    apiKey,
    timeoutMs,
    maxTokens,
    datasetEnabled,
    datasetPath
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
      "max-tokens",
      "dataset-enabled",
      "dataset-path"
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

  if (key === "max-tokens") {
    return {
      kind: "configSet",
      key,
      value: coerceMaxTokens(rawValue)
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
        maxTokens: defaults.maxTokens,
        datasetEnabled: defaults.datasetEnabled,
        datasetPath: defaults.datasetPath
      }
    };
  }

  let timeoutMs = defaults.timeoutMs;
  let maxTokens = defaults.maxTokens;
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

    if (token === "-t") {
      const next = argv[index + 1];

      if (!next) {
        throw new UsageError("Missing value for -t.");
      }

      maxTokens = coerceMaxTokens(next);
      index += 1;
      continue;
    }

    if (token === "--max-tokens" || token.startsWith("--max-tokens=")) {
      const parsed = readFlagValue(argv, index, "--max-tokens");
      maxTokens = coerceMaxTokens(parsed.value);
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
      maxTokens,
      datasetEnabled: defaults.datasetEnabled,
      datasetPath: defaults.datasetPath
    }
  };
}

export function formatUsage(): string {
  return [
    "Usage:",
    '  cmd 2>&1 | distill "question"',
    '  distill translate "Best: Fix auth bug. Pass: tests pass." [language]',
    '  distill config host http://127.0.0.1:11434/v1',
    '  distill config model "qwen3.5:2b"',
    '  distill config max-tokens 1000',
    '  distill --host http://127.0.0.1:1234/v1 --model my-model --max-tokens 1000 "summarize"',
    "",
    "Options:",
    `  --model <name>        Model name (default: ${DEFAULT_MODEL})`,
    `  --host <url>          OpenAI-compatible base URL (default: ${DEFAULT_HOST})`,
    "  --api-key <key>       API key (env: DISTILL_API_KEY)",
    `  --timeout-ms <ms>     Request timeout in milliseconds (default: ${DEFAULT_TIMEOUT_MS})`,
    `  --max-tokens, -t <n>  Max completion/output tokens (default: ${DEFAULT_MAX_TOKENS})`,
    "",
    "Local fine-tuning capture (enabled by default):",
    "  Successful batch summaries are appended as JSONL under the config dir",
    "  (input + completion). The file is created with mode 0600.",
    "  DISTILL_DATASET_ENABLED=false  Disable local JSONL dataset capture",
    "  DISTILL_DATASET_PATH=<path>    Override dataset JSONL path",
    "  --help                Show usage",
    "  --version             Show version"
  ].join("\n");
}
