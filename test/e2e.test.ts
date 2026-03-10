import {
  beforeAll,
  describe,
  expect,
  it,
  setDefaultTimeout
} from "bun:test";
import cliPackage from "../packages/cli/package.json";
import { readdirSync } from "node:fs";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { spawn, spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import path from "node:path";

setDefaultTimeout(60_000);

const root = path.resolve(import.meta.dir, "..");
const launcher = path.join(root, "packages", "cli", "bin", "distill.js");
const expectedVersion = "0.1.0";
const WATCH_IDLE_MS = 1_800;
const WATCH_START_DELAY_MS = 600;
const INTERACTIVE_DELAY_MS = 1_000;
const currentPlatformPackage = (() => {
  const key = `${process.platform}-${process.arch}`;
  const mapping: Record<string, string> = {
    "darwin-arm64": "@samuelfaj/distill-darwin-arm64",
    "darwin-x64": "@samuelfaj/distill-darwin-x64",
    "linux-arm64": "@samuelfaj/distill-linux-arm64",
    "linux-x64": "@samuelfaj/distill-linux-x64",
    "win32-x64": "@samuelfaj/distill-win32-x64"
  };

  const value = mapping[key];

  if (!value) {
    throw new Error(`Unsupported platform for e2e tests: ${key}`);
  }

  return value;
})();

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

function runOrThrow(
  command: string,
  args: string[],
  cwd: string,
  env?: NodeJS.ProcessEnv,
  input?: string
): string {
  const result = spawnSync(command, args, {
    cwd,
    env: {
      ...process.env,
      ...env
    },
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

async function runLauncher(args: string[], options?: {
  env?: NodeJS.ProcessEnv;
  inputSteps?: InputStep[];
  finalDelayMs?: number;
}): Promise<RunResult> {
  const child = spawn("node", [launcher, ...args], {
    cwd: root,
    env: {
      ...process.env,
      ...options?.env
    },
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
    for (const step of options?.inputSteps ?? []) {
      await delay(step.afterMs ?? 0);

      if (!child.stdin.destroyed && !child.killed) {
        child.stdin.write(step.data);
      }
    }

    await delay(options?.finalDelayMs ?? 0);

    if (!child.stdin.destroyed && !child.killed) {
      child.stdin.end();
    }
  })();

  const exit = new Promise<RunResult>((resolve, reject) => {
    child.on("error", reject);
    child.on("close", (code, signal) => {
      resolve({
        code,
        signal,
        stdout,
        stderr
      });
    });
  });

  await writer;
  return exit;
}

async function createFakeOllama(
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
      const payload = (await request.json()) as Record<string, unknown>;
      requests.push(payload);
      return responder(payload, requests.length - 1);
    }
  });

  return {
    host: `http://127.0.0.1:${server.port}`,
    requests,
    stop() {
      server.stop(true);
    }
  };
}

function normalizePtyOutput(output: string): string {
  return output
    .replace(/\u001b\[[0-9;]*[A-Za-z]/g, "")
    .replace(/\r/g, "\n")
    .replace(/[\u0004\u0008]/g, "")
    .split("\n")
    .map((line) => line.trimEnd())
    .filter(Boolean)
    .join("\n");
}

beforeAll(() => {
  runOrThrow("npm", ["run", "build"], root);
});

describe("distill end-to-end", () => {
  it("summarizes batch output through the npm launcher", async () => {
    const fake = await createFakeOllama((_body, _index) =>
      new Response(JSON.stringify({ response: "All tests passed." }), {
        status: 200
      })
    );

    try {
      const result = await runLauncher(["did the tests pass?"], {
        env: {
          OLLAMA_HOST: fake.host
        },
        inputSteps: [{ data: "Ran 12 tests\n12 passed\n" }]
      });

      expect(result.code).toBe(0);
      expect(result.stderr).toBe("");
      expect(result.stdout).toBe("All tests passed.\n");
      expect(fake.requests).toHaveLength(1);
      expect(fake.requests[0]).toMatchObject({
        stream: false,
        think: false,
        model: "phi3:mini"
      });
    } finally {
      fake.stop();
    }
  });

  it("keeps the spinner moving in a pty while collecting streamed input and summarizing", async () => {
    if (process.platform === "win32") {
      return;
    }

    const fake = await createFakeOllama(async (_body, _index) => {
      await delay(700);
      return new Response(JSON.stringify({ response: "All tests passed." }), {
        status: 200
      });
    });
    const dir = await mkdtemp(path.join(tmpdir(), "distill-e2e-pty-"));
    const capturePath = path.join(dir, "terminal.log");
    const shellCommand =
      "perl -e '$|=1; for (1..8) { print qq(Ran chunk $_\\n); select undef,undef,undef,0.18; }' | " +
      `node ${launcher} 'did the tests pass?'`;

    try {
      runOrThrow(
        "script",
        ["-q", capturePath, "zsh", "-lc", shellCommand],
        root,
        {
          OLLAMA_HOST: fake.host
        }
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

  it("falls back to the raw input when Ollama is unavailable", async () => {
    const result = await runLauncher(["summarize briefly"], {
      env: {
        OLLAMA_HOST: "http://127.0.0.1:9",
        DISTILL_TIMEOUT_MS: "150"
      },
      inputSteps: [{ data: "fallback payload\n" }]
    });

    expect(result.code).toBe(0);
    expect(result.stderr).toBe("");
    expect(result.stdout).toBe("fallback payload\n");
  });

  it("detects watch-like recurring output and emits watch summaries", async () => {
    const fake = await createFakeOllama((body) => {
      const prompt = String(body.prompt ?? "");

      if (!prompt.includes("Previous cycle:") || !prompt.includes("Current cycle:")) {
        return new Response(JSON.stringify({ response: "unexpected prompt" }), {
          status: 200
        });
      }

      return new Response(JSON.stringify({ response: "Failure count changed from 0 to 1." }), {
        status: 200
      });
    });

    try {
      const result = await runLauncher(["what changed?"], {
        env: {
          OLLAMA_HOST: fake.host
        },
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
      expect(String(fake.requests[0].prompt ?? "")).toContain("Previous cycle:");
    } finally {
      fake.stop();
    }
  });

  it("passes through simple interactive prompts without calling Ollama", async () => {
    const fake = await createFakeOllama((_body, _index) =>
      new Response(JSON.stringify({ response: "should not happen" }), {
        status: 200
      })
    );

    try {
      const result = await runLauncher(["confirm the action"], {
        env: {
          OLLAMA_HOST: fake.host
        },
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
    const fake = await createFakeOllama((_body, _index) =>
      new Response(JSON.stringify({ response: "Tests passed." }), {
        status: 200
      })
    );

    const packDir = await mkdtemp(path.join(tmpdir(), "distill-e2e-pack-"));
    const installDir = await mkdtemp(path.join(tmpdir(), "distill-e2e-install-"));

    try {
      runOrThrow(
        "npm",
        ["pack", "--workspace", currentPlatformPackage, "--pack-destination", packDir],
        root
      );
      runOrThrow(
        "npm",
        ["pack", "--workspace", "@samuelfaj/distill", "--pack-destination", packDir],
        root
      );
      runOrThrow("npm", ["init", "-y"], installDir);

      const tarballs = readdirSync(packDir)
        .sort()
        .map((entry) => path.join(packDir, entry));
      runOrThrow("npm", ["install", ...tarballs], installDir);

      const version = runOrThrow(
        process.platform === "win32"
          ? path.join(installDir, "node_modules", ".bin", "distill.cmd")
          : path.join(installDir, "node_modules", ".bin", "distill"),
        ["--version"],
        installDir
      );
      expect(version).toBe(expectedVersion);

      const summary = runOrThrow(
        process.platform === "win32"
          ? path.join(installDir, "node_modules", ".bin", "distill.cmd")
          : path.join(installDir, "node_modules", ".bin", "distill"),
        ["did the tests pass?"],
        installDir,
        {
          OLLAMA_HOST: fake.host
        },
        "12 passed\n"
      );

      expect(summary).toBe("Tests passed.");
      expect(fake.requests).toHaveLength(1);
    } finally {
      fake.stop();
      await rm(packDir, { recursive: true, force: true });
      await rm(installDir, { recursive: true, force: true });
    }
  });

  it("persists model and thinking config through the launcher", async () => {
    const fake = await createFakeOllama((_body, _index) =>
      new Response(JSON.stringify({ response: "Configured summary." }), {
        status: 200
      })
    );
    const dir = await mkdtemp(path.join(tmpdir(), "distill-e2e-config-"));
    const configPath = path.join(dir, "config.json");

    try {
      const setModel = await runLauncher(["config", "model", "phi3:mini"], {
        env: {
          DISTILL_CONFIG_PATH: configPath
        }
      });

      const setThinking = await runLauncher(["config", "thinking", "true"], {
        env: {
          DISTILL_CONFIG_PATH: configPath
        }
      });

      const result = await runLauncher(["summarize"], {
        env: {
          DISTILL_CONFIG_PATH: configPath,
          OLLAMA_HOST: fake.host
        },
        inputSteps: [{ data: "all good\n" }]
      });

      expect(setModel.stdout).toBe("model=phi3:mini\n");
      expect(setThinking.stdout).toBe("thinking=true\n");
      expect(result.stdout).toBe("Configured summary.\n");
      expect(fake.requests).toHaveLength(1);
      expect(fake.requests[0]).toMatchObject({
        model: "phi3:mini",
        think: true
      });
    } finally {
      fake.stop();
      await rm(dir, { recursive: true, force: true });
    }
  });
});
