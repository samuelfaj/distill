import { describe, expect, it } from "bun:test";
import { chmod, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import path from "node:path";
import cliPackage from "../packages/cli/package.json";
import { stopDistillDaemon } from "../src/daemon";
import { DISTILL_TEST_QUESTION } from "../src/provider-test-fixture";

const root = path.resolve(import.meta.dir, "..");
const cli = path.join(root, "src", "cli.ts");

async function createFakePython(
  scriptBody: string
): Promise<{ path: string; cleanup: () => Promise<void> }> {
  const dir = await mkdtemp(path.join(tmpdir(), "distill-fake-python-"));
  const scriptPath = path.join(dir, "python3");
  await writeFile(
    scriptPath,
    `#!/bin/sh
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
${scriptBody}
  });
});

server.listen(socketPath);
NODE
`,
    "utf8"
  );
  await chmod(scriptPath, 0o755);

  return {
    path: scriptPath,
    cleanup: () => rm(dir, { recursive: true, force: true })
  };
}

describe("cli entrypoint", () => {
  it("prints help", () => {
    const result = spawnSync("bun", ["run", cli, "--help"], {
      cwd: root,
      encoding: "utf8"
    });

    expect(result.status).toBe(0);
    expect(result.stdout).toContain('cmd 2>&1 | distill "question"');
  });

  it("prints the version", () => {
    const result = spawnSync("bun", ["run", cli, "--version"], {
      cwd: root,
      encoding: "utf8"
    });

    expect(result.status).toBe(0);
    expect(result.stdout.trim()).toBe(cliPackage.version);
  });

  it("fails without stdin when attached to a tty", () => {
    const result = spawnSync(
      "script",
      ["-q", "/dev/null", "bun", "run", cli, "is this safe?"],
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
      const setProvider = spawnSync(
        "bun",
        ["run", cli, "config", "provider", "bitnet"],
        {
          cwd: root,
          encoding: "utf8",
          env: {
            ...process.env,
            DISTILL_CONFIG_PATH: configPath
          }
        }
      );

      const setModel = spawnSync(
        "bun",
        ["run", cli, "config", "model", "mlx-community/bitnet-b1.58-2B-4T"],
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
        "bun",
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

      expect(setProvider.status).toBe(0);
      expect(setModel.status).toBe(0);
      expect(setThinking.status).toBe(0);
      expect(JSON.parse(await readFile(configPath, "utf8"))).toEqual({
        provider: "bitnet",
        model: "mlx-community/bitnet-b1.58-2B-4T",
        thinking: false
      });
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  it("runs distill test with override flags without mutating persisted config", async () => {
    const dir = await mkdtemp(path.join(tmpdir(), "distill-cli-test-"));
    const configPath = path.join(dir, "config.json");
    const socketDir = await mkdtemp(path.join(tmpdir(), "distill-cli-daemon-"));
    const socketPath = path.join(socketDir, "daemon.sock");
    const fakePython = await createFakePython(`
socket.end(JSON.stringify({
  ok: true,
  python: process.argv[1],
  runtime: payload.runtime,
  backend: payload.runtime === "mlx" ? "mlx" : "cuda",
  output:
    String(payload.prompt ?? "").includes(${JSON.stringify(DISTILL_TEST_QUESTION)}) &&
    String(payload.prompt ?? "").includes("desktop/tailwind.config.ts")
      ? "ok"
      : "unexpected prompt"
}));
    `);

    try {
      await writeFile(
        configPath,
        JSON.stringify(
          {
            provider: "ollama",
            model: "qwen3.5:2b"
          },
          null,
          2
        ),
        "utf8"
      );

      const result = spawnSync(
        "bun",
        [
          "run",
          cli,
          "test",
          "--provider",
          "bitnet",
          "--model",
          "override-model"
        ],
        {
          cwd: root,
          encoding: "utf8",
          env: {
            ...process.env,
            DISTILL_CONFIG_PATH: configPath,
            DISTILL_DISABLE_DAEMON_AUTOSTART: "1",
            DISTILL_PYTHON_BIN: fakePython.path,
            DISTILL_DAEMON_SOCKET: socketPath
          }
        }
      );

      expect(result.status).toBe(0);
      expect(result.stdout).toContain("Original prompt:");
      expect(result.stdout).toContain(DISTILL_TEST_QUESTION);
      expect(result.stdout).toContain("Final response:");
      expect(result.stdout).toContain("Saved ");
      expect(result.stdout).toContain("token/s:");
      expect(result.stdout).toContain("provider: bitnet");
      expect(result.stdout).toContain("generate: ok");
      expect(JSON.parse(await readFile(configPath, "utf8"))).toEqual({
        provider: "ollama",
        model: "qwen3.5:2b"
      });
    } finally {
      await stopDistillDaemon({
        ...process.env,
        DISTILL_PYTHON_BIN: fakePython.path,
        DISTILL_DAEMON_SOCKET: socketPath
      });
      await fakePython.cleanup();
      await rm(dir, { recursive: true, force: true });
      await rm(socketDir, { recursive: true, force: true });
    }
  });

  it("fails distill test clearly when python is missing", () => {
    const socketPath = path.join(
      tmpdir(),
      `distill-cli-missing-${process.pid}-${Date.now()}.sock`
    );
    const result = spawnSync("bun", ["run", cli, "test", "--provider", "bitnet"], {
      cwd: root,
      encoding: "utf8",
      env: {
        ...process.env,
        DISTILL_DISABLE_DAEMON_AUTOSTART: "1",
        DISTILL_PYTHON_BIN: "/missing/python",
        DISTILL_DAEMON_SOCKET: socketPath
      }
    });

    expect(result.status).toBe(1);
    expect(result.stdout).toContain("Original prompt:");
    expect(result.stdout).toContain("python: failed");
  });

  it("fails distill test when the python runner returns invalid json", async () => {
    const fakePython = await createFakePython(`
socket.end("nope");
    `);
    const socketPath = path.join(
      tmpdir(),
      `distill-cli-invalid-json-${process.pid}-${Date.now()}.sock`
    );

    try {
      const result = spawnSync("bun", ["run", cli, "test", "--provider", "bitnet"], {
        cwd: root,
        encoding: "utf8",
        env: {
          ...process.env,
          DISTILL_DISABLE_DAEMON_AUTOSTART: "1",
          DISTILL_PYTHON_BIN: fakePython.path,
          DISTILL_DAEMON_SOCKET: socketPath
        }
      });

      expect(result.status).toBe(1);
      expect(result.stdout).toContain("generate: failed");
      expect(result.stdout).toContain("invalid JSON");
    } finally {
      await fakePython.cleanup();
    }
  });
});
