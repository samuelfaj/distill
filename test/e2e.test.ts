import {
  beforeAll,
  describe,
  expect,
  it,
  setDefaultTimeout
} from "bun:test";
import { readdirSync } from "node:fs";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { spawn, spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import path from "node:path";
import cliPackage from "../packages/cli/package.json";
import { DEFAULT_MODEL } from "../src/config";
import {
  getCurrentPlatformKey,
  getPlatformTarget
} from "../scripts/platform-targets";
import { createScriptCommand } from "./script-command";

setDefaultTimeout(process.platform === "win32" ? 120_000 : 60_000);

const root = path.resolve(import.meta.dir, "..");
const launcher = path.join(root, "packages", "cli", "bin", "distill.js");
const expectedVersion = cliPackage.version;
const WATCH_IDLE_MS = 1_800;
const WATCH_START_DELAY_MS = 600;
const INTERACTIVE_DELAY_MS = 1_000;
const itPty = process.platform === "linux" ? it : it.skip;
const npmCommand = process.platform === "win32" ? "npm.cmd" : "npm";
const isolatedConfigPath = path.join(
  tmpdir(),
  `distill-e2e-default-config-${process.pid}.json`
);
const currentPlatformPackage = (() => {
  const value = getPlatformTarget(getCurrentPlatformKey())?.packageName;

  if (!value) {
    throw new Error(`Unsupported platform for e2e tests: ${getCurrentPlatformKey()}`);
  }

  return value;
})();

function createProviderEnv(host: string, env?: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
  return {
    ...env,
    DISTILL_HOST: host
  };
}

function createChildEnv(env?: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
  return {
    ...process.env,
    DISTILL_CONFIG_PATH: isolatedConfigPath,
    DISTILL_MODEL: undefined,
    DISTILL_HOST: undefined,
    DISTILL_API_KEY: undefined,
    DISTILL_TIMEOUT_MS: undefined,
    ...env
  };
}

function resolveInstalledShimPath(installDir: string): string {
  return path.join(
    installDir,
    "node_modules",
    ".bin",
    process.platform === "win32" ? "distill.cmd" : "distill"
  );
}

function resolveInstalledBinaryPath(installDir: string): string {
  return path.join(
    installDir,
    "node_modules",
    ...currentPlatformPackage.split("/"),
    "bin",
    process.platform === "win32" ? "distill.exe" : "distill"
  );
}

interface InputStep {
  afterMs?: number;
  data: string;
}

interface RunResult {
  code: number | null;
  signal: NodeJS.Signals | null;
  stdout: string;
  stderr: string;
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function resolveUnixShell(): string {
  const candidates = [process.env.SHELL, "zsh", "bash", "sh"].filter(
    (value): value is string => Boolean(value)
  );
  const uniqueCandidates = [...new Set(candidates)];

  for (const candidate of uniqueCandidates) {
    const result = spawnSync(candidate, ["-lc", "exit 0"], {
      stdio: "ignore"
    });

    if (!result.error && result.status === 0) {
      return candidate;
    }
  }

  throw new Error("Unable to find a usable Unix shell for the pty test.");
}

function runOrThrow(
  command: string,
  args: string[],
  cwd: string,
  env?: NodeJS.ProcessEnv,
  input?: string
): string {
  const result = spawnSync(command, args, {
    cwd,
    env: createChildEnv(env),
    encoding: "utf8",
    input
  });

  if (result.error) {
    throw result.error;
  }

  if (result.status !== 0) {
    throw new Error(
      [result.stdout, result.stderr].filter(Boolean).join("\n") || `${command} failed`
    );
  }

  return result.stdout.trim();
}

async function runProcess(
  command: string,
  args: string[],
  options: {
    cwd: string;
    env?: NodeJS.ProcessEnv;
    inputSteps?: InputStep[];
    finalDelayMs?: number;
  }
): Promise<RunResult> {
  const child = spawn(command, args, {
    cwd: options.cwd,
    env: createChildEnv(options.env),
    stdio: ["pipe", "pipe", "pipe"]
  });

  let stdout = "";
  let stderr = "";

  child.stdout.on("data", (chunk) => {
    stdout += chunk.toString();
  });

  child.stderr.on("data", (chunk) => {
    stderr += chunk.toString();
  });

  const writer = (async () => {
    for (const step of options.inputSteps ?? []) {
      await delay(step.afterMs ?? 0);

      if (!child.stdin.destroyed && !child.killed) {
        child.stdin.write(step.data);
      }
    }

    await delay(options.finalDelayMs ?? 0);

    if (!child.stdin.destroyed && !child.killed) {
      child.stdin.end();
    }
  })();

  const exit = new Promise<RunResult>((resolve, reject) => {
    child.on("error", reject);
    child.on("close", (code, signal) => {
      resolve({ code, signal, stdout, stderr });
    });
  });

  await writer;
  return exit;
}

async function runLauncher(
  args: string[],
  options?: {
    env?: NodeJS.ProcessEnv;
    inputSteps?: InputStep[];
    finalDelayMs?: number;
  }
): Promise<RunResult> {
  return runProcess("node", [launcher, ...args], {
    cwd: root,
    env: options?.env,
    inputSteps: options?.inputSteps,
    finalDelayMs: options?.finalDelayMs
  });
}

async function createFakeChatProvider(
  responder: (
    body: Record<string, unknown>,
    index: number
  ) => Response | Promise<Response>
): Promise<{
  host: string;
  requests: Array<Record<string, unknown>>;
  stop: () => void;
}> {
  const requests: Array<Record<string, unknown>> = [];
  const server = Bun.serve({
    port: 0,
    async fetch(request) {
      if (new URL(request.url).pathname !== "/v1/chat/completions") {
        return new Response("not found", { status: 404 });
      }

      const payload = (await request.json()) as Record<string, unknown>;
      requests.push(payload);
      return responder(payload, requests.length - 1);
    }
  });

  return {
    host: `http://127.0.0.1:${server.port}/v1`,
    requests,
    stop() {
      server.stop(true);
    }
  };
}

function normalizePtyOutput(output: string): string {
  return output
    .replace(/\[[0-9;]*[A-Za-z]/g, "")
    .replace(/\r/g, "\n")
    .replace(/[]/g, "")
    .split("\n")
    .map((line) => line.trimEnd())
    .filter(Boolean)
    .join("\n");
}

beforeAll(() => {
  runOrThrow(npmCommand, ["run", "build"], root);
});

describe("distill end-to-end", () => {
  it("summarizes batch output through the launcher", async () => {
    const fake = await createFakeChatProvider((_body, _index) =>
      new Response(
        JSON.stringify({
          choices: [{ message: { content: "All tests passed." } }]
        }),
        { status: 200 }
      )
    );

    try {
      const result = await runLauncher(["did the tests pass?"], {
        env: createProviderEnv(fake.host),
        inputSteps: [{ data: "Ran 12 tests\n12 passed\n" }]
      });

      expect(result.code).toBe(0);
      expect(result.stderr).toBe("");
      expect(result.stdout).toBe("All tests passed.\n");
      expect(fake.requests).toHaveLength(1);
      expect(fake.requests[0]).toMatchObject({
        model: DEFAULT_MODEL
      });
    } finally {
      fake.stop();
    }
  });

  itPty("keeps the spinner moving in a pty while collecting streamed input and summarizing", async () => {
    const fake = await createFakeChatProvider(async (_body, _index) => {
      await delay(700);
      return new Response(
        JSON.stringify({
          choices: [{ message: { content: "All tests passed." } }]
        }),
        { status: 200 }
      );
    });
    const dir = await mkdtemp(path.join(tmpdir(), "distill-e2e-pty-"));
    const capturePath = path.join(dir, "terminal.log");
    const shellCommand =
      "perl -e '$|=1; for (1..8) { print qq(Ran chunk $_\\n); select undef,undef,undef,0.18; }' | " +
      `node ${launcher} 'did the tests pass?'`;
    const scriptCommand = createScriptCommand(capturePath, resolveUnixShell(), [
      "-lc",
      shellCommand
    ]);

    try {
      runOrThrow(
        scriptCommand.command,
        scriptCommand.args,
        root,
        createProviderEnv(fake.host)
      );

      const output = normalizePtyOutput(await readFile(capturePath, "utf8"));
      const lines = output.split("\n");
      const waitingFrames = output.match(/distill: waiting/g) ?? [];

      expect(output).toContain("distill: waiting");
      expect(output).toContain("distill: summarizing");
      expect(waitingFrames.length).toBeGreaterThan(1);
      expect(lines[lines.length - 1]).toBe("All tests passed.");
      expect(fake.requests).toHaveLength(1);
    } finally {
      fake.stop();
      await rm(dir, { recursive: true, force: true });
    }
  });

  it("falls back to the raw input when the provider is unavailable", async () => {
    const result = await runLauncher(["summarize briefly"], {
      env: {
        DISTILL_HOST: "http://127.0.0.1:9/v1",
        DISTILL_TIMEOUT_MS: "150"
      },
      inputSteps: [{ data: "fallback payload\n" }]
    });

    expect(result.code).toBe(0);
    expect(result.stderr).toBe("");
    expect(result.stdout).toBe("fallback payload\n");
  });

  it("detects watch-like recurring output and emits watch summaries", async () => {
    const fake = await createFakeChatProvider((body) => {
      const messages = (body.messages ?? []) as Array<{ content?: string }>;
      const prompt = messages.map((m) => String(m?.content ?? "")).join("\n");

      if (!prompt.includes("Previous cycle:") || !prompt.includes("Current cycle:")) {
        return new Response(
          JSON.stringify({
            choices: [{ message: { content: "unexpected prompt" } }]
          }),
          { status: 200 }
        );
      }

      return new Response(
        JSON.stringify({
          choices: [
            { message: { content: "Failure count changed from 0 to 1." } }
          ]
        }),
        { status: 200 }
      );
    });

    try {
      const result = await runLauncher(["what changed?"], {
        env: createProviderEnv(fake.host),
        inputSteps: [
          { afterMs: WATCH_START_DELAY_MS, data: "watch run\r\nfailures: 0\n" },
          { afterMs: WATCH_IDLE_MS, data: "watch run\nfailures: 1\n" }
        ],
        finalDelayMs: WATCH_IDLE_MS
      });

      expect(result.code).toBe(0);
      expect(result.stderr).toBe("");
      expect(result.stdout).toBe("Failure count changed from 0 to 1.\n");
      expect(fake.requests).toHaveLength(1);
      const messages = (fake.requests[0].messages ?? []) as Array<{
        content?: string;
      }>;
      const allContent = messages.map((m) => String(m?.content ?? "")).join("\n");
      expect(allContent).toContain("Previous cycle:");
    } finally {
      fake.stop();
    }
  });

  it("passes through simple interactive prompts without calling the provider", async () => {
    const fake = await createFakeChatProvider((_body, _index) =>
      new Response(
        JSON.stringify({
          choices: [{ message: { content: "should not happen" } }]
        }),
        { status: 200 }
      )
    );

    try {
      const result = await runLauncher(["confirm the action"], {
        env: createProviderEnv(fake.host),
        inputSteps: [
          { data: "Continue? [y/N]" },
          { afterMs: INTERACTIVE_DELAY_MS, data: "\ny\n" }
        ]
      });

      expect(result.code).toBe(0);
      expect(result.stderr).toBe("");
      expect(result.stdout).toBe("Continue? [y/N]\ny\n");
      expect(fake.requests).toHaveLength(0);
    } finally {
      fake.stop();
    }
  });

  it("works after packing and installing the npm package locally", async () => {
    const fake = await createFakeChatProvider((_body, _index) =>
      new Response(
        JSON.stringify({
          choices: [{ message: { content: "Tests passed." } }]
        }),
        { status: 200 }
      )
    );

    const packDir = await mkdtemp(path.join(tmpdir(), "distill-e2e-pack-"));
    const installDir = await mkdtemp(path.join(tmpdir(), "distill-e2e-install-"));

    try {
      runOrThrow(
        npmCommand,
        ["pack", "--workspace", currentPlatformPackage, "--pack-destination", packDir],
        root
      );
      runOrThrow(
        npmCommand,
        ["pack", "--workspace", "@samuelfaj/distill", "--pack-destination", packDir],
        root
      );
      runOrThrow(npmCommand, ["init", "-y"], installDir);

      const tarballs = readdirSync(packDir)
        .sort()
        .map((entry) => path.join(packDir, entry));
      runOrThrow(npmCommand, ["install", ...tarballs], installDir);

      const installedShim = resolveInstalledShimPath(installDir);
      const version = await runProcess(installedShim, ["--version"], {
        cwd: installDir
      });
      expect(version.code).toBe(0);
      expect(version.stderr).toBe("");
      expect(version.stdout.trim()).toBe(expectedVersion);

      const installedBinary = resolveInstalledBinaryPath(installDir);
      const summary = await runProcess(installedBinary, ["did the tests pass?"], {
        cwd: installDir,
        env: createProviderEnv(fake.host),
        inputSteps: [{ data: "12 passed\n" }]
      });

      expect(summary.code).toBe(0);
      expect(summary.stderr).toBe("");
      expect(summary.stdout.trim()).toBe("Tests passed.");
      expect(fake.requests).toHaveLength(1);
    } finally {
      fake.stop();
      await rm(packDir, { recursive: true, force: true });
      await rm(installDir, { recursive: true, force: true });
    }
  }, process.platform === "win32" ? 300_000 : undefined);

  it("persists model config through the launcher", async () => {
    const fake = await createFakeChatProvider((_body, _index) =>
      new Response(
        JSON.stringify({
          choices: [{ message: { content: "Configured summary." } }]
        }),
        { status: 200 }
      )
    );
    const dir = await mkdtemp(path.join(tmpdir(), "distill-e2e-config-"));
    const configPath = path.join(dir, "config.json");

    try {
      const setModel = await runLauncher(["config", "model", "my-model"], {
        env: {
          DISTILL_CONFIG_PATH: configPath
        }
      });

      const result = await runLauncher(["summarize"], {
        env: {
          DISTILL_CONFIG_PATH: configPath,
          DISTILL_HOST: fake.host
        },
        inputSteps: [{ data: "all good\n" }]
      });

      expect(setModel.stdout).toBe("model=my-model\n");
      expect(result.stdout).toBe("Configured summary.\n");
      expect(fake.requests).toHaveLength(1);
      expect(fake.requests[0]).toMatchObject({
        model: "my-model"
      });
    } finally {
      fake.stop();
      await rm(dir, { recursive: true, force: true });
    }
  });
});
