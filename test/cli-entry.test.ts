import { describe, expect, it } from "bun:test";
import {
  chmod,
  copyFile,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  writeFile
} from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import path from "node:path";
import cliPackage from "../packages/cli/package.json";
import { createScriptCommand } from "./script-command";

const root = path.resolve(import.meta.dir, "..");
const cli = path.join(root, "src", "cli.ts");
const bunExecutable = process.execPath;
const itUnixOnly = process.platform === "win32" ? it.skip : it;

describe("cli entrypoint", () => {
  it("prints help", () => {
    const result = spawnSync(bunExecutable, ["run", cli, "--help"], {
      cwd: root,
      encoding: "utf8"
    });

    expect(result.status).toBe(0);
    expect(result.stdout).toContain('cmd 2>&1 | distill "question"');
  });

  it("prints the version", () => {
    const result = spawnSync(bunExecutable, ["run", cli, "--version"], {
      cwd: root,
      encoding: "utf8"
    });

    expect(result.status).toBe(0);
    expect(result.stdout.trim()).toBe(cliPackage.version);
  });

  it("fails on unsupported platforms", () => {
    const launcher = JSON.stringify(path.join(root, "packages", "cli", "bin", "distill.js"));
    const result = spawnSync(
      "node",
      [
        "-e",
        `Object.defineProperty(process, "platform", { value: "haiku" }); Object.defineProperty(process, "arch", { value: "x64" }); require(${launcher});`
      ],
      {
        cwd: root,
        encoding: "utf8"
      }
    );

    expect(result.status).toBe(1);
    expect(result.stderr).toContain("[distill] Unsupported platform: haiku/x64.");
  });

  itUnixOnly("fails without stdin when attached to a tty", () => {
    const scriptCommand = createScriptCommand("/dev/null", "bun", [
      "run",
      cli,
      "is this safe?"
    ]);
    const result = spawnSync(scriptCommand.command, scriptCommand.args, {
      cwd: root,
      encoding: "utf8"
    });

    expect(result.status).toBe(2);
    expect(`${result.stdout}${result.stderr}`).toContain("stdin is required.");
  });

  it("persists config commands", async () => {
    const dir = await mkdtemp(path.join(tmpdir(), "distill-cli-config-"));
    const configPath = path.join(dir, "config.json");

    try {
      const setModel = spawnSync(
        bunExecutable,
        ["run", cli, "config", "model", "qwen3.5:2b"],
        {
          cwd: root,
          encoding: "utf8",
          env: {
            ...process.env,
            DISTILL_CONFIG_PATH: configPath
          }
        }
      );

      const setDatasetEnabled = spawnSync(
        bunExecutable,
        ["run", cli, "config", "dataset-enabled", "false"],
        {
          cwd: root,
          encoding: "utf8",
          env: {
            ...process.env,
            DISTILL_CONFIG_PATH: configPath
          }
        }
      );

      const setMaxTokens = spawnSync(
        bunExecutable,
        ["run", cli, "config", "max-tokens", "2048"],
        {
          cwd: root,
          encoding: "utf8",
          env: {
            ...process.env,
            DISTILL_CONFIG_PATH: configPath
          }
        }
      );

      const showConfig = spawnSync(bunExecutable, ["run", cli, "config"], {
        cwd: root,
        encoding: "utf8",
        env: {
          ...process.env,
          DISTILL_CONFIG_PATH: configPath
        }
      });

      expect(setModel.status).toBe(0);
      expect(setDatasetEnabled.status).toBe(0);
      expect(setMaxTokens.status).toBe(0);
      expect(showConfig.status).toBe(0);
      expect(JSON.parse(await readFile(configPath, "utf8"))).toEqual({
        model: "qwen3.5:2b",
        maxTokens: 2048,
        datasetEnabled: false
      });
      expect(showConfig.stdout).toContain("max-tokens=2048");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  it("runs onboarding with config and skill install defaults", async () => {
    const dir = await mkdtemp(path.join(tmpdir(), "distill-onboarding-"));
    const home = path.join(dir, "home");
    const configPath = path.join(dir, "config.json");
    const oldBlock = [
      "keep before",
      "<!-- distill skill: begin -->",
      "old distill instructions",
      "<!-- distill skill: end -->",
      "keep after"
    ].join("\n");

    try {
      await mkdir(path.join(home, ".codex"), { recursive: true });
      await mkdir(path.join(home, ".claude"), { recursive: true });
      await writeFile(path.join(home, ".codex", "AGENTS.md"), oldBlock);
      await writeFile(path.join(home, ".claude", "CLAUDE.md"), oldBlock);

      const result = spawnSync(bunExecutable, ["run", cli], {
        cwd: root,
        encoding: "utf8",
        input: [
          "http://127.0.0.1:1234/v1",
          "local-model",
          "",
          "120000",
          "2048",
          ""
        ].join("\n"),
        env: {
          ...process.env,
          HOME: home,
          USERPROFILE: home,
          DISTILL_CONFIG_PATH: configPath,
          DISTILL_PACKAGE_ROOT: root
        }
      });

      expect(result.status).toBe(0);
      expect(result.stdout).toContain("distill onboarding");
      expect(result.stdout).toContain("/distill skill installed for Codex and Claude");
      expect(JSON.parse(await readFile(configPath, "utf8"))).toEqual({
        host: "http://127.0.0.1:1234/v1",
        model: "local-model",
        timeoutMs: 120000,
        maxTokens: 2048
      });
      expect(
        await readFile(
          path.join(home, ".codex", "skills", "distill", "SKILL.md"),
          "utf8"
        )
      ).toContain("name: distill");
      expect(
        await readFile(
          path.join(home, ".claude", "skills", "distill", "SKILL.md"),
          "utf8"
        )
      ).toContain("name: distill");
      const codexInstructions = await readFile(
        path.join(home, ".codex", "AGENTS.md"),
        "utf8"
      );
      const claudeInstructions = await readFile(
        path.join(home, ".claude", "CLAUDE.md"),
        "utf8"
      );

      for (const instructions of [codexInstructions, claudeInstructions]) {
        expect(instructions).toContain("keep before");
        expect(instructions).toContain("keep after");
        expect(instructions).not.toContain("old distill instructions");
        expect(
          instructions.match(/<!-- distill skill: begin -->/g) ?? []
        ).toHaveLength(1);
        expect(instructions).toContain("Always communicate with the user in `/distill`");
        expect(instructions).toContain(
          "Do not return a rewritten/compressed copy of the user's prompt"
        );
        expect(instructions).toContain("Keep hidden chain-of-thought private");
        expect(instructions).toContain(
          "When running non-interactive shell commands, pipe output through `distill`"
        );
        expect(instructions).toContain(
          'bun test 2>&1 | distill "Did tests pass? Return PASS or FAIL'
        );
        expect(instructions).toContain(
          'git diff | distill "What changed? Return only files changed'
        );
        expect(instructions).toContain(
          'terraform plan 2>&1 | distill "Is this safe? Return SAFE, REVIEW, or UNSAFE'
        );
      }
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  itUnixOnly("falls back to the workspace binary when the platform package is not installed", async () => {
    const dir = await mkdtemp(path.join(tmpdir(), "distill-workspace-fallback-"));
    const fakeTargetDir = path.join(
      dir,
      "packages",
      `distill-${process.platform}-${process.arch}`,
      "bin"
    );
    const launcherPath = path.join(dir, "packages", "cli", "bin", "distill.js");
    const fakeBinaryPath = path.join(fakeTargetDir, "distill");

    try {
      await mkdir(path.dirname(launcherPath), { recursive: true });
      await mkdir(fakeTargetDir, { recursive: true });
      await copyFile(path.join(root, "packages", "cli", "bin", "distill.js"), launcherPath);
      await writeFile(
        fakeBinaryPath,
        "#!/bin/sh\nprintf 'workspace fallback\\n'\n"
      );
      await chmod(fakeBinaryPath, 0o755);

      const result = spawnSync("node", [launcherPath, "--version"], {
        cwd: dir,
        encoding: "utf8"
      });

      expect(result.status).toBe(0);
      expect(result.stdout).toBe("workspace fallback\n");
      expect(result.stderr).toBe("");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});
