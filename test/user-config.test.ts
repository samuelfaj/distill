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
        "phi3:mini"
      );
      await setPersistedConfigValue(
        { DISTILL_CONFIG_PATH: configPath },
        "thinking",
        false
      );

      expect(await readPersistedConfig({ DISTILL_CONFIG_PATH: configPath })).toEqual({
        model: "phi3:mini",
        thinking: false
      });

      const raw = JSON.parse(await readFile(configPath, "utf8"));
      expect(raw).toEqual({
        model: "phi3:mini",
        thinking: false
      });
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  it("resolves config path from xdg or explicit path", () => {
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
  });
});
