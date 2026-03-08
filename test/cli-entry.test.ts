import { describe, expect, it } from "bun:test";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import path from "node:path";
import cliPackage from "../packages/cli/package.json";

const root = path.resolve(import.meta.dir, "..");
const cli = path.join(root, "src", "cli.ts");
const bunExe = process.execPath;
const isWindows = process.platform === "win32";

describe("cli entrypoint", () => {
  it("prints help", () => {
    const result = spawnSync(bunExe, ["run", cli, "--help"], {
      cwd: root,
      encoding: "utf8"
    });

    if (result.error) {
      throw result.error;
    }

    expect(result.status).toBe(0);
    expect(result.stdout).toContain('cmd 2>&1 | distill "question"');
  });

  it("prints the version", () => {
    const result = spawnSync(bunExe, ["run", cli, "--version"], {
      cwd: root,
      encoding: "utf8"
    });

    if (result.error) {
      throw result.error;
    }

    expect(result.status).toBe(0);
    expect(result.stdout.trim()).toBe(cliPackage.version);
  });

  it("fails without stdin when attached to a tty", () => {
    if (isWindows) {
      return;
    }

    const result = spawnSync(
      "script",
      ["-q", "/dev/null", bunExe, "run", cli, "is this safe?"],
      {
        cwd: root,
        encoding: "utf8"
      }
    );

    expect(result.status).toBe(2);
    expect(result.stdout).toContain("stdin is required.");
  });

  it("persists config commands", async () => {
    const dir = await mkdtemp(path.join(tmpdir(), "distill-cli-config-"));
    const configPath = path.join(dir, "config.json");

    try {
      const setModel = spawnSync(
        bunExe,
        ["run", cli, "config", "model", "phi3:mini"],
        {
          cwd: root,
          encoding: "utf8",
          env: {
            ...process.env,
            DISTILL_CONFIG_PATH: configPath
          }
        }
      );

      const setThinking = spawnSync(
        bunExe,
        ["run", cli, "config", "thinking", "false"],
        {
          cwd: root,
          encoding: "utf8",
          env: {
            ...process.env,
            DISTILL_CONFIG_PATH: configPath
          }
        }
      );

      if (setModel.error) {
        throw setModel.error;
      }
      if (setThinking.error) {
        throw setThinking.error;
      }

      expect(setModel.status).toBe(0);
      expect(setThinking.status).toBe(0);
      expect(JSON.parse(await readFile(configPath, "utf8"))).toEqual({
        model: "phi3:mini",
        thinking: false
      });
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});
