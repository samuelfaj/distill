import { describe, expect, it } from "bun:test";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

import {
  readPersistedConfig,
  resolveConfigPath,
  setPersistedConfigValue
} from "../src/user-config";

describe("user config", () => {
  it("writes and reads persisted config values", async () => {
    const dir = await mkdtemp(path.join(tmpdir(), "distill-config-"));
    const configPath = path.join(dir, "config.json");

    try {
      await setPersistedConfigValue(
        { DISTILL_CONFIG_PATH: configPath },
        "model",
        "qwen3.5:2b"
      );
      await setPersistedConfigValue(
        { DISTILL_CONFIG_PATH: configPath },
        "max-tokens",
        2048
      );
      await setPersistedConfigValue(
        { DISTILL_CONFIG_PATH: configPath },
        "dataset-enabled",
        false
      );
      await setPersistedConfigValue(
        { DISTILL_CONFIG_PATH: configPath },
        "dataset-path",
        "/tmp/distill.jsonl"
      );

      expect(await readPersistedConfig({ DISTILL_CONFIG_PATH: configPath })).toEqual({
        model: "qwen3.5:2b",
        maxTokens: 2048,
        datasetEnabled: false,
        datasetPath: "/tmp/distill.jsonl"
      });

      const raw = JSON.parse(await readFile(configPath, "utf8"));
      expect(raw).toEqual({
        model: "qwen3.5:2b",
        maxTokens: 2048,
        datasetEnabled: false,
        datasetPath: "/tmp/distill.jsonl"
      });
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  it("resolves config path from explicit path, xdg, and Windows env vars", () => {
    const appData = "C:\\Users\\me\\AppData\\Roaming";
    const localAppData = "C:\\Users\\me\\AppData\\Local";
    const userProfile = "C:\\Users\\me";

    expect(
      resolveConfigPath({
        DISTILL_CONFIG_PATH: "/tmp/custom-distill.json"
      })
    ).toBe("/tmp/custom-distill.json");

    expect(
      resolveConfigPath({
        XDG_CONFIG_HOME: "/tmp/xdg"
      })
    ).toBe(path.join("/tmp/xdg", "distill", "config.json"));

    expect(
      resolveConfigPath({
        APPDATA: appData
      })
    ).toBe(path.join(appData, "distill", "config.json"));

    expect(
      resolveConfigPath({
        APPDATA: appData,
        LOCALAPPDATA: localAppData
      })
    ).toBe(path.join(appData, "distill", "config.json"));

    expect(
      resolveConfigPath({
        LOCALAPPDATA: localAppData
      })
    ).toBe(path.join(localAppData, "distill", "config.json"));

    expect(
      resolveConfigPath({
        USERPROFILE: userProfile
      })
    ).toBe(
      path.join(userProfile, "AppData", "Roaming", "distill", "config.json")
    );
  });
});
