import { describe, expect, it, setDefaultTimeout } from "bun:test";
import { chmod, mkdtemp, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

import {
  requestBitnet,
  resolveBitnetRuntime,
  resolvePythonBin,
  shutdownBitnetWorker,
  runBitnetTest
} from "../src/bitnet";
import type { RuntimeConfig } from "../src/config";

setDefaultTimeout(15_000);

async function createFakePython(
  body: string,
  executableName = "python3"
): Promise<{ path: string; dir: string }> {
  const dir = await mkdtemp(path.join(tmpdir(), "distill-bitnet-"));
  const scriptPath = path.join(dir, executableName);

  await writeFile(
    scriptPath,
    `#!/bin/sh
exec node - "$@" <<'NODE'
const net = require("node:net");
const fs = require("node:fs");
const socketPath = process.argv[process.argv.length - 3];
const runtime = process.argv[process.argv.length - 2];
const model = process.argv[process.argv.length - 1];
const launchCountFile = process.env.DISTILL_FAKE_LAUNCH_COUNT_FILE;

if (launchCountFile) {
  const current = fs.existsSync(launchCountFile)
    ? Number(fs.readFileSync(launchCountFile, "utf8") || "0")
    : 0;
  fs.writeFileSync(launchCountFile, String(current + 1));
}

try {
  fs.unlinkSync(socketPath);
} catch {}

function defaultResponse(payload) {
  return {
    ok: true,
    output: "ok",
    python: process.argv[1],
    runtime: payload.runtime ?? runtime,
    backend: runtime === "mlx" ? "mlx" : "cuda"
  };
}

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

${body}
  });
});

server.listen(socketPath);
NODE
`,
    "utf8"
  );
  await chmod(scriptPath, 0o755);

  return { path: scriptPath, dir };
}

function config(overrides: Partial<RuntimeConfig> = {}): RuntimeConfig {
  return {
    provider: "bitnet",
    question: "summarize",
    model: "test-model",
    host: "http://127.0.0.1:11434",
    timeoutMs: 1_000,
    thinking: false,
    ...overrides
  };
}

describe("bitnet", () => {
  it("prefers DISTILL_PYTHON_BIN", async () => {
    const fake = await createFakePython("socket.end(JSON.stringify(defaultResponse(payload)));", "custom-python");

    try {
      const resolved = await resolvePythonBin({
        DISTILL_PYTHON_BIN: fake.path,
        PATH: ""
      });

      expect(resolved).toBe(fake.path);
    } finally {
      await rm(fake.dir, { recursive: true, force: true });
    }
  });

  it("falls back from python3 to python", async () => {
    const fake = await createFakePython("socket.end(JSON.stringify(defaultResponse(payload)));", "python");
    const originalCwd = process.cwd();

    try {
      process.chdir(fake.dir);
      const resolved = await resolvePythonBin({
        PATH: fake.dir
      });

      expect(resolved).toBe(path.join(fake.dir, "python"));
    } finally {
      process.chdir(originalCwd);
      await rm(fake.dir, { recursive: true, force: true });
    }
  });

  it("prefers a local .venv python before PATH lookup", async () => {
    const workspaceDir = await mkdtemp(path.join(tmpdir(), "distill-bitnet-venv-"));
    const venvDir = path.join(workspaceDir, ".venv", "bin");
    const fake = await createFakePython(
      "socket.end(JSON.stringify(defaultResponse(payload)));",
      "python3"
    );
    const originalCwd = process.cwd();

    try {
      await Bun.$`mkdir -p ${venvDir}`;
      await writeFile(
        path.join(venvDir, "python3"),
        await readFile(fake.path, "utf8"),
        "utf8"
      );
      await chmod(path.join(venvDir, "python3"), 0o755);
      process.chdir(workspaceDir);

      const resolved = await resolvePythonBin({
        PATH: ""
      });

      expect(await realpath(resolved)).toBe(await realpath(path.join(venvDir, "python3")));
    } finally {
      process.chdir(originalCwd);
      await rm(fake.dir, { recursive: true, force: true });
      await rm(workspaceDir, { recursive: true, force: true });
    }
  });

  it("prefers VIRTUAL_ENV before PATH lookup", async () => {
    const workspaceDir = await mkdtemp(path.join(tmpdir(), "distill-bitnet-virtualenv-"));
    const venvDir = path.join(workspaceDir, "bin");
    const fake = await createFakePython(
      "socket.end(JSON.stringify(defaultResponse(payload)));",
      "python3"
    );

    try {
      await Bun.$`mkdir -p ${venvDir}`;
      await writeFile(
        path.join(venvDir, "python3"),
        await readFile(fake.path, "utf8"),
        "utf8"
      );
      await chmod(path.join(venvDir, "python3"), 0o755);

      const resolved = await resolvePythonBin({
        VIRTUAL_ENV: workspaceDir,
        PATH: ""
      });

      expect(await realpath(resolved)).toBe(await realpath(path.join(venvDir, "python3")));
    } finally {
      await rm(fake.dir, { recursive: true, force: true });
      await rm(workspaceDir, { recursive: true, force: true });
    }
  });

  it("prefers env.PWD when resolving a local .venv", async () => {
    const workspaceDir = await mkdtemp(path.join(tmpdir(), "distill-bitnet-pwd-"));
    const venvDir = path.join(workspaceDir, ".venv", "bin");
    const fake = await createFakePython(
      "socket.end(JSON.stringify(defaultResponse(payload)));",
      "python3"
    );
    const originalCwd = process.cwd();

    try {
      await Bun.$`mkdir -p ${venvDir}`;
      await writeFile(
        path.join(venvDir, "python3"),
        await readFile(fake.path, "utf8"),
        "utf8"
      );
      await chmod(path.join(venvDir, "python3"), 0o755);
      process.chdir(tmpdir());

      const resolved = await resolvePythonBin({
        PATH: "",
        PWD: workspaceDir
      });

      expect(await realpath(resolved)).toBe(await realpath(path.join(venvDir, "python3")));
    } finally {
      process.chdir(originalCwd);
      await rm(fake.dir, { recursive: true, force: true });
      await rm(workspaceDir, { recursive: true, force: true });
    }
  });

  it("selects the runtime by platform", () => {
    expect(resolveBitnetRuntime("darwin", "arm64")).toBe("mlx");
    expect(resolveBitnetRuntime("linux", "x64")).toBe("transformers");
  });

  it("sends the request payload to the python daemon", async () => {
    const dir = await mkdtemp(path.join(tmpdir(), "distill-bitnet-payload-"));
    const capturePath = path.join(dir, "payload.json");
    const model = "test-model-payload";
    const fake = await createFakePython(
      `
fs.writeFileSync(${JSON.stringify(capturePath)}, input);
socket.end(JSON.stringify({
  ok: true,
  output: "concise",
  python: process.argv[1],
  runtime: payload.runtime,
  backend: payload.runtime === "mlx" ? "mlx" : "cuda"
}));
      `
    );
    const previous = process.env.DISTILL_PYTHON_BIN;
    process.env.DISTILL_PYTHON_BIN = fake.path;

    try {
      const output = await requestBitnet({
        ...config({ question: "what changed?", model }),
        timeoutMs: 1_500
      });

      const payload = JSON.parse(await readFile(capturePath, "utf8")) as Record<
        string,
        unknown
      >;

      expect(output).toBe("concise");
      expect(payload).toMatchObject({
        mode: "generate",
        model,
        prompt: "what changed?",
        runtime: resolveBitnetRuntime()
      });
    } finally {
      if (previous === undefined) {
        delete process.env.DISTILL_PYTHON_BIN;
      } else {
        process.env.DISTILL_PYTHON_BIN = previous;
      }
      await shutdownBitnetWorker(model, {
        ...process.env,
        DISTILL_PYTHON_BIN: fake.path
      });
      await rm(fake.dir, { recursive: true, force: true });
      await rm(dir, { recursive: true, force: true });
    }
  });

  it("reuses the same worker across requests for the same model", async () => {
    const dir = await mkdtemp(path.join(tmpdir(), "distill-bitnet-reuse-"));
    const launchCountPath = path.join(dir, "launch-count.txt");
    const model = "test-model-reuse";
    const fake = await createFakePython(
      `
socket.end(JSON.stringify({
  ok: true,
  output: "reuse",
  python: process.argv[1],
  runtime: payload.runtime,
  backend: payload.runtime === "mlx" ? "mlx" : "cuda"
}));
      `
    );
    const previousPython = process.env.DISTILL_PYTHON_BIN;
    const previousLaunchCount = process.env.DISTILL_FAKE_LAUNCH_COUNT_FILE;
    process.env.DISTILL_PYTHON_BIN = fake.path;
    process.env.DISTILL_FAKE_LAUNCH_COUNT_FILE = launchCountPath;

    try {
      await requestBitnet(config({ question: "one", model }));
      await requestBitnet(config({ question: "two", model }));

      expect(await readFile(launchCountPath, "utf8")).toBe("1");
    } finally {
      if (previousPython === undefined) {
        delete process.env.DISTILL_PYTHON_BIN;
      } else {
        process.env.DISTILL_PYTHON_BIN = previousPython;
      }
      if (previousLaunchCount === undefined) {
        delete process.env.DISTILL_FAKE_LAUNCH_COUNT_FILE;
      } else {
        process.env.DISTILL_FAKE_LAUNCH_COUNT_FILE = previousLaunchCount;
      }
      await shutdownBitnetWorker(model, {
        ...process.env,
        DISTILL_PYTHON_BIN: fake.path
      });
      await rm(fake.dir, { recursive: true, force: true });
      await rm(dir, { recursive: true, force: true });
    }
  });

  it("resolves a newline-delimited response without waiting for socket close", async () => {
    const model = "test-model-framed-response";
    const fake = await createFakePython(
      `
socket.write(JSON.stringify({
  ok: true,
  output: "framed",
  python: process.argv[1],
  runtime: payload.runtime,
  backend: payload.runtime === "mlx" ? "mlx" : "cuda"
}) + "\\n");
setTimeout(() => socket.end(), 5_000);
      `
    );
    const previous = process.env.DISTILL_PYTHON_BIN;
    process.env.DISTILL_PYTHON_BIN = fake.path;

    try {
      const startedAt = Date.now();
      const output = await requestBitnet(config({ question: "frame", model }));
      const elapsedMs = Date.now() - startedAt;

      expect(output).toBe("framed");
      expect(elapsedMs).toBeLessThan(4_000);
    } finally {
      if (previous === undefined) {
        delete process.env.DISTILL_PYTHON_BIN;
      } else {
        process.env.DISTILL_PYTHON_BIN = previous;
      }
      await shutdownBitnetWorker(model, {
        ...process.env,
        DISTILL_PYTHON_BIN: fake.path
      });
      await rm(fake.dir, { recursive: true, force: true });
    }
  });

  it("returns a clear test report when the runner succeeds", async () => {
    const model = "test-model-report";
    const fake = await createFakePython(
      `
socket.end(JSON.stringify({
  ok: true,
  output: "ok",
  python: process.argv[1],
  runtime: payload.runtime,
  backend: payload.runtime === "mlx" ? "mlx" : "rocm"
}));
      `
    );

    const previous = process.env.DISTILL_PYTHON_BIN;
    process.env.DISTILL_PYTHON_BIN = fake.path;

    try {
      const result = await runBitnetTest(config({ model }));

      expect(result.ok).toBe(true);
      expect(result.lines).toContain("provider: bitnet");
      expect(result.lines).toContain("model load: ok");
      expect(result.lines).toContain("generate: ok");
    } finally {
      if (previous === undefined) {
        delete process.env.DISTILL_PYTHON_BIN;
      } else {
        process.env.DISTILL_PYTHON_BIN = previous;
      }
      await shutdownBitnetWorker(model, {
        ...process.env,
        DISTILL_PYTHON_BIN: fake.path
      });
      await rm(fake.dir, { recursive: true, force: true });
    }
  });

  it("surfaces subprocess failures", async () => {
    const model = "test-model-failure";
    const fake = await createFakePython(
      `
socket.end(JSON.stringify({
  ok: false,
  stage: "model load",
  error: "boom",
  backend: "mlx"
}));
      `
    );
    const previous = process.env.DISTILL_PYTHON_BIN;
    process.env.DISTILL_PYTHON_BIN = fake.path;

    try {
      const result = await runBitnetTest(config({ model }));

      expect(result.ok).toBe(false);
      expect(result.lines).toContain("model load: failed");
      expect(result.lines).toContain("error: boom");
    } finally {
      if (previous === undefined) {
        delete process.env.DISTILL_PYTHON_BIN;
      } else {
        process.env.DISTILL_PYTHON_BIN = previous;
      }
      await shutdownBitnetWorker(model, {
        ...process.env,
        DISTILL_PYTHON_BIN: fake.path
      });
      await rm(fake.dir, { recursive: true, force: true });
    }
  });

  it("fails fast on an empty response instead of timing out", async () => {
    const model = "test-model-empty-response";
    const fake = await createFakePython(
      `
socket.end("");
      `
    );
    const previous = process.env.DISTILL_PYTHON_BIN;
    process.env.DISTILL_PYTHON_BIN = fake.path;

    try {
      const startedAt = Date.now();
      const result = await runBitnetTest(config({ model, timeoutMs: 1_500 }));
      const elapsedMs = Date.now() - startedAt;

      expect(result.ok).toBe(false);
      expect(result.lines).toContain("generate: failed");
      expect(result.lines).toContain("error: bitnet returned an empty response.");
      expect(elapsedMs).toBeLessThan(1_500);
    } finally {
      if (previous === undefined) {
        delete process.env.DISTILL_PYTHON_BIN;
      } else {
        process.env.DISTILL_PYTHON_BIN = previous;
      }
      await shutdownBitnetWorker(model, {
        ...process.env,
        DISTILL_PYTHON_BIN: fake.path
      });
      await rm(fake.dir, { recursive: true, force: true });
    }
  });
});
