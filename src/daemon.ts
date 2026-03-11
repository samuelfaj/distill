import { constants as fsConstants, existsSync } from "node:fs";
import { access, mkdir, unlink, writeFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";

import type { RuntimeConfig } from "./config";
import type { ProviderTestResult } from "./provider-test";
import {
  buildBitnetPythonEnv,
  resolveBitnetRuntime,
  resolvePythonBin,
  type BitnetBackend,
  type BitnetRuntime
} from "./bitnet";
import { requestOllama, requestOllamaDetailed } from "./ollama";
import { DISTILL_TEST_PROMPT as DAEMON_TEST_PROMPT } from "./provider-test-fixture";

const DAEMON_DIR = path.join(tmpdir(), "distill");
const DEFAULT_DAEMON_SOCKET = path.join(DAEMON_DIR, "daemon-v2.sock");
const MAX_SUMMARY_TOKENS = 24;
const MAX_TEST_TOKENS = 16;
const DAEMON_STARTUP_TIMEOUT_MS = 120_000;
const ENGINE_REQUEST_TIMEOUT_MS = 90_000;
export const DISTILL_DAEMON_PROTOCOL_VERSION = 2;

type DaemonConfig = Omit<RuntimeConfig, "question">;

type DaemonRequest =
  | { type: "ping" }
  | { type: "summarize"; config: RuntimeConfig; prompt: string }
  | { type: "test"; config: DaemonConfig };

type DaemonResponse =
  | {
      ok: true;
      output?: string;
      result?: ProviderTestResult;
      status?: string;
      provider?: DaemonConfig["provider"];
      model?: string;
      pid?: number;
      protocolVersion?: number;
    }
  | {
      ok: false;
      error: string;
      provider?: DaemonConfig["provider"];
      model?: string;
      pid?: number;
      protocolVersion?: number;
    };

export function isDistillDaemonCompatible(
  response: DaemonResponse | null | undefined,
  config: DaemonConfig
): boolean {
  return Boolean(
    response?.ok &&
      response.protocolVersion === DISTILL_DAEMON_PROTOCOL_VERSION &&
      response.provider === config.provider &&
      response.model === config.model
  );
}

interface BitnetReadyMessage {
  type: "ready";
  python: string;
  runtime: BitnetRuntime;
  backend: BitnetBackend;
  modelLoadMs: number;
}

interface BitnetStartupErrorMessage {
  type: "startup_error";
  error: string;
  stage: string;
}

interface BitnetSuccessMessage {
  id: string;
  ok: true;
  output: string;
  tokenCount?: number;
  generationMs?: number;
  tokenPerSecond?: number;
}

interface BitnetFailureMessage {
  id: string;
  ok: false;
  error: string;
  stage?: string;
}

type BitnetMessage =
  | BitnetReadyMessage
  | BitnetStartupErrorMessage
  | BitnetSuccessMessage
  | BitnetFailureMessage;

interface PendingRequest {
  reject(error: Error): void;
  resolve(value: BitnetSuccessMessage): void;
  timeout: ReturnType<typeof setTimeout>;
}

interface BitnetEngineInfo {
  python: string;
  runtime: BitnetRuntime;
  backend: BitnetBackend;
  modelLoadMs: number;
}

type DaemonStatus = "starting" | "ready" | "error";

interface DaemonState {
  config: DaemonConfig;
  bitnetEngine: BitnetDaemonEngine | null;
  status: DaemonStatus;
  error: string | null;
}

export interface DistillDaemonHandle {
  close(): Promise<void>;
  socketPath: string;
}

const daemonStartupBySocket = new Map<string, Promise<void>>();

function estimateTokens(input: string): number {
  return Math.max(1, Math.ceil(input.trim().length / 4));
}

function formatSavedPercent(prompt: string, response: string): number {
  const promptTokens = estimateTokens(prompt);
  const responseTokens = estimateTokens(response);
  const saved = ((promptTokens - responseTokens) / promptTokens) * 100;
  return Math.max(0, Math.round(saved));
}

function formatTokensPerSecond(
  tokenPerSecond: number | undefined,
  tokenCount: number | undefined,
  generationMs: number | undefined
): string {
  if (typeof tokenPerSecond === "number" && Number.isFinite(tokenPerSecond)) {
    return tokenPerSecond.toFixed(1);
  }

  if (!tokenCount || !generationMs || generationMs <= 0) {
    return "n/a";
  }

  return (tokenCount / (generationMs / 1000)).toFixed(1);
}

function buildProviderTestReport(options: {
  prompt: string;
  response?: string;
  tokenPerSecond?: number;
  tokenCount?: number;
  generationMs?: number;
  lines: string[];
  ok: boolean;
}): ProviderTestResult {
  return {
    ok: options.ok,
    lines: [
      "Original prompt:",
      options.prompt,
      "",
      "Final response:",
      options.response ?? "",
      "",
      `Saved ${formatSavedPercent(options.prompt, options.response ?? "")}% tokens.`,
      "",
      `token/s: ${formatTokensPerSecond(
        options.tokenPerSecond,
        options.tokenCount,
        options.generationMs
      )}`,
      "",
      ...options.lines
    ]
  };
}

export function getDaemonSocketPath(
  env: NodeJS.ProcessEnv = process.env
): string {
  return env.DISTILL_DAEMON_SOCKET?.trim() || DEFAULT_DAEMON_SOCKET;
}

async function removeSocketIfPresent(socketPath: string): Promise<void> {
  try {
    await unlink(socketPath);
  } catch {
    // Ignore stale socket cleanup failures.
  }
}

async function pathExists(filePath: string): Promise<boolean> {
  return access(filePath, fsConstants.F_OK)
    .then(() => true)
    .catch(() => false);
}

async function readSocketLine(socket: net.Socket): Promise<string> {
  return await new Promise<string>((resolve, reject) => {
    let data = "";

    socket.on("data", (chunk) => {
      data += chunk.toString("utf8");
      const newlineIndex = data.indexOf("\n");

      if (newlineIndex === -1) {
        return;
      }

      resolve(data.slice(0, newlineIndex));
    });

    socket.on("error", reject);
    socket.on("end", () => {
      if (!data.trim()) {
        reject(new Error("distill daemon returned an empty response."));
      } else {
        resolve(data.trim());
      }
    });
  });
}

export async function requestDistillDaemon(
  request: DaemonRequest,
  timeoutMs: number,
  env: NodeJS.ProcessEnv = process.env
): Promise<DaemonResponse> {
  const socketPath = getDaemonSocketPath(env);

  return await new Promise<DaemonResponse>((resolve, reject) => {
    const socket = net.createConnection({ path: socketPath });
    const timeout = setTimeout(() => {
      socket.destroy();
      reject(new Error(`distill daemon request timed out after ${timeoutMs}ms.`));
    }, timeoutMs);

    socket.on("connect", () => {
      socket.write(`${JSON.stringify(request)}\n`);
    });

    readSocketLine(socket)
      .then((raw) => {
        clearTimeout(timeout);
        resolve(JSON.parse(raw) as DaemonResponse);
      })
      .catch((error) => {
        clearTimeout(timeout);
        reject(error);
      });

    socket.on("error", (error) => {
      clearTimeout(timeout);
      reject(error);
    });
  });
}

export async function tryRequestDistillDaemon(
  request: DaemonRequest,
  timeoutMs: number,
  env: NodeJS.ProcessEnv = process.env
): Promise<DaemonResponse | null> {
  try {
    return await requestDistillDaemon(request, timeoutMs, env);
  } catch {
    return null;
  }
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

export async function waitForDistillDaemonReady(
  timeoutMs: number,
  env: NodeJS.ProcessEnv = process.env
): Promise<void> {
  const startedAt = Date.now();
  let lastError: Error | null = null;

  while (Date.now() - startedAt < timeoutMs) {
    try {
      const remainingMs = Math.max(1, timeoutMs - (Date.now() - startedAt));
      const response = await requestDistillDaemon(
        { type: "ping" },
        Math.min(1_000, remainingMs),
        env
      );

      if (!response.ok) {
        throw new Error(response.error);
      }

      if (response.status === "ready") {
        return;
      }

      lastError = new Error("distill daemon is still starting.");
    } catch (error) {
      lastError = error instanceof Error ? error : new Error("distill daemon is unavailable.");
    }

    await delay(150);
  }

  throw lastError ?? new Error(`distill daemon did not become ready within ${timeoutMs}ms.`);
}

export function isDistillDaemonAutostartDisabled(
  env: NodeJS.ProcessEnv = process.env
): boolean {
  return env.DISTILL_DISABLE_DAEMON_AUTOSTART === "1";
}

function resolveDaemonLaunchCommand(
  env: NodeJS.ProcessEnv
): { command: string; args: string[] } {
  const explicit = env.DISTILL_DAEMON_BIN?.trim();

  if (explicit) {
    return {
      command: explicit,
      args: []
    };
  }

  const scriptPath = process.argv[1];

  if (scriptPath && /[/\\]src[/\\]cli\.(?:ts|js|mjs|cjs)$/.test(scriptPath)) {
    const repoRoot = path.resolve(path.dirname(scriptPath), "..");
    const launcherPath = path.join(repoRoot, "packages", "cli", "bin", "distill.js");

    if (existsSync(launcherPath)) {
      return {
        command: env.DISTILL_NODE_BIN?.trim() || "node",
        args: [launcherPath]
      };
    }
  }

  if (scriptPath && /\.(?:[cm]?js|ts)$/.test(scriptPath)) {
    return {
      command: process.execPath,
      args: [scriptPath]
    };
  }

  return {
    command: process.execPath,
    args: []
  };
}

function spawnDistillDaemonProcess(
  config: DaemonConfig,
  env: NodeJS.ProcessEnv
): void {
  const launch = resolveDaemonLaunchCommand(env);
  const shellQuote = (value: string) => `'${value.replaceAll("'", `'\\''`)}'`;
  const daemonCommand = [
    launch.command,
    ...launch.args,
    "daemon",
    "--provider",
    config.provider,
    "--model",
    config.model
  ]
    .map(shellQuote)
    .join(" ");
  const child = spawn(
    "/bin/sh",
    [
      "-lc",
      `nohup ${daemonCommand} >/dev/null 2>&1 < /dev/null &`
    ],
    {
      detached: true,
      stdio: "ignore",
      env: {
        ...env
      }
    }
  );

  child.unref();
}

async function waitForDaemonShutdown(
  env: NodeJS.ProcessEnv,
  timeoutMs = 5_000
): Promise<void> {
  const startedAt = Date.now();

  while (Date.now() - startedAt < timeoutMs) {
    const response = await tryRequestDistillDaemon({ type: "ping" }, 250, env);

    if (!response) {
      return;
    }

    await delay(100);
  }

  throw new Error("distill daemon did not stop in time.");
}

export async function stopDistillDaemon(
  env: NodeJS.ProcessEnv = process.env
): Promise<void> {
  const response = await tryRequestDistillDaemon({ type: "ping" }, 500, env);

  if (!response || typeof response.pid !== "number") {
    return;
  }

  try {
    process.kill(response.pid, "SIGTERM");
  } catch {
    return;
  }
  await waitForDaemonShutdown(env);
}

export async function ensureDistillDaemonRunning(
  config: DaemonConfig,
  env: NodeJS.ProcessEnv = process.env
): Promise<void> {
  const socketPath = getDaemonSocketPath(env);
  const existing = daemonStartupBySocket.get(socketPath);

  if (existing) {
    await existing;
    return;
  }

  const startup = (async () => {
    const response = await tryRequestDistillDaemon({ type: "ping" }, 250, env);

    if (response?.ok) {
      const matchesConfig = isDistillDaemonCompatible(response, config);

      if (matchesConfig && response.status === "ready") {
        return;
      }

      if (matchesConfig && response.status === "starting") {
        await waitForDistillDaemonReady(config.timeoutMs, env);
        return;
      }

      if (typeof response.pid === "number") {
        try {
          process.kill(response.pid, "SIGTERM");
        } catch {
          // Ignore races if the daemon exited between ping and restart.
        }
        await waitForDaemonShutdown(env);
      }
    } else if (response && !response.ok) {
      if (typeof response.pid === "number") {
        try {
          process.kill(response.pid, "SIGTERM");
        } catch {
          // Ignore races if the daemon exited between ping and restart.
        }
        await waitForDaemonShutdown(env);
      } else if (await pathExists(socketPath)) {
        await removeSocketIfPresent(socketPath);
      }
    }

    spawnDistillDaemonProcess(config, env);
    await waitForDistillDaemonReady(config.timeoutMs, env);

    const ready = await requestDistillDaemon({ type: "ping" }, 1_000, env);

    if (
      !ready.ok ||
      ready.protocolVersion !== DISTILL_DAEMON_PROTOCOL_VERSION ||
      ready.provider !== config.provider ||
      ready.model !== config.model
    ) {
      throw new Error("distill daemon started with a different configuration.");
    }
  })().finally(() => {
    daemonStartupBySocket.delete(socketPath);
  });

  daemonStartupBySocket.set(socketPath, startup);
  await startup;
}

async function ensureRunnerFile(): Promise<string> {
  const runnerPath = path.join(
    DAEMON_DIR,
    `bitnet-engine-${createHash("sha1").update(PYTHON_ENGINE).digest("hex").slice(0, 12)}.py`
  );
  await mkdir(DAEMON_DIR, { recursive: true });
  if (await pathExists(runnerPath)) {
    await removeSocketIfPresent(runnerPath);
  }
  await writeFile(runnerPath, PYTHON_ENGINE, "utf8");
  return runnerPath;
}

function parseBitnetMessage(line: string): BitnetMessage | null {
  try {
    const parsed = JSON.parse(line) as Partial<BitnetMessage>;

    if (typeof parsed !== "object" || parsed === null) {
      return null;
    }

    if (parsed.type === "ready" || parsed.type === "startup_error") {
      return parsed as BitnetMessage;
    }

    if (typeof parsed.ok === "boolean") {
      return parsed as BitnetMessage;
    }

    return null;
  } catch {
    return null;
  }
}

function debugDaemonLog(env: NodeJS.ProcessEnv, message: string): void {
  if (env.DISTILL_DEBUG_DAEMON !== "1") {
    return;
  }

  process.stderr.write(`[distill-daemon] ${message}\n`);
}

class BitnetDaemonEngine {
  private readonly config: DaemonConfig;
  private readonly env: NodeJS.ProcessEnv;
  private child: ChildProcessWithoutNullStreams | null = null;
  private buffer = "";
  private pending: PendingRequest | null = null;
  private readyInfo: BitnetEngineInfo | null = null;
  private startupResolve: ((value: BitnetEngineInfo) => void) | null = null;
  private startupReject: ((reason?: unknown) => void) | null = null;
  private queue: Promise<void> = Promise.resolve();

  constructor(config: DaemonConfig, env: NodeJS.ProcessEnv) {
    this.config = config;
    this.env = env;
  }

  async start(): Promise<BitnetEngineInfo> {
    const pythonBin = await resolvePythonBin(this.env);
    const runtime = resolveBitnetRuntime();
    const runnerPath = await ensureRunnerFile();
    const childEnv = await buildBitnetPythonEnv(
      {
        ...process.env,
        ...this.env
      },
      this.config.model
    );

    this.child = spawn(pythonBin, [runnerPath, runtime, this.config.model], {
      stdio: ["pipe", "pipe", "pipe"],
      env: childEnv
    });

    this.child.stdout.setEncoding("utf8");
    this.child.stdout.on("data", (chunk: string) => {
      debugDaemonLog(this.env, `engine stdout chunk=${JSON.stringify(chunk)}`);
      this.buffer += chunk;
      this.flushBuffer();
    });

    this.child.stderr.setEncoding("utf8");
    this.child.stderr.on("data", (chunk: string) => {
      debugDaemonLog(this.env, `engine stderr chunk=${JSON.stringify(chunk)}`);
    });

    this.child.on("exit", (code, signal) => {
      debugDaemonLog(
        this.env,
        `engine exit signal=${signal ?? "none"} code=${code ?? "none"}`
      );
      const message = new Error(
        `bitnet engine exited unexpectedly (${signal ?? code ?? "unknown"}).`
      );

      if (this.startupReject) {
        this.startupReject(message);
        this.startupReject = null;
        this.startupResolve = null;
      }

      if (this.pending) {
        clearTimeout(this.pending.timeout);
        this.pending.reject(message);
        this.pending = null;
      }

      this.child = null;
    });

    return await new Promise<BitnetEngineInfo>((resolve, reject) => {
      this.startupResolve = resolve;
      this.startupReject = reject;

      setTimeout(() => {
        if (this.readyInfo || this.startupReject === null) {
          return;
        }

        this.startupReject(
          new Error(
            `bitnet daemon engine did not become ready in time after ${DAEMON_STARTUP_TIMEOUT_MS}ms.`
          )
        );
        this.startupReject = null;
        this.startupResolve = null;
      }, DAEMON_STARTUP_TIMEOUT_MS);
    });
  }

  private flushBuffer(): void {
    while (true) {
      const newlineIndex = this.buffer.indexOf("\n");

      if (newlineIndex === -1) {
        return;
      }

      const line = this.buffer.slice(0, newlineIndex).trim();
      this.buffer = this.buffer.slice(newlineIndex + 1);

      if (!line) {
        continue;
      }

      debugDaemonLog(this.env, `engine stdout line=${JSON.stringify(line)}`);
      const message = parseBitnetMessage(line);

      if (!message) {
        debugDaemonLog(this.env, "engine stdout line ignored");
        continue;
      }

      this.handleMessage(message);
    }
  }

  private handleMessage(message: BitnetMessage): void {
    debugDaemonLog(this.env, `engine message=${JSON.stringify(message)}`);
    if (message.type === "ready") {
      this.readyInfo = {
        python: message.python,
        runtime: message.runtime,
        backend: message.backend,
        modelLoadMs: message.modelLoadMs
      };
      this.startupResolve?.(this.readyInfo);
      this.startupResolve = null;
      this.startupReject = null;
      return;
    }

    if (message.type === "startup_error") {
      this.startupReject?.(
        new Error(`${message.stage}: ${message.error}`)
      );
      this.startupResolve = null;
      this.startupReject = null;
      return;
    }

    if (!this.pending) {
      return;
    }

    const pending = this.pending;
    this.pending = null;
    clearTimeout(pending.timeout);

    if (!message.ok) {
      pending.reject(
        new Error(
          message.stage ? `${message.stage}: ${message.error}` : message.error
        )
      );
      return;
    }

    pending.resolve(message);
  }

  private enqueue<T>(task: () => Promise<T>): Promise<T> {
    const next = this.queue.then(task, task);
    this.queue = next.then(
      () => undefined,
      () => undefined
    );
    return next;
  }

  private async sendRequest(payload: Record<string, unknown>): Promise<BitnetSuccessMessage> {
    if (!this.child) {
      throw new Error("bitnet daemon engine is not running.");
    }

    if (!this.readyInfo) {
      throw new Error("bitnet daemon engine is not ready.");
    }

    return await new Promise<BitnetSuccessMessage>((resolve, reject) => {
      const timeout = setTimeout(() => {
        this.pending = null;
        reject(
          new Error(
            `bitnet daemon engine request timed out after ${ENGINE_REQUEST_TIMEOUT_MS}ms.`
          )
        );
      }, ENGINE_REQUEST_TIMEOUT_MS);

      this.pending = {
        resolve,
        reject,
        timeout
      };

      this.child?.stdin.write(`${JSON.stringify(payload)}\n`);
    });
  }

  get info(): BitnetEngineInfo {
    if (!this.readyInfo) {
      throw new Error("bitnet daemon engine is not ready.");
    }

    return this.readyInfo;
  }

  async summarize(prompt: string): Promise<BitnetSuccessMessage> {
    return await this.enqueue(() =>
      this.sendRequest({
        id: String(Date.now() + Math.random()),
        type: "generate",
        prompt,
        maxTokens: MAX_SUMMARY_TOKENS
      })
    );
  }

  async selfTest(): Promise<BitnetSuccessMessage> {
    return await this.enqueue(() =>
      this.sendRequest({
        id: String(Date.now() + Math.random()),
        type: "generate",
        prompt: DAEMON_TEST_PROMPT,
        maxTokens: MAX_TEST_TOKENS
      })
    );
  }

  async close(): Promise<void> {
    if (!this.child) {
      return;
    }

    try {
      await this.enqueue(() =>
        this.sendRequest({
          id: String(Date.now() + Math.random()),
          type: "shutdown"
        })
      );
    } catch {
      // Ignore shutdown races.
    }

    this.child.kill("SIGTERM");
    this.child = null;
  }
}

async function createBitnetTestResult(
  config: DaemonConfig,
  engine: BitnetDaemonEngine
): Promise<ProviderTestResult> {
  const info = engine.info;

  try {
    const result = await engine.selfTest();

    return buildProviderTestReport({
      prompt: DAEMON_TEST_PROMPT,
      response: result.output.trim(),
      tokenPerSecond: result.tokenPerSecond,
      tokenCount: result.tokenCount,
      generationMs: result.generationMs,
      ok: true,
      lines: [
        "provider: bitnet",
        `model: ${config.model}`,
        `python: ok (${info.python})`,
        `runtime: ${info.runtime}`,
        `backend: ${info.backend}`,
        "model load: ok",
        "generate: ok"
      ]
    });
  } catch (error) {
    return buildProviderTestReport({
      prompt: DAEMON_TEST_PROMPT,
      ok: false,
      lines: [
        "provider: bitnet",
        `model: ${config.model}`,
        `python: ok (${info.python})`,
        `runtime: ${info.runtime}`,
        `backend: ${info.backend}`,
        "model load: ok",
        "generate: failed",
        `error: ${error instanceof Error ? error.message : "Unknown bitnet error."}`
      ]
    });
  }
}

function bitnetConfigMismatchError(
  daemonConfig: DaemonConfig,
  requestConfig: DaemonConfig
): string | null {
  if (requestConfig.provider !== daemonConfig.provider) {
    return `distill daemon provider mismatch: daemon=${daemonConfig.provider}, request=${requestConfig.provider}.`;
  }

  if (requestConfig.model !== daemonConfig.model) {
    return `distill daemon model mismatch: daemon=${daemonConfig.model}, request=${requestConfig.model}.`;
  }

  return null;
}

async function handleDaemonRequest(
  request: DaemonRequest,
  state: DaemonState
): Promise<DaemonResponse> {
  if (request.type === "ping") {
    if (state.status === "error") {
      return {
        ok: false,
        error: state.error ?? "distill daemon failed to start.",
        provider: state.config.provider,
        model: state.config.model,
        pid: process.pid,
        protocolVersion: DISTILL_DAEMON_PROTOCOL_VERSION
      };
    }

    return {
      ok: true,
      status: state.status,
      provider: state.config.provider,
      model: state.config.model,
      pid: process.pid,
      protocolVersion: DISTILL_DAEMON_PROTOCOL_VERSION
    };
  }

  try {
    if (state.config.provider === "bitnet") {
      if (state.status === "starting") {
        return {
          ok: false,
          error: "distill daemon is still starting."
        };
      }

      if (state.status === "error") {
        return {
          ok: false,
          error: state.error ?? "distill daemon failed to start."
        };
      }

      const mismatch = bitnetConfigMismatchError(state.config, request.config);

      if (mismatch) {
        return {
          ok: false,
          error: mismatch,
          protocolVersion: DISTILL_DAEMON_PROTOCOL_VERSION
        };
      }

      if (!state.bitnetEngine) {
        return {
          ok: false,
          error: "bitnet daemon engine is not available."
        };
      }

      if (request.type === "summarize") {
        const result = await state.bitnetEngine.summarize(request.prompt);
        return {
          ok: true,
          output: result.output.trim()
        };
      }

      return {
        ok: true,
        result: await createBitnetTestResult(state.config, state.bitnetEngine)
      };
    }

    if (request.type === "summarize") {
      const output = await requestOllama({
        host: request.config.host,
        model: request.config.model,
        prompt: request.prompt,
        timeoutMs: request.config.timeoutMs,
        thinking: request.config.thinking
      });

      return {
        ok: true,
        output
      };
    }

    const response = await requestOllamaDetailed({
      host: request.config.host,
      model: request.config.model,
      prompt: DAEMON_TEST_PROMPT,
      timeoutMs: request.config.timeoutMs,
      thinking: request.config.thinking
    });

    return {
      ok: true,
      result: buildProviderTestReport({
        prompt: DAEMON_TEST_PROMPT,
        response: response.output,
        tokenCount: response.evalCount,
        generationMs:
          response.evalDurationNs === undefined
            ? undefined
            : response.evalDurationNs / 1_000_000,
        ok: true,
        lines: [
          "provider: ollama",
          `host: ${request.config.host}`,
          `model: ${request.config.model}`,
          "generate: ok"
        ]
      })
    };
  } catch (error) {
    return {
      ok: false,
      error: error instanceof Error ? error.message : "distill daemon request failed."
    };
  }
}

export async function startDistillDaemonServer(
  config: DaemonConfig,
  env: NodeJS.ProcessEnv = process.env
): Promise<DistillDaemonHandle> {
  const socketPath = getDaemonSocketPath(env);
  await mkdir(path.dirname(socketPath), { recursive: true });

  const existing = await tryRequestDistillDaemon({ type: "ping" }, 1_000, env);

  if (existing?.ok) {
    throw new Error(`distill daemon is already running at ${socketPath}.`);
  }

  if (await pathExists(socketPath)) {
    await removeSocketIfPresent(socketPath);
  }

  const bitnetEngine =
    config.provider === "bitnet"
      ? new BitnetDaemonEngine(config, env)
      : null;

  const state: DaemonState = {
    config,
    bitnetEngine,
    status: bitnetEngine ? "starting" : "ready",
    error: null
  };

  const server = net.createServer((socket) => {
    let data = "";
    let handled = false;

    socket.on("data", async (chunk) => {
      if (handled) {
        return;
      }

      data += chunk.toString("utf8");
      const newlineIndex = data.indexOf("\n");

      if (newlineIndex === -1) {
        return;
      }

      handled = true;

      let response: DaemonResponse;

      try {
        const request = JSON.parse(data.slice(0, newlineIndex)) as DaemonRequest;
        response = await handleDaemonRequest(request, state);
      } catch (error) {
        response = {
          ok: false,
          error: error instanceof Error ? error.message : "Invalid daemon request."
        };
      }

      socket.end(`${JSON.stringify(response)}\n`);
    });

    socket.on("error", () => {
      socket.destroy();
    });
  });

  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(socketPath, () => {
      server.off("error", reject);
      resolve();
    });
  });

  if (bitnetEngine) {
    bitnetEngine
      .start()
      .then(() => {
        state.status = "ready";
        state.error = null;
      })
      .catch((error) => {
        state.status = "error";
        state.error =
          error instanceof Error ? error.message : "distill daemon failed to start.";
      });
  }

  return {
    socketPath,
    async close() {
      await new Promise<void>((resolve, reject) => {
        server.close((error) => {
          if (error) {
            reject(error);
            return;
          }

          resolve();
        });
      });
      await bitnetEngine?.close();
      await removeSocketIfPresent(socketPath);
    }
  };
}

export async function runDistillDaemon(
  config: DaemonConfig,
  env: NodeJS.ProcessEnv = process.env
): Promise<void> {
  const handle = await startDistillDaemonServer(config, env);
  process.stdout.write(`socket=${handle.socketPath}\n`);

  const shutdown = async () => {
    await handle.close();
    process.exit(0);
  };

  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);
  process.on("SIGHUP", shutdown);
}

const PYTHON_ENGINE = String.raw`
import contextlib
import io
import json
import os
import re
import sys
import time
import warnings

os.environ.setdefault("HF_HUB_DISABLE_PROGRESS_BARS", "1")
os.environ.setdefault("TOKENIZERS_PARALLELISM", "false")
warnings.filterwarnings("ignore")

RUNTIME = sys.argv[1]
MODEL_NAME = sys.argv[2]
STATE = {
    "model": None,
    "tokenizer": None,
    "generate": None,
    "backend": None,
}

class StageError(Exception):
    def __init__(self, stage, message):
        super().__init__(message)
        self.stage = stage

def build_prompt(tokenizer, prompt):
    if hasattr(tokenizer, "chat_template") and tokenizer.chat_template:
        return tokenizer.apply_chat_template(
            [{"role": "user", "content": prompt}],
            tokenize=False,
            add_generation_prompt=True,
        )
    return prompt

def load_mlx(model_name):
    try:
        from mlx_lm import load, generate
    except Exception as exc:
        raise StageError("dependencies", f"Missing dependency mlx-lm: {exc}") from exc

    try:
        captured = io.StringIO()
        with contextlib.redirect_stdout(captured), contextlib.redirect_stderr(captured):
            model, tokenizer = load(model_name)
    except Exception as exc:
        raise StageError("model load", str(exc)) from exc

    return model, tokenizer, generate

def load_transformers(model_name):
    try:
        import torch
        from transformers import AutoModelForCausalLM, AutoTokenizer
    except Exception as exc:
        raise StageError("dependencies", f"Missing dependencies torch/transformers: {exc}") from exc

    if not torch.cuda.is_available():
        raise StageError("dependencies", "CUDA/ROCm device not available.")

    backend = "rocm" if getattr(torch.version, "hip", None) else "cuda"

    try:
        captured = io.StringIO()
        with contextlib.redirect_stdout(captured), contextlib.redirect_stderr(captured):
            tokenizer = AutoTokenizer.from_pretrained(model_name, trust_remote_code=True)
            model = AutoModelForCausalLM.from_pretrained(
                model_name,
                trust_remote_code=True,
                torch_dtype="auto",
            )
            model.to("cuda")
    except Exception as exc:
        raise StageError("model load", str(exc)) from exc

    return model, tokenizer, backend

def initialize():
    started_at = time.perf_counter()
    if RUNTIME == "mlx":
        model, tokenizer, generate = load_mlx(MODEL_NAME)
        STATE["model"] = model
        STATE["tokenizer"] = tokenizer
        STATE["generate"] = generate
        STATE["backend"] = "mlx"
    elif RUNTIME == "transformers":
        model, tokenizer, backend = load_transformers(MODEL_NAME)
        STATE["model"] = model
        STATE["tokenizer"] = tokenizer
        STATE["backend"] = backend
    else:
        raise StageError("runtime", f"Unsupported runtime: {RUNTIME}")

    return (time.perf_counter() - started_at) * 1000

def run_mlx(prompt, max_tokens):
    try:
        rendered_prompt = build_prompt(STATE["tokenizer"], prompt)
        started_at = time.perf_counter()
        captured = io.StringIO()
        with contextlib.redirect_stdout(captured), contextlib.redirect_stderr(captured):
            output = STATE["generate"](
                STATE["model"],
                STATE["tokenizer"],
                prompt=rendered_prompt,
                max_tokens=max_tokens,
                verbose=True if max_tokens <= 16 else False,
            )
        generation_ms = (time.perf_counter() - started_at) * 1000
        token_count = len(STATE["tokenizer"].encode(output))
        verbose_text = captured.getvalue()
        matches = re.findall(r"([0-9]+(?:\.[0-9]+)?)\s*(?:tok/s|tokens/s)", verbose_text)
        token_per_second = float(matches[-1]) if matches else None
    except Exception as exc:
        raise StageError("generate", str(exc)) from exc

    return output.strip(), token_count, generation_ms, token_per_second

def run_transformers(prompt, max_tokens):
    try:
        rendered_prompt = build_prompt(STATE["tokenizer"], prompt)
        inputs = STATE["tokenizer"](rendered_prompt, return_tensors="pt")
        inputs = {key: value.to(STATE["model"].device) for key, value in inputs.items()}
        pad_token_id = STATE["tokenizer"].eos_token_id or STATE["model"].config.eos_token_id
        started_at = time.perf_counter()
        captured = io.StringIO()
        with contextlib.redirect_stdout(captured), contextlib.redirect_stderr(captured):
            output_ids = STATE["model"].generate(
                **inputs,
                max_new_tokens=max_tokens,
                do_sample=False,
                pad_token_id=pad_token_id,
            )
        generation_ms = (time.perf_counter() - started_at) * 1000
        new_tokens = output_ids[0][inputs["input_ids"].shape[1]:]
        token_count = int(new_tokens.shape[0])
        output = STATE["tokenizer"].decode(new_tokens, skip_special_tokens=True).strip()
    except Exception as exc:
        raise StageError("generate", str(exc)) from exc

    return output, token_count, generation_ms, None

def handle_request(request):
    if request["type"] == "shutdown":
        return {
            "id": request["id"],
            "ok": True,
            "output": "shutdown",
            "tokenCount": 0,
            "generationMs": 0,
            "tokenPerSecond": None,
        }

    prompt = request["prompt"]
    max_tokens = int(request.get("maxTokens", 24))

    try:
        if RUNTIME == "mlx":
            output, token_count, generation_ms, token_per_second = run_mlx(prompt, max_tokens)
        else:
            output, token_count, generation_ms, token_per_second = run_transformers(prompt, max_tokens)
        return {
            "id": request["id"],
            "ok": True,
            "output": output,
            "tokenCount": token_count,
            "generationMs": generation_ms,
            "tokenPerSecond": token_per_second,
        }
    except StageError as exc:
        return {
            "id": request["id"],
            "ok": False,
            "error": str(exc),
            "stage": exc.stage,
        }

def emit(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

def main():
    try:
        model_load_ms = initialize()
        emit({
            "type": "ready",
            "python": sys.executable,
            "runtime": RUNTIME,
            "backend": STATE["backend"],
            "modelLoadMs": model_load_ms,
        })
    except StageError as exc:
        emit({
            "type": "startup_error",
            "stage": exc.stage,
            "error": str(exc),
        })
        raise SystemExit(1)

    for line in sys.stdin:
        raw = line.strip()
        if not raw:
            continue

        try:
            request = json.loads(raw)
        except Exception as exc:
            emit({
                "id": "invalid",
                "ok": False,
                "error": str(exc),
                "stage": "request",
            })
            continue

        response = handle_request(request)
        emit(response)

        if request["type"] == "shutdown":
            break

if __name__ == "__main__":
    main()
`;
