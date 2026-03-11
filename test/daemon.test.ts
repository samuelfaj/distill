import { describe, expect, it } from "bun:test";
import { chmod, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

import {
  requestDistillDaemon,
  startDistillDaemonServer,
  waitForDistillDaemonReady
} from "../src/daemon";

async function createFakePython(): Promise<{ dir: string; path: string }> {
  const dir = await mkdtemp(path.join(tmpdir(), "distill-daemon-python-"));
  const scriptPath = path.join(dir, "python3");
  const enginePath = path.join(dir, "engine.js");

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

    console.log(JSON.stringify({
      id: payload.id,
      ok: true,
      output: payload.prompt === "You are validating a local summarization model. Reply with exactly this single word and nothing else: ok"
        ? "ok"
        : "Daemon summary.",
      tokenCount: 2,
      generationMs: 10,
      tokenPerSecond: 200
    }));
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

  return { dir, path: scriptPath };
}

describe("distill daemon", () => {
  it("serves summarize requests over the local socket", async () => {
    const tempDir = await mkdtemp(path.join(tmpdir(), "distill-daemon-"));
    const socketPath = path.join(tempDir, "daemon.sock");
    const fakePython = await createFakePython();
    const originalPython = process.env.DISTILL_PYTHON_BIN;
    const originalSocket = process.env.DISTILL_DAEMON_SOCKET;

    process.env.DISTILL_PYTHON_BIN = fakePython.path;
    process.env.DISTILL_DAEMON_SOCKET = socketPath;

    const handle = await startDistillDaemonServer(
      {
        provider: "bitnet",
        model: "test-model",
        host: "http://127.0.0.1:11434",
        timeoutMs: 2_000,
        thinking: false
      },
      process.env
    );

    try {
      await waitForDistillDaemonReady(2_000, process.env);
      const response = await requestDistillDaemon(
        {
          type: "summarize",
          config: {
            provider: "bitnet",
            question: "unused",
            model: "test-model",
            host: "http://127.0.0.1:11434",
            timeoutMs: 2_000,
            thinking: false
          },
          prompt: "summarize this"
        },
        2_000,
        process.env
      );

      expect(response).toEqual({
        ok: true,
        output: "Daemon summary."
      });
    } finally {
      await handle.close();
      if (originalPython === undefined) {
        delete process.env.DISTILL_PYTHON_BIN;
      } else {
        process.env.DISTILL_PYTHON_BIN = originalPython;
      }
      if (originalSocket === undefined) {
        delete process.env.DISTILL_DAEMON_SOCKET;
      } else {
        process.env.DISTILL_DAEMON_SOCKET = originalSocket;
      }
      await rm(fakePython.dir, { recursive: true, force: true });
      await rm(tempDir, { recursive: true, force: true });
    }
  });

  it("becomes ready even when the python engine prints startup noise", async () => {
    const tempDir = await mkdtemp(path.join(tmpdir(), "distill-daemon-noise-"));
    const socketPath = path.join(tempDir, "daemon.sock");
    const fakePython = await createFakePython();
    const originalPython = process.env.DISTILL_PYTHON_BIN;
    const originalSocket = process.env.DISTILL_DAEMON_SOCKET;

    process.env.DISTILL_PYTHON_BIN = fakePython.path;
    process.env.DISTILL_DAEMON_SOCKET = socketPath;

    await writeFile(
      path.join(fakePython.dir, "engine.js"),
      `const runtime = process.argv[process.argv.length - 2];
console.log("Fetching 7 files...");
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
        output: "shutdown"
      }));
      process.exit(0);
    }

    console.log(JSON.stringify({
      id: payload.id,
      ok: true,
      output: "Daemon summary."
    }));
  }
});
process.stdin.resume();
`,
      "utf8"
    );

    const handle = await startDistillDaemonServer(
      {
        provider: "bitnet",
        model: "test-model",
        host: "http://127.0.0.1:11434",
        timeoutMs: 2_000,
        thinking: false
      },
      process.env
    );

    try {
      await waitForDistillDaemonReady(2_000, process.env);
      const ping = await requestDistillDaemon({ type: "ping" }, 2_000, process.env);
      expect(ping).toMatchObject({
        ok: true,
        status: "ready",
        provider: "bitnet",
        model: "test-model"
      });
    } finally {
      await handle.close();
      if (originalPython === undefined) {
        delete process.env.DISTILL_PYTHON_BIN;
      } else {
        process.env.DISTILL_PYTHON_BIN = originalPython;
      }
      if (originalSocket === undefined) {
        delete process.env.DISTILL_DAEMON_SOCKET;
      } else {
        process.env.DISTILL_DAEMON_SOCKET = originalSocket;
      }
      await rm(fakePython.dir, { recursive: true, force: true });
      await rm(tempDir, { recursive: true, force: true });
    }
  });
});
