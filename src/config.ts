import cliPackage from "../packages/cli/package.json";

export const DISTILL_VERSION = cliPackage.version;
export const DEFAULT_OLLAMA_MODEL = "qwen3.5:2b";
export const DEFAULT_BITNET_APPLE_MODEL =
  "mlx-community/bitnet-b1.58-2B-4T";
export const DEFAULT_BITNET_LINUX_MODEL = "microsoft/bitnet-b1.58-2B-4T";
export const DEFAULT_HOST = "http://127.0.0.1:11434";
export const DEFAULT_TIMEOUT_MS = 90_000;
export const DEFAULT_IDLE_MS = 1_200;
export const DEFAULT_INTERACTIVE_GAP_MS = 180;
export const DEFAULT_PROGRESS_FRAME_MS = 120;

export type Provider = "ollama" | "bitnet";

export interface RuntimeConfig {
  provider: Provider;
  question: string;
  model: string;
  host: string;
  timeoutMs: number;
  thinking: boolean;
}

export interface PersistedConfig {
  provider?: Provider;
  model?: string;
  host?: string;
  timeoutMs?: number;
  thinking?: boolean;
}

export type ConfigKey = "provider" | "model" | "host" | "timeout-ms" | "thinking";

export type Command =
  | { kind: "help" }
  | { kind: "version" }
  | { kind: "daemon"; config: Omit<RuntimeConfig, "question"> }
  | { kind: "configShow" }
  | { kind: "configGet"; key: ConfigKey }
  | { kind: "configSet"; key: ConfigKey; value: string | number | boolean }
  | { kind: "test"; config: Omit<RuntimeConfig, "question"> }
  | { kind: "run"; config: RuntimeConfig };

export class UsageError extends Error {
  readonly exitCode = 2;

  constructor(message: string) {
    super(message);
    this.name = "UsageError";
  }
}

export function getDefaultProvider(
  platform = process.platform,
  arch = process.arch
): Provider {
  if (platform === "darwin" && arch === "arm64") {
    return "bitnet";
  }

  if (platform === "linux") {
    return "bitnet";
  }

  return "ollama";
}

export const DEFAULT_PROVIDER = getDefaultProvider();

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

export function parseProvider(input: string | undefined): Provider {
  const value = (input ?? DEFAULT_PROVIDER).trim().toLowerCase();

  if (value === "ollama" || value === "bitnet") {
    return value;
  }

  throw new UsageError(`Provider must be one of: ollama, bitnet.`);
}

export function getDefaultModel(
  provider: Provider,
  platform = process.platform,
  arch = process.arch
): string {
  if (provider === "ollama") {
    return DEFAULT_OLLAMA_MODEL;
  }

  if (platform === "darwin" && arch === "arm64") {
    return DEFAULT_BITNET_APPLE_MODEL;
  }

  if (platform === "linux") {
    return DEFAULT_BITNET_LINUX_MODEL;
  }

  return DEFAULT_BITNET_LINUX_MODEL;
}

export function resolveRuntimeDefaults(
  env: NodeJS.ProcessEnv,
  persisted: PersistedConfig,
  platform = process.platform,
  arch = process.arch
): Omit<RuntimeConfig, "question"> {
  const provider = parseProvider(
    env.DISTILL_PROVIDER ?? persisted.provider ?? getDefaultProvider(platform, arch)
  );
  const model = env.DISTILL_MODEL ?? persisted.model ?? getDefaultModel(provider, platform, arch);
  const rawHost = env.OLLAMA_HOST ?? persisted.host ?? DEFAULT_HOST;
  const host =
    provider === "ollama"
      ? normalizeHost(rawHost)
      : rawHost.trim() || DEFAULT_HOST;
  const timeoutMs = coerceTimeout(
    env.DISTILL_TIMEOUT_MS ?? String(persisted.timeoutMs ?? DEFAULT_TIMEOUT_MS)
  );
  const thinking = parseBoolean(
    env.DISTILL_THINKING ?? String(persisted.thinking ?? false),
    "Thinking"
  );

  return {
    provider,
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

  if (!["provider", "model", "host", "timeout-ms", "thinking"].includes(key)) {
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

  if (key === "provider") {
    return {
      kind: "configSet",
      key,
      value: parseProvider(rawValue)
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

function parseRuntimeCommand(
  argv: string[],
  env: NodeJS.ProcessEnv,
  persisted: PersistedConfig,
  kind: "run" | "test"
): Command {
  const defaults = resolveRuntimeDefaults(env, persisted);
  let provider = defaults.provider;
  let model = defaults.model;
  let host = defaults.host;
  let timeoutMs = defaults.timeoutMs;
  let thinking = defaults.thinking;
  let sawHostFlag = false;
  const questionParts: string[] = [];

  for (let index = 0; index < argv.length; index += 1) {
    const token = argv[index];

    if (token === "--") {
      questionParts.push(...argv.slice(index + 1));
      break;
    }

    if (token === "--provider" || token.startsWith("--provider=")) {
      const parsed = readFlagValue(argv, index, "--provider");
      provider = parseProvider(parsed.value);
      if (env.DISTILL_MODEL === undefined && persisted.model === undefined) {
        model = getDefaultModel(provider);
      }
      index = parsed.nextIndex;
      continue;
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
      sawHostFlag = true;
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

  if (provider === "bitnet" && sawHostFlag) {
    throw new UsageError("--host is only supported with provider=ollama.");
  }

  if (kind === "test" && questionParts.length > 0) {
    throw new UsageError("distill test does not accept positional arguments.");
  }

  const config = {
    provider,
    model,
    host: provider === "ollama" ? normalizeHost(host) : host.trim() || DEFAULT_HOST,
    timeoutMs,
    thinking
  };

  if (kind === "test") {
    return {
      kind,
      config
    };
  }

  const question = questionParts.join(" ").trim();

  if (!question) {
    throw new UsageError("A question is required.");
  }

  return {
    kind,
    config: {
      ...config,
      question
    }
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

  if (argv[0] === "test") {
    return parseRuntimeCommand(argv.slice(1), env, persisted, "test");
  }

  if (argv[0] === "daemon") {
    const command = parseRuntimeCommand(argv.slice(1), env, persisted, "test");
    return {
      kind: "daemon",
      config: command.config
    };
  }

  if (argv.length === 1 && (argv[0] === "--help" || argv[0] === "-h")) {
    return { kind: "help" };
  }

  if (argv.length === 1 && (argv[0] === "--version" || argv[0] === "-v")) {
    return { kind: "version" };
  }

  return parseRuntimeCommand(argv, env, persisted, "run");
}

export function formatUsage(): string {
  return [
    "Usage:",
    '  cmd 2>&1 | distill "question"',
    "  distill test",
    "  distill daemon",
    '  distill config provider "bitnet"',
    '  distill config model "qwen3.5:2b"',
    "  distill config thinking false",
    "",
    "Options:",
    `  --provider <name>     Provider: ollama or bitnet (default: ${DEFAULT_PROVIDER})`,
    "  --model <name>        Model name or repo id for the selected provider",
    `  --host <url>          Ollama host (default: ${DEFAULT_HOST})`,
    `  --timeout-ms <ms>     Request timeout in milliseconds (default: ${DEFAULT_TIMEOUT_MS})`,
    "  --thinking <bool>     Enable or disable model thinking (default: false)",
    "  --help                Show usage",
    "  --version             Show version"
  ].join("\n");
}
