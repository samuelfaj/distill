import cliPackage from "../packages/cli/package.json";

export const DISTILL_VERSION = cliPackage.version;

export const DEFAULT_MODEL = "qwen3.5:2b";
export const DEFAULT_HOST = "http://127.0.0.1:11434/v1";
export const DEFAULT_TIMEOUT_MS = 90_000;
export const DEFAULT_IDLE_MS = 1_200;
export const DEFAULT_INTERACTIVE_GAP_MS = 180;
export const DEFAULT_PROGRESS_FRAME_MS = 120;

export interface RuntimeConfig {
  question: string;
  model: string;
  host: string;
  apiKey: string;
  timeoutMs: number;
}

export interface PersistedConfig {
  model?: string;
  host?: string;
  apiKey?: string;
  timeoutMs?: number;
}

export type ConfigKey = "model" | "host" | "api-key" | "timeout-ms";

export type Command =
  | { kind: "help" }
  | { kind: "version" }
  | { kind: "configShow" }
  | { kind: "configGet"; key: ConfigKey }
  | { kind: "configSet"; key: ConfigKey; value: string | number }
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

export function resolveRuntimeDefaults(
  env: NodeJS.ProcessEnv,
  persisted: PersistedConfig
): Omit<RuntimeConfig, "question"> {
  const model = env.DISTILL_MODEL ?? persisted.model ?? DEFAULT_MODEL;
  const host = normalizeHost(
    env.DISTILL_HOST ?? persisted.host ?? DEFAULT_HOST
  );
  const apiKey = env.DISTILL_API_KEY ?? persisted.apiKey ?? "";
  const timeoutMs = coerceTimeout(
    env.DISTILL_TIMEOUT_MS ?? String(persisted.timeoutMs ?? DEFAULT_TIMEOUT_MS)
  );

  return {
    model,
    host,
    apiKey,
    timeoutMs
  };
}

function parseConfigCommand(argv: string[]): Command {
  if (argv.length === 1) {
    return { kind: "configShow" };
  }

  const key = argv[1] as ConfigKey;

  if (!["model", "host", "api-key", "timeout-ms"].includes(key)) {
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
      timeoutMs
    }
  };
}

export function formatUsage(): string {
  return [
    "Usage:",
    '  cmd 2>&1 | distill "question"',
    '  distill config host http://127.0.0.1:11434/v1',
    '  distill config model "qwen3.5:2b"',
    '  distill --host http://127.0.0.1:1234/v1 --model my-model "summarize"',
    "",
    "Options:",
    `  --model <name>        Model name (default: ${DEFAULT_MODEL})`,
    `  --host <url>          OpenAI-compatible base URL (default: ${DEFAULT_HOST})`,
    "  --api-key <key>       API key (env: DISTILL_API_KEY)",
    `  --timeout-ms <ms>     Request timeout in milliseconds (default: ${DEFAULT_TIMEOUT_MS})`,
    "  --help                Show usage",
    "  --version             Show version"
  ].join("\n");
}
