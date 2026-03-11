import {
  beforeAll,
  describe,
  expect,
  it,
  setDefaultTimeout
} from "bun:test";
import cliPackage from "../packages/cli/package.json";
import { stopDistillDaemon } from "../src/daemon";
import { DISTILL_TEST_QUESTION } from "../src/provider-test-fixture";
import { readdirSync } from "node:fs";
import { chmod, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { spawn, spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import path from "node:path";

setDefaultTimeout(60_000);

const root = path.resolve(import.meta.dir, "..");
const launcher = path.join(root, "packages", "cli", "bin", "distill.js");
const expectedVersion = cliPackage.version;
const WATCH_IDLE_MS = 1_800;
const WATCH_START_DELAY_MS = 600;
const INTERACTIVE_DELAY_MS = 1_000;
const currentPlatformPackage = (() => {
  const key = `${process.platform}-${process.arch}`;
  const mapping: Record<string, string> = {
    "darwin-arm64": "@samuelfaj/distill-darwin-arm64",
    "darwin-x64": "@samuelfaj/distill-darwin-x64",
    "linux-arm64": "@samuelfaj/distill-linux-arm64",
    "linux-x64": "@samuelfaj/distill-linux-x64"
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

async function createFakePython(
  responder: (payload: Record<string, unknown>) => Record<string, unknown>
): Promise<{ path: string; stop: () => Promise<void> }> {
  const dir = await mkdtemp(path.join(tmpdir(), "distill-e2e-python-"));
  const scriptPath = path.join(dir, "python3");
  const script = `#!/bin/sh
exec node - "$@" <<'NODE'
const net = require("node:net");
const fs = require("node:fs");
const socketPath = process.argv[process.argv.length - 3];
const runtime = process.argv[process.argv.length - 2];

function defaultResponse(payload) {
  return {
    ok: true,
    output: "ok",
    python: process.argv[1],
    runtime: payload.runtime ?? runtime,
    backend: runtime === "mlx" ? "mlx" : "cuda"
  };
}

try {
  fs.unlinkSync(socketPath);
} catch {}

const server = net.createServer((socket) => {
  let input = "";
  socket.on("data", (chunk) => {
    input += chunk.toString();
    if (!input.includes("\\n")) {
      return;
    }

    const payload = JSON.parse(input.split("\\n")[0]);
    if (payload.mode === "ping") {
      socket.end(JSON.stringify(defaultResponse(payload)));
      return;
    }
    if (payload.mode === "shutdown") {
      socket.end(JSON.stringify(defaultResponse(payload)));
      server.close(() => process.exit(0));
      return;
    }
    const response = (${responder.toString()})(payload);
    socket.end(JSON.stringify(response));
  });
});

server.listen(socketPath);
NODE
`;
  await writeFile(scriptPath, script, "utf8");
  await chmod(scriptPath, 0o755);

  return {
    path: scriptPath,
    stop: () => rm(dir, { recursive: true, force: true })
  };
}

async function createFakePythonEngine(
  responder?: (payload: Record<string, unknown>) => Record<string, unknown>
): Promise<{ path: string; stop: () => Promise<void> }> {
  const dir = await mkdtemp(path.join(tmpdir(), "distill-e2e-engine-"));
  const scriptPath = path.join(dir, "python3");
  const enginePath = path.join(dir, "engine.js");

  const responderSource =
    responder?.toString() ??
    `(payload) => ({
      id: String(payload.id ?? ""),
      ok: true,
      output:
        String(payload.prompt ?? "").includes(${JSON.stringify(DISTILL_TEST_QUESTION)}) &&
        String(payload.prompt ?? "").includes("desktop/tailwind.config.ts")
          ? "ok"
          : "Bitnet summary.",
      tokenCount: 2,
      generationMs: 10,
      tokenPerSecond: 200
    })`;

  await writeFile(
    enginePath,
    `const runtime = process.argv[process.argv.length - 2];
console.log(JSON.stringify({
  type: "ready",
  python: process.argv[1],
  runtime,
  backend: runtime === "mlx" ? "mlx" : "cuda",
  modelLoadMs: 1
}));
let buffer = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  const lines = buffer.split("\\n");
  buffer = lines.pop() ?? "";

  for (const line of lines) {
    if (!line.trim()) continue;
    const payload = JSON.parse(line);
    if (payload.type === "shutdown") {
      console.log(JSON.stringify({
        id: payload.id,
        ok: true,
        output: "shutdown",
        tokenCount: 0,
        generationMs: 0
      }));
      process.exit(0);
    }

    const response = (${responderSource})(payload);

    console.log(JSON.stringify(response));
  }
});
process.stdin.resume();
`,
    "utf8"
  );

  await writeFile(
    scriptPath,
    `#!/bin/sh
exec node ${JSON.stringify(enginePath)} "$@"
`,
    "utf8"
  );
  await chmod(scriptPath, 0o755);

  return {
    path: scriptPath,
    stop: () => rm(dir, { recursive: true, force: true })
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
      const result = await runLauncher(["--provider", "ollama", "did the tests pass?"], {
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
        model: "qwen3.5:2b"
      });
    } finally {
      fake.stop();
    }
  });

  it("keeps the spinner moving in a pty while collecting streamed input and summarizing", async () => {
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
      `node ${launcher} --provider ollama 'did the tests pass?'`;

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
    const result = await runLauncher(["--provider", "ollama", "summarize briefly"], {
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
      const result = await runLauncher(["--provider", "ollama", "what changed?"], {
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
      const result = await runLauncher(["--provider", "ollama", "confirm the action"], {
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
        path.join(installDir, "node_modules", ".bin", "distill"),
        ["--version"],
        installDir
      );
      expect(version).toBe(expectedVersion);

      const summary = runOrThrow(
        path.join(installDir, "node_modules", ".bin", "distill"),
        ["--provider", "ollama", "did the tests pass?"],
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
      const setModel = await runLauncher(["config", "model", "qwen3.5:2b"], {
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
          DISTILL_PROVIDER: "ollama",
          OLLAMA_HOST: fake.host
        },
        inputSteps: [{ data: "all good\n" }]
      });

      expect(setModel.stdout).toBe("model=qwen3.5:2b\n");
      expect(setThinking.stdout).toBe("thinking=true\n");
      expect(result.stdout).toBe("Configured summary.\n");
      expect(fake.requests).toHaveLength(1);
      expect(fake.requests[0]).toMatchObject({
        model: "qwen3.5:2b",
        think: true
      });
    } finally {
      fake.stop();
      await rm(dir, { recursive: true, force: true });
    }
  });

  it("uses bitnet by default with a fake python runner", async () => {
    const fake = await createFakePythonEngine();
    const socketDir = await mkdtemp(path.join(tmpdir(), "distill-e2e-daemon-"));
    const socketPath = path.join(socketDir, "daemon.sock");
    const daemonEnv = {
      ...process.env,
      DISTILL_PYTHON_BIN: fake.path,
      DISTILL_DAEMON_SOCKET: socketPath
    };

    try {
      const result = await runLauncher(["did the tests pass?"], {
        env: {
          DISTILL_PYTHON_BIN: fake.path,
          DISTILL_DAEMON_SOCKET: socketPath,
          DISTILL_MODEL: "test-model"
        },
        inputSteps: [{ data: "Ran 12 tests\n12 passed\n" }]
      });

      expect(result.code).toBe(0);
      expect(result.stdout).toBe("Bitnet summary.\n");
      expect(result.stderr).toBe("");
    } finally {
      await stopDistillDaemon(daemonEnv);
      await fake.stop();
      await rm(socketDir, { recursive: true, force: true });
    }
  });

  it("runs distill test end-to-end for bitnet", async () => {
    const fake = await createFakePythonEngine((payload) => ({
      id: String(payload.id ?? ""),
      ok: true,
      output: "ok",
      tokenCount: 1,
      generationMs: 10,
      tokenPerSecond: 100
    }));
    const socketDir = await mkdtemp(path.join(tmpdir(), "distill-e2e-test-daemon-"));
    const socketPath = path.join(socketDir, "daemon.sock");
    const daemonEnv = {
      ...process.env,
      DISTILL_PYTHON_BIN: fake.path,
      DISTILL_DAEMON_SOCKET: socketPath
    };

    try {
      const result = await runLauncher(["test"], {
        env: {
          DISTILL_PYTHON_BIN: fake.path,
          DISTILL_DAEMON_SOCKET: socketPath
        }
      });

      expect(result.code).toBe(0);
      expect(result.stdout).toContain("Original prompt:");
      expect(result.stdout).toContain(DISTILL_TEST_QUESTION);
      expect(result.stdout).toContain("Final response:");
      expect(result.stdout).toContain("Saved ");
      expect(result.stdout).toContain("token/s:");
      expect(result.stdout).toContain("provider: bitnet");
      expect(result.stdout).toContain("generate: ok");
    } finally {
      await stopDistillDaemon(daemonEnv);
      await fake.stop();
      await rm(socketDir, { recursive: true, force: true });
    }
  });

  it("falls back to raw input when bitnet inference fails", async () => {
    const fake = await createFakePythonEngine((payload) => ({
      id: String(payload.id ?? ""),
      ok: false,
      error: "boom",
      stage: "generate"
    }));
    const socketDir = await mkdtemp(path.join(tmpdir(), "distill-e2e-fallback-daemon-"));
    const socketPath = path.join(socketDir, "daemon.sock");
    const daemonEnv = {
      ...process.env,
      DISTILL_PYTHON_BIN: fake.path,
      DISTILL_DAEMON_SOCKET: socketPath
    };

    try {
      const result = await runLauncher(["did the tests pass?"], {
        env: {
          DISTILL_PYTHON_BIN: fake.path,
          DISTILL_DAEMON_SOCKET: socketPath
        },
        inputSteps: [{ data: "fallback payload\n" }]
      });

      expect(result.code).toBe(0);
      expect(result.stdout).toBe("fallback payload\n");
    } finally {
      await stopDistillDaemon(daemonEnv);
      await fake.stop();
      await rm(socketDir, { recursive: true, force: true });
    }
  });

  it("runs distill test end-to-end for ollama", async () => {
    const fake = await createFakeOllama((body, _index) =>
      new Response(
        JSON.stringify({
          response:
            String(body.prompt ?? "").includes(DISTILL_TEST_QUESTION) &&
            String(body.prompt ?? "").includes("desktop/tailwind.config.ts")
              ? "ok"
              : "unexpected prompt"
        }),
        {
        status: 200
        }
      )
    );

    try {
      const result = await runLauncher(["test", "--provider", "ollama"], {
        env: {
          OLLAMA_HOST: fake.host
        }
      });

      expect(result.code).toBe(0);
      expect(result.stdout).toContain("Original prompt:");
      expect(result.stdout).toContain(DISTILL_TEST_QUESTION);
      expect(result.stdout).toContain("Final response:");
      expect(result.stdout).toContain("provider: ollama");
      expect(result.stdout).toContain("generate: ok");
    } finally {
      fake.stop();
    }
  });

  it("fails distill test for ollama when the host is unavailable", async () => {
    const result = await runLauncher(["test", "--provider", "ollama"], {
      env: {
        OLLAMA_HOST: "http://127.0.0.1:9",
        DISTILL_TIMEOUT_MS: "150"
      }
    });

    expect(result.code).toBe(1);
    expect(result.stdout).toContain("provider: ollama");
    expect(result.stdout).toContain("generate: failed");
  });
});
