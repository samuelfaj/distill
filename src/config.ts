import cliPackage from "../packages/cli/package.json";

export const DISTILL_VERSION = cliPackage.version;
export const DEFAULT_MODEL = "qwen3.5:2b";
export const DEFAULT_HOST = "http://127.0.0.1:11434";
export const DEFAULT_TIMEOUT_MS = 90_000;
export const DEFAULT_IDLE_MS = 1_200;
export const DEFAULT_INTERACTIVE_GAP_MS = 180;
export const DEFAULT_PROGRESS_FRAME_MS = 120;

export interface RuntimeConfig {
  question: string;
  model: string;
  host: string;
  timeoutMs: number;
  thinking: boolean;
}

export interface PersistedConfig {
  model?: string;
  host?: string;
  timeoutMs?: number;
  thinking?: boolean;
}

export type ConfigKey = "model" | "host" | "timeout-ms" | "thinking";

export type Command =
  | { kind: "help" }
  | { kind: "version" }
  | { kind: "configShow" }
  | { kind: "configGet"; key: ConfigKey }
  | { kind: "configSet"; key: ConfigKey; value: string | number | boolean }
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

function parseBoolean(input: string, name: string): boolean {
  const value = input.trim().toLowerCase();

  switch (value) {
    case "true":
    case "1":
    case "yes":
    case "on":
      return true;
    case "false":
    case "0":
    case "no":
    case "off":
      return false;
    default:
      throw new UsageError(`${name} must be true or false.`);
  }
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
  const host = normalizeHost(env.OLLAMA_HOST ?? persisted.host ?? DEFAULT_HOST);
  const timeoutMs = coerceTimeout(
    env.DISTILL_TIMEOUT_MS ?? String(persisted.timeoutMs ?? DEFAULT_TIMEOUT_MS)
  );
  const thinking = parseBoolean(
    env.DISTILL_THINKING ?? String(persisted.thinking ?? false),
    "Thinking"
  );

  return {
    model,
    host,
    timeoutMs,
    thinking
  };
}

function parseConfigCommand(argv: string[]): Command {
  if (argv.length === 1) {
    return { kind: "configShow" };
  }

  const key = argv[1] as ConfigKey;

  if (!["model", "host", "timeout-ms", "thinking"].includes(key)) {
    throw new UsageError(`Unknown config key: ${argv[1]}`);
  }

  if (argv.length === 2) {
    return { kind: "configGet", key };
  }

  const rawValue = argv.slice(2).join(" ").trim();

  if (!rawValue) {
    throw new UsageError(`Missing value for config key ${key}.`);
  }

  if (key === "thinking") {
    return {
      kind: "configSet",
      key,
      value: parseBoolean(rawValue, "Thinking")
    };
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
  let model = defaults.model;
  let host = defaults.host;
  let timeoutMs = defaults.timeoutMs;
  let thinking = defaults.thinking;
  const questionParts: string[] = [];

  for (let index = 0; index < argv.length; index += 1) {
    const token = argv[index];

    if (token === "--") {
      questionParts.push(...argv.slice(index + 1));
      break;
    }

    if (token === "--model" || token.startsWith("--model=")) {
      const parsed = readFlagValue(argv, index, "--model");
      model = parsed.value;
      index = parsed.nextIndex;
      continue;
    }

    if (token === "--host" || token.startsWith("--host=")) {
      const parsed = readFlagValue(argv, index, "--host");
      host = parsed.value;
      index = parsed.nextIndex;
      continue;
    }

    if (token === "--timeout-ms" || token.startsWith("--timeout-ms=")) {
      const parsed = readFlagValue(argv, index, "--timeout-ms");
      timeoutMs = coerceTimeout(parsed.value);
      index = parsed.nextIndex;
      continue;
    }

    if (token === "--thinking" || token.startsWith("--thinking=")) {
      const parsed = readFlagValue(argv, index, "--thinking");
      thinking = parseBoolean(parsed.value, "Thinking");
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

  return {
    kind: "run",
    config: {
      question,
      model,
      host: normalizeHost(host),
      timeoutMs,
      thinking
    }
  };
}

export function formatUsage(): string {
  return [
    "Usage:",
    '  cmd 2>&1 | distill "question"',
    '  distill config model "qwen3.5:2b"',
    "  distill config thinking false",
    "",
    "Options:",
    `  --model <name>        Ollama model (default: ${DEFAULT_MODEL})`,
    `  --host <url>          Ollama host (default: ${DEFAULT_HOST})`,
    `  --timeout-ms <ms>     Request timeout in milliseconds (default: ${DEFAULT_TIMEOUT_MS})`,
    "  --thinking <bool>     Enable or disable model thinking (default: false)",
    "  --help                Show usage",
    "  --version             Show version"
  ].join("\n");
}
