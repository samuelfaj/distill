import cliPackage from "../packages/cli/package.json";

export const DISTILL_VERSION = cliPackage.version;
export type Provider =
  | "ollama"
  | "openai"
  | "openai-compatible"
  | "lmstudio"
  | "jan"
  | "localai"
  | "vllm"
  | "sglang"
  | "llama.cpp"
  | "mlx-lm"
  | "docker-model-runner";
type ProviderTransport = "ollama" | "openai-compatible";
export const DEFAULT_PROVIDER: Provider = "ollama";
export const DEFAULT_MODEL = "qwen3.5:2b";
export const DEFAULT_HOST = "http://127.0.0.1:11434";
export const DEFAULT_OPENAI_BASE_URL = "https://api.openai.com/v1";
export const DEFAULT_LMSTUDIO_BASE_URL = "http://127.0.0.1:1234/v1";
export const DEFAULT_JAN_BASE_URL = "http://127.0.0.1:1337/v1";
export const DEFAULT_LOCALAI_BASE_URL = "http://127.0.0.1:8080/v1";
export const DEFAULT_VLLM_BASE_URL = "http://127.0.0.1:8000/v1";
export const DEFAULT_DOCKER_MODEL_RUNNER_BASE_URL =
  "http://127.0.0.1:12434/engines/v1";
export const DEFAULT_TIMEOUT_MS = 90_000;
export const DEFAULT_IDLE_MS = 1_200;
export const DEFAULT_INTERACTIVE_GAP_MS = 180;
export const DEFAULT_PROGRESS_FRAME_MS = 120;

interface ProviderSpec {
  apiKeyEnvVars: string[];
  defaultHost?: string;
  displayName: string;
  hostEnvVars: string[];
  requiresApiKey: boolean;
  transport: ProviderTransport;
}

const PROVIDER_SPECS: Record<Provider, ProviderSpec> = {
  ollama: {
    displayName: "Ollama",
    transport: "ollama",
    defaultHost: DEFAULT_HOST,
    hostEnvVars: ["OLLAMA_HOST"],
    apiKeyEnvVars: [],
    requiresApiKey: false
  },
  openai: {
    displayName: "OpenAI",
    transport: "openai-compatible",
    defaultHost: DEFAULT_OPENAI_BASE_URL,
    hostEnvVars: ["OPENAI_BASE_URL", "OPENAI_API_BASE"],
    apiKeyEnvVars: ["OPENAI_API_KEY"],
    requiresApiKey: true
  },
  "openai-compatible": {
    displayName: "OpenAI-compatible provider",
    transport: "openai-compatible",
    hostEnvVars: ["OPENAI_BASE_URL", "OPENAI_API_BASE"],
    apiKeyEnvVars: ["OPENAI_API_KEY"],
    requiresApiKey: false
  },
  lmstudio: {
    displayName: "LM Studio",
    transport: "openai-compatible",
    defaultHost: DEFAULT_LMSTUDIO_BASE_URL,
    hostEnvVars: [],
    apiKeyEnvVars: [],
    requiresApiKey: false
  },
  jan: {
    displayName: "Jan",
    transport: "openai-compatible",
    defaultHost: DEFAULT_JAN_BASE_URL,
    hostEnvVars: [],
    apiKeyEnvVars: [],
    requiresApiKey: true
  },
  localai: {
    displayName: "LocalAI",
    transport: "openai-compatible",
    defaultHost: DEFAULT_LOCALAI_BASE_URL,
    hostEnvVars: [],
    apiKeyEnvVars: [],
    requiresApiKey: false
  },
  vllm: {
    displayName: "vLLM",
    transport: "openai-compatible",
    defaultHost: DEFAULT_VLLM_BASE_URL,
    hostEnvVars: [],
    apiKeyEnvVars: [],
    requiresApiKey: false
  },
  sglang: {
    displayName: "SGLang",
    transport: "openai-compatible",
    hostEnvVars: [],
    apiKeyEnvVars: [],
    requiresApiKey: false
  },
  "llama.cpp": {
    displayName: "llama.cpp",
    transport: "openai-compatible",
    hostEnvVars: [],
    apiKeyEnvVars: [],
    requiresApiKey: false
  },
  "mlx-lm": {
    displayName: "MLX LM",
    transport: "openai-compatible",
    hostEnvVars: [],
    apiKeyEnvVars: [],
    requiresApiKey: false
  },
  "docker-model-runner": {
    displayName: "Docker Model Runner",
    transport: "openai-compatible",
    defaultHost: DEFAULT_DOCKER_MODEL_RUNNER_BASE_URL,
    hostEnvVars: [],
    apiKeyEnvVars: [],
    requiresApiKey: false
  }
};

const PROVIDER_ALIASES: Record<string, Provider> = {
  ollama: "ollama",
  openai: "openai",
  openaicompatible: "openai-compatible",
  lmstudio: "lmstudio",
  jan: "jan",
  localai: "localai",
  vllm: "vllm",
  sglang: "sglang",
  llamacpp: "llama.cpp",
  mlxlm: "mlx-lm",
  dockermodelrunner: "docker-model-runner",
  dmr: "docker-model-runner",
  modelrunner: "docker-model-runner"
};

const SUPPORTED_PROVIDERS = Object.keys(PROVIDER_SPECS).join(", ");

export interface RuntimeConfig {
  question: string;
  provider: Provider;
  model: string;
  host: string;
  apiKey: string;
  timeoutMs: number;
  thinking: boolean;
}

export interface PersistedConfig {
  provider?: Provider;
  model?: string;
  host?: string;
  apiKey?: string;
  timeoutMs?: number;
  thinking?: boolean;
}

export type ConfigKey = "provider" | "model" | "host" | "api-key" | "timeout-ms" | "thinking";

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

function readFirstEnv(env: NodeJS.ProcessEnv, keys: string[]): string | undefined {
  for (const key of keys) {
    const value = env[key]?.trim();

    if (value) {
      return value;
    }
  }

  return undefined;
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

function parseProvider(input: string): Provider {
  const value = input.trim().toLowerCase().replace(/[.\s_-]+/g, "");
  const provider = PROVIDER_ALIASES[value];

  if (provider) {
    return provider;
  }

  throw new UsageError(`Provider must be one of: ${SUPPORTED_PROVIDERS}.`);
}

function getProviderSpec(provider: Provider): ProviderSpec {
  return PROVIDER_SPECS[provider];
}

export function getProviderTransport(provider: Provider): ProviderTransport {
  return getProviderSpec(provider).transport;
}

export function getProviderDisplayName(provider: Provider): string {
  return getProviderSpec(provider).displayName;
}

export function resolveRuntimeDefaults(
  env: NodeJS.ProcessEnv,
  persisted: PersistedConfig
): Omit<RuntimeConfig, "question"> {
  const provider = parseProvider(
    env.DISTILL_PROVIDER ?? persisted.provider ?? DEFAULT_PROVIDER
  );
  const spec = getProviderSpec(provider);
  const model = env.DISTILL_MODEL ?? persisted.model ?? DEFAULT_MODEL;
  const hostInput =
    readFirstEnv(env, ["DISTILL_HOST", ...spec.hostEnvVars]) ??
    persisted.host ??
    spec.defaultHost;

  if (!hostInput) {
    throw new UsageError(
      `A host is required for the ${spec.displayName} provider. Set DISTILL_HOST or use --host.`
    );
  }

  const host = normalizeHost(hostInput);
  const apiKey =
    readFirstEnv(env, ["DISTILL_API_KEY", ...spec.apiKeyEnvVars]) ??
    persisted.apiKey ??
    "";
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
    apiKey,
    timeoutMs,
    thinking
  };
}

function parseConfigCommand(argv: string[]): Command {
  if (argv.length === 1) {
    return { kind: "configShow" };
  }

  const key = argv[1] as ConfigKey;

  if (!["provider", "model", "host", "api-key", "timeout-ms", "thinking"].includes(key)) {
    throw new UsageError(`Unknown config key: ${argv[1]}`);
  }

  if (argv.length === 2) {
    return { kind: "configGet", key };
  }

  const rawValue = argv.slice(2).join(" ").trim();

  if (!rawValue) {
    throw new UsageError(`Missing value for config key ${key}.`);
  }

  if (key === "provider") {
    return {
      kind: "configSet",
      key,
      value: parseProvider(rawValue)
    };
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
  let provider = defaults.provider;
  let model = defaults.model;
  let timeoutMs = defaults.timeoutMs;
  let thinking = defaults.thinking;
  let hostOverride: string | undefined;
  let apiKeyOverride: string | undefined;
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

  const spec = getProviderSpec(provider);
  const hostInput =
    hostOverride ??
    readFirstEnv(env, ["DISTILL_HOST", ...spec.hostEnvVars]) ??
    persisted.host ??
    spec.defaultHost;

  if (!hostInput) {
    throw new UsageError(
      `A host is required for the ${spec.displayName} provider. Set DISTILL_HOST or use --host.`
    );
  }

  const host = normalizeHost(hostInput);
  const apiKey =
    apiKeyOverride ??
    readFirstEnv(env, ["DISTILL_API_KEY", ...spec.apiKeyEnvVars]) ??
    persisted.apiKey ??
    "";

  if (spec.requiresApiKey && !apiKey) {
    throw new UsageError(
      `An API key is required for the ${spec.displayName} provider. Set DISTILL_API_KEY or use --api-key.`
    );
  }

  return {
    kind: "run",
    config: {
      question,
      provider,
      model,
      host: normalizeHost(host),
      apiKey,
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
    '  distill config provider openai',
    '  distill config provider lmstudio',
    '  distill --provider openai-compatible --host http://127.0.0.1:9000/v1 "summarize errors"',
    "",
    "Options:",
    `  --provider <name>     LLM provider: ${SUPPORTED_PROVIDERS} (default: ${DEFAULT_PROVIDER})`,
    `  --model <name>        Model name (default: ${DEFAULT_MODEL})`,
    "  --host <url>          API base URL. Include /v1 (or /engines/v1 for Docker Model Runner).",
    "  --api-key <key>       API key for providers that require auth (env: DISTILL_API_KEY)",
    `  --timeout-ms <ms>     Request timeout in milliseconds (default: ${DEFAULT_TIMEOUT_MS})`,
    "  --thinking <bool>     Enable or disable model thinking (default: false)",
    "  --help                Show usage",
    "  --version             Show version"
  ].join("\n");
}
