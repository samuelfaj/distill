import { mkdir, readFile, writeFile } from "node:fs/promises";
import path from "node:path";

import type { ConfigKey, PersistedConfig } from "./config";

function resolveConfigBaseDir(env: NodeJS.ProcessEnv): string {
  const xdg = env.XDG_CONFIG_HOME?.trim();

  if (xdg) {
    return path.join(xdg, "distill");
  }

  const home = env.HOME?.trim();

  if (!home) {
    throw new Error("Could not resolve a home directory for distill config.");
  }

  return path.join(home, ".config", "distill");
}

export function resolveConfigPath(env: NodeJS.ProcessEnv): string {
  const explicit = env.DISTILL_CONFIG_PATH?.trim();

  if (explicit) {
    return explicit;
  }

  return path.join(resolveConfigBaseDir(env), "config.json");
}

export async function readPersistedConfig(
  env: NodeJS.ProcessEnv
): Promise<PersistedConfig> {
  const configPath = resolveConfigPath(env);

  try {
    const raw = await readFile(configPath, "utf8");
    const parsed = JSON.parse(raw) as PersistedConfig;

    if (!parsed || typeof parsed !== "object") {
      return {};
    }

    return parsed;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return {};
    }

    throw error;
  }
}

export async function writePersistedConfig(
  env: NodeJS.ProcessEnv,
  config: PersistedConfig
): Promise<void> {
  const configPath = resolveConfigPath(env);
  await mkdir(path.dirname(configPath), { recursive: true });
  await writeFile(configPath, `${JSON.stringify(config, null, 2)}\n`);
}

export async function setPersistedConfigValue(
  env: NodeJS.ProcessEnv,
  key: ConfigKey,
  value: string | number | boolean
): Promise<PersistedConfig> {
  const current = await readPersistedConfig(env);

  if (key === "provider") {
    current.provider = String(value) as PersistedConfig["provider"];
  } else if (key === "timeout-ms") {
    current.timeoutMs = Number(value);
  } else if (key === "thinking") {
    current.thinking = Boolean(value);
  } else if (key === "host") {
    current.host = String(value);
  } else if (key === "api-key") {
    current.apiKey = String(value);
  } else {
    current.model = String(value);
  }

  await writePersistedConfig(env, current);
  return current;
}

export function getPersistedConfigValue(
  config: PersistedConfig,
  key: ConfigKey
): string | number | boolean | undefined {
  if (key === "provider") {
    return config.provider;
  }

  if (key === "timeout-ms") {
    return config.timeoutMs;
  }

  if (key === "thinking") {
    return config.thinking;
  }

  if (key === "host") {
    return config.host;
  }

  if (key === "api-key") {
    return config.apiKey;
  }

  return config.model;
}
