import { access, mkdir, unlink, writeFile } from "node:fs/promises";
import { constants as fsConstants } from "node:fs";
import { createHash } from "node:crypto";
import { spawn } from "node:child_process";
import net from "node:net";
import { homedir, tmpdir } from "node:os";
import path from "node:path";

import type { RuntimeConfig } from "./config";
import { DISTILL_TEST_PROMPT as SELF_TEST_PROMPT } from "./provider-test-fixture";

const MAX_GENERATION_TOKENS = 80;
const WORKER_STARTUP_TIMEOUT_MS = 30_000;
const DEFAULT_WORKER_IDLE_MS = 10 * 60 * 1_000;
const WORKER_BASE_DIR = path.join(tmpdir(), "distill-bitnet");

type Platform = NodeJS.Platform;

export type BitnetRuntime = "mlx" | "transformers";
export type BitnetBackend = "mlx" | "cuda" | "rocm";

interface PythonSuccess {
  ok: true;
  output: string;
  python: string;
  runtime: BitnetRuntime;
  backend: BitnetBackend;
  tokenCount?: number;
  generationMs?: number;
  tokenPerSecond?: number;
}

interface PythonFailure {
  ok: false;
  error: string;
  stage?: string;
  python?: string;
  runtime?: BitnetRuntime;
  backend?: BitnetBackend;
}

type PythonResponse = PythonSuccess | PythonFailure;

export interface BitnetTestResult {
  ok: boolean;
  lines: string[];
  response?: string;
  tokenCount?: number;
  generationMs?: number;
  tokenPerSecond?: number;
}

interface WorkerDescriptor {
  socketPath: string;
  runtime: BitnetRuntime;
  model: string;
  pythonBin: string;
}

const startupBySocket = new Map<string, Promise<void>>();

function getHuggingFaceModelCachePath(model: string): string | null {
  if (!model.includes("/")) {
    return null;
  }

  return path.join(
    homedir(),
    ".cache",
    "huggingface",
    "hub",
    `models--${model.replaceAll("/", "--")}`
  );
}

async function hasCachedHuggingFaceModel(model: string): Promise<boolean> {
  const cachePath = getHuggingFaceModelCachePath(model);

  if (!cachePath) {
    return false;
  }

  return access(cachePath, fsConstants.F_OK)
    .then(() => true)
    .catch(() => false);
}

export async function buildBitnetPythonEnv(
  baseEnv: NodeJS.ProcessEnv,
  model: string
): Promise<NodeJS.ProcessEnv> {
  const env: NodeJS.ProcessEnv = {
    ...baseEnv,
    HF_HUB_DISABLE_PROGRESS_BARS:
      baseEnv.HF_HUB_DISABLE_PROGRESS_BARS ?? "1",
    PYTHONUNBUFFERED: baseEnv.PYTHONUNBUFFERED ?? "1",
    TOKENIZERS_PARALLELISM: baseEnv.TOKENIZERS_PARALLELISM ?? "false",
    TRANSFORMERS_VERBOSITY: baseEnv.TRANSFORMERS_VERBOSITY ?? "error"
  };

  if (await hasCachedHuggingFaceModel(model)) {
    env.HF_HUB_OFFLINE = baseEnv.HF_HUB_OFFLINE ?? "1";
    env.TRANSFORMERS_OFFLINE = baseEnv.TRANSFORMERS_OFFLINE ?? "1";
  }

  return env;
}

function parseWorkerResponse(
  stdout: string,
  options: {
    allowPartial: boolean;
    emptyMessage: string;
    invalidMessage: string;
    payloadMessage: string;
  }
): { done: boolean; value?: PythonResponse; error?: Error } {
  const newlineIndex = stdout.indexOf("\n");
  const raw = (newlineIndex === -1 ? stdout : stdout.slice(0, newlineIndex)).trim();

  if (!raw) {
    if (!options.allowPartial) {
      return { done: true, error: new Error(options.emptyMessage) };
    }

    return {
      done: false,
      error: newlineIndex !== -1 ? new Error(options.emptyMessage) : undefined
    };
  }

  let parsed: unknown;

  try {
    parsed = JSON.parse(raw);
  } catch {
    if (newlineIndex === -1 && options.allowPartial) {
      return { done: false };
    }

    return { done: true, error: new Error(options.invalidMessage) };
  }

  if (
    typeof parsed !== "object" ||
    parsed === null ||
    typeof (parsed as { ok?: unknown }).ok !== "boolean"
  ) {
    return { done: true, error: new Error(options.payloadMessage) };
  }

  return { done: true, value: parsed as PythonResponse };
}

function isExecutable(filePath: string): Promise<boolean> {
  return access(filePath, fsConstants.X_OK)
    .then(() => true)
    .catch(() => false);
}

function splitPathEntries(pathValue: string | undefined): string[] {
  return (pathValue ?? "")
    .split(path.delimiter)
    .map((entry) => entry.trim())
    .filter(Boolean);
}

async function findExecutableInPath(
  command: string,
  env: NodeJS.ProcessEnv
): Promise<string | null> {
  if (command.includes(path.sep)) {
    return (await isExecutable(command)) ? command : null;
  }

  for (const directory of splitPathEntries(env.PATH)) {
    const candidate = path.join(directory, command);

    if (await isExecutable(candidate)) {
      return candidate;
    }
  }

  return null;
}

async function findLocalVenvPython(startDir = process.cwd()): Promise<string | null> {
  let current = path.resolve(startDir);

  while (true) {
    for (const candidate of [
      path.join(current, ".venv", "bin", "python3"),
      path.join(current, ".venv", "bin", "python")
    ]) {
      if (await isExecutable(candidate)) {
        return candidate;
      }
    }

    const parent = path.dirname(current);
    if (parent === current) {
      return null;
    }

    current = parent;
  }
}

async function findPythonInVirtualEnv(
  venvRoot: string | undefined
): Promise<string | null> {
  const root = venvRoot?.trim();

  if (!root) {
    return null;
  }

  for (const candidate of [
    path.join(root, "bin", "python3"),
    path.join(root, "bin", "python")
  ]) {
    if (await isExecutable(candidate)) {
      return candidate;
    }
  }

  return null;
}

export async function resolvePythonBin(
  env: NodeJS.ProcessEnv = process.env
): Promise<string> {
  const explicit = env.DISTILL_PYTHON_BIN?.trim();

  if (explicit) {
    const resolved = await findExecutableInPath(explicit, env);

    if (resolved) {
      return resolved;
    }

    throw new Error(`Python interpreter not found: ${explicit}`);
  }

  const activatedVenv = await findPythonInVirtualEnv(env.VIRTUAL_ENV);

  if (activatedVenv) {
    return activatedVenv;
  }

  const pwdVenv = await findLocalVenvPython(env.PWD);

  if (pwdVenv) {
    return pwdVenv;
  }

  const localVenv = await findLocalVenvPython();

  if (localVenv) {
    return localVenv;
  }

  for (const candidate of ["python3", "python"]) {
    const resolved = await findExecutableInPath(candidate, env);

    if (resolved) {
      return resolved;
    }
  }

  throw new Error(
    "Python interpreter not found. Set DISTILL_PYTHON_BIN or install python3."
  );
}

export function resolveBitnetRuntime(
  platform = process.platform as Platform,
  arch = process.arch
): BitnetRuntime {
  if (platform === "darwin" && arch === "arm64") {
    return "mlx";
  }

  if (platform === "linux") {
    return "transformers";
  }

  throw new Error(
    `Provider bitnet is only supported on Apple Silicon and Linux. Current platform: ${platform}/${arch}.`
  );
}

async function runPythonJson(
  descriptor: WorkerDescriptor,
  payload: Record<string, unknown>,
  timeoutMs: number
): Promise<PythonResponse> {
  await ensureBitnetWorker(descriptor, timeoutMs);

  return await new Promise<PythonResponse>((resolve, reject) => {
    const socket = net.createConnection({ path: descriptor.socketPath });
    let stdout = "";
    let finished = false;
    const timeout = setTimeout(() => {
      if (finished) {
        return;
      }

      finished = true;
      socket.destroy();
      reject(new Error(`bitnet request timed out after ${timeoutMs}ms.`));
    }, timeoutMs);

    const complete = (
      handler: () => void,
      options: { destroySocket?: boolean } = {}
    ) => {
      if (finished) {
        return;
      }

      finished = true;
      clearTimeout(timeout);
      if (options.destroySocket ?? false) {
        socket.destroy();
      }
      handler();
    };

    const tryResolve = (allowPartial: boolean) => {
      const parsed = parseWorkerResponse(stdout, {
        allowPartial,
        emptyMessage: "bitnet returned an empty response.",
        invalidMessage: "bitnet returned invalid JSON.",
        payloadMessage: "bitnet returned an invalid response payload."
      });

      if (!parsed.done) {
        return false;
      }

      complete(() => {
        if (parsed.error) {
          reject(parsed.error);
          return;
        }

        resolve(parsed.value as PythonResponse);
      });
      return true;
    };

    socket.on("connect", () => {
      socket.write(`${JSON.stringify(payload)}\n`);
    });

    socket.on("data", (chunk) => {
      stdout += chunk.toString("utf8");
      tryResolve(true);
    });

    socket.on("error", (error) => {
      complete(() => reject(error), { destroySocket: true });
    });

    socket.on("end", () => {
      tryResolve(false);
    });
  });
}

function getWorkerSocketPath(
  pythonBin: string,
  runtime: BitnetRuntime,
  model: string
): string {
  const digest = createHash("sha1")
    .update(`${pythonBin}\0${runtime}\0${model}`)
    .digest("hex")
    .slice(0, 24);

  return path.join(WORKER_BASE_DIR, `${digest}.sock`);
}

async function removeSocketIfPresent(socketPath: string): Promise<void> {
  try {
    await unlink(socketPath);
  } catch {
    // Ignore stale socket cleanup failures.
  }
}

async function ensurePythonRunnerFile(): Promise<string> {
  const runnerPath = path.join(
    WORKER_BASE_DIR,
    `worker-${createHash("sha1").update(PYTHON_RUNNER).digest("hex").slice(0, 12)}.py`
  );
  await mkdir(WORKER_BASE_DIR, { recursive: true });
  await writeFile(runnerPath, PYTHON_RUNNER, "utf8");
  return runnerPath;
}

function buildWorkerDescriptor(
  pythonBin: string,
  runtime: BitnetRuntime,
  model: string
): WorkerDescriptor {
  return {
    socketPath: getWorkerSocketPath(pythonBin, runtime, model),
    runtime,
    model,
    pythonBin
  };
}

async function pingBitnetWorker(
  descriptor: WorkerDescriptor,
  timeoutMs: number
): Promise<boolean> {
  try {
    const payload = await new Promise<PythonResponse>((resolve, reject) => {
      const socket = net.createConnection({ path: descriptor.socketPath });
      let stdout = "";
      let finished = false;
      const timeout = setTimeout(() => {
        if (finished) {
          return;
        }

        finished = true;
        socket.destroy();
        reject(new Error("timeout"));
      }, timeoutMs);

      const complete = (
        handler: () => void,
        options: { destroySocket?: boolean } = {}
      ) => {
        if (finished) {
          return;
        }

        finished = true;
        clearTimeout(timeout);
        if (options.destroySocket ?? false) {
          socket.destroy();
        }
        handler();
      };

      const tryResolve = (allowPartial: boolean) => {
        const parsed = parseWorkerResponse(stdout, {
          allowPartial,
          emptyMessage: "empty ping response",
          invalidMessage: "invalid ping response",
          payloadMessage: "invalid ping response"
        });

        if (!parsed.done) {
          return false;
        }

        complete(() => {
          if (parsed.error) {
            reject(parsed.error);
            return;
          }

          resolve(parsed.value as PythonResponse);
        });
        return true;
      };

      socket.on("connect", () => {
        socket.write(`${JSON.stringify({ mode: "ping" })}\n`);
      });

      socket.on("data", (chunk) => {
        stdout += chunk.toString("utf8");
        tryResolve(true);
      });

      socket.on("error", (error) => {
        complete(() => reject(error), { destroySocket: true });
      });

      socket.on("end", () => {
        tryResolve(false);
      });
    });

    return payload.ok;
  } catch {
    return false;
  }
}

async function waitForBitnetWorker(
  descriptor: WorkerDescriptor,
  timeoutMs: number
): Promise<void> {
  const deadline = Date.now() + timeoutMs;

  while (Date.now() < deadline) {
    if (await pingBitnetWorker(descriptor, 500)) {
      return;
    }

    await new Promise((resolve) => setTimeout(resolve, 100));
  }

  throw new Error("bitnet worker did not become ready in time.");
}

function spawnBitnetWorker(
  descriptor: WorkerDescriptor,
  runnerPath: string,
  env: NodeJS.ProcessEnv
): void {
  const child = spawn(
    descriptor.pythonBin,
    [runnerPath, descriptor.socketPath, descriptor.runtime, descriptor.model],
    {
      stdio: "ignore",
      env
    }
  );

  child.unref();
}

async function ensureBitnetWorker(
  descriptor: WorkerDescriptor,
  timeoutMs: number
): Promise<void> {
  if (await pingBitnetWorker(descriptor, 300)) {
    return;
  }

  const existing = startupBySocket.get(descriptor.socketPath);
  if (existing) {
    await existing;
    return;
  }

  const startup = (async () => {
    await mkdir(WORKER_BASE_DIR, { recursive: true });
    await removeSocketIfPresent(descriptor.socketPath);
    const runnerPath = await ensurePythonRunnerFile();
    const env = await buildBitnetPythonEnv(
      {
        ...process.env,
        DISTILL_BITNET_IDLE_MS:
          process.env.DISTILL_BITNET_IDLE_MS ?? String(DEFAULT_WORKER_IDLE_MS)
      },
      descriptor.model
    );
    spawnBitnetWorker(descriptor, runnerPath, env);
    await waitForBitnetWorker(descriptor, Math.max(timeoutMs, WORKER_STARTUP_TIMEOUT_MS));
  })()
    .finally(() => {
      startupBySocket.delete(descriptor.socketPath);
    });

  startupBySocket.set(descriptor.socketPath, startup);
  await startup;
}

export async function shutdownBitnetWorker(
  model: string,
  env: NodeJS.ProcessEnv = process.env
): Promise<void> {
  const pythonBin = await resolvePythonBin(env);
  const runtime = resolveBitnetRuntime();
  const descriptor = buildWorkerDescriptor(pythonBin, runtime, model);

  if (!(await pingBitnetWorker(descriptor, 200))) {
    return;
  }

  try {
    await runPythonJson(descriptor, { mode: "shutdown" }, 1_000);
  } catch {
    // Ignore shutdown races.
  }
}

export async function requestBitnet(config: RuntimeConfig): Promise<string> {
  const python = await resolvePythonBin(process.env);
  const runtime = resolveBitnetRuntime();
  const descriptor = buildWorkerDescriptor(python, runtime, config.model);
  const payload = await runPythonJson(
    descriptor,
    {
      mode: "generate",
      runtime,
      model: config.model,
      prompt: config.question,
      maxTokens: MAX_GENERATION_TOKENS
    },
    config.timeoutMs
  );

  if (!payload.ok) {
    throw new Error(payload.error);
  }

  const output = payload.output.trim();

  if (!output) {
    throw new Error("bitnet returned an empty response.");
  }

  return output;
}

export async function runBitnetTest(
  config: Omit<RuntimeConfig, "question">
): Promise<BitnetTestResult> {
  const lines = [`provider: bitnet`, `model: ${config.model}`];
  let pythonBin: string;

  try {
    pythonBin = await resolvePythonBin(process.env);
    lines.push(`python: ok (${pythonBin})`);
  } catch (error) {
    lines.push("python: failed");
    lines.push(
      `error: ${error instanceof Error ? error.message : "Unknown Python error."}`
    );
    return { ok: false, lines };
  }

  let runtime: BitnetRuntime;

  try {
    runtime = resolveBitnetRuntime();
    lines.push(`runtime: ${runtime}`);
  } catch (error) {
    lines.push("runtime: failed");
    lines.push(
      `error: ${error instanceof Error ? error.message : "Unsupported runtime."}`
    );
    return { ok: false, lines };
  }

  let payload: PythonResponse;
  try {
    const descriptor = buildWorkerDescriptor(pythonBin, runtime, config.model);
    payload = await runPythonJson(
      descriptor,
      {
        mode: "test",
        runtime,
        model: config.model,
        prompt: SELF_TEST_PROMPT,
        maxTokens: 16
      },
      config.timeoutMs
    );
  } catch (error) {
    lines.push("generate: failed");
    lines.push(
      `error: ${error instanceof Error ? error.message : "Unknown bitnet error."}`
    );
    return { ok: false, lines };
  }

  if (!payload.ok) {
    if (payload.backend) {
      lines.push(`backend: ${payload.backend}`);
    }

    const stage = payload.stage ?? "generate";
    lines.push(`${stage}: failed`);
    lines.push(`error: ${payload.error}`);
    return { ok: false, lines };
  }

  const response = payload.output.trim();
  lines.push(`backend: ${payload.backend}`);
  lines.push("model load: ok");
  lines.push("generate: ok");
  return {
    ok: true,
    lines,
    response,
    tokenCount: payload.tokenCount,
    generationMs: payload.generationMs,
    tokenPerSecond: payload.tokenPerSecond
  };
}

const PYTHON_RUNNER = String.raw`
import contextlib
import io
import json
import re
import socket
import sys
import time

SOCKET_PATH = sys.argv[1]
RUNTIME = sys.argv[2]
MODEL_NAME = sys.argv[3]
IDLE_MS = int(__import__("os").environ.get("DISTILL_BITNET_IDLE_MS", "600000"))
STATE = {
    "loaded": False,
    "model": None,
    "tokenizer": None,
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
        model, tokenizer = load(model_name)
    except Exception as exc:
        raise StageError("model load", str(exc)) from exc

    return model, tokenizer, generate

def run_mlx(model, tokenizer, generate, prompt, max_tokens):
    try:
        rendered_prompt = build_prompt(tokenizer, prompt)
        started_at = time.perf_counter()
        verbose_capture = io.StringIO()
        with contextlib.redirect_stdout(verbose_capture):
            output = generate(
                model,
                tokenizer,
                prompt=rendered_prompt,
                max_tokens=max_tokens,
                verbose=True if max_tokens <= 16 else False,
            )
        generation_ms = (time.perf_counter() - started_at) * 1000
        token_count = len(tokenizer.encode(output))
        verbose_text = verbose_capture.getvalue()
        matches = re.findall(r"([0-9]+(?:\.[0-9]+)?)\s*(?:tok/s|tokens/s)", verbose_text)
        token_per_second = float(matches[-1]) if matches else None
    except Exception as exc:
        raise StageError("generate", str(exc)) from exc

    return "mlx", output, token_count, generation_ms, token_per_second

def load_transformers(model_name):
    try:
        import torch
        from transformers import AutoModelForCausalLM, AutoTokenizer
    except Exception as exc:
        raise StageError(
            "dependencies",
            f"Missing dependencies torch/transformers: {exc}"
        ) from exc

    if not torch.cuda.is_available():
        raise StageError("dependencies", "CUDA/ROCm device not available.")

    backend = "rocm" if getattr(torch.version, "hip", None) else "cuda"

    try:
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

def run_transformers(model, tokenizer, backend, prompt, max_tokens):
    try:
        rendered_prompt = build_prompt(tokenizer, prompt)
        inputs = tokenizer(rendered_prompt, return_tensors="pt")
        inputs = {key: value.to(model.device) for key, value in inputs.items()}
        pad_token_id = tokenizer.eos_token_id or model.config.eos_token_id
        started_at = time.perf_counter()
        output_ids = model.generate(
            **inputs,
            max_new_tokens=max_tokens,
            do_sample=False,
            pad_token_id=pad_token_id,
        )
        generation_ms = (time.perf_counter() - started_at) * 1000
        new_tokens = output_ids[0][inputs["input_ids"].shape[1]:]
        token_count = int(new_tokens.shape[0])
        output = tokenizer.decode(new_tokens, skip_special_tokens=True).strip()
    except Exception as exc:
        raise StageError("generate", str(exc)) from exc

    return backend, output, token_count, generation_ms, None

def ensure_loaded():
    if STATE["loaded"]:
        return

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
        raise RuntimeError(f"Unsupported runtime: {RUNTIME}")

    STATE["loaded"] = True

def handle_request(request):
    mode = request.get("mode", "generate")
    try:
        if mode == "ping":
            return {
                "ok": True,
                "output": "",
                "python": sys.executable,
                "runtime": RUNTIME,
                "backend": STATE["backend"] or ("mlx" if RUNTIME == "mlx" else "cuda"),
            }

        if mode == "shutdown":
            return {
                "ok": True,
                "output": "shutdown",
                "python": sys.executable,
                "runtime": RUNTIME,
                "backend": STATE["backend"] or ("mlx" if RUNTIME == "mlx" else "cuda"),
            }

        ensure_loaded()
        prompt = request["prompt"]
        max_tokens = int(request.get("maxTokens", 80))

        if RUNTIME == "mlx":
            backend, output, token_count, generation_ms, token_per_second = run_mlx(
                STATE["model"],
                STATE["tokenizer"],
                STATE["generate"],
                prompt,
                max_tokens,
            )
        else:
            backend, output, token_count, generation_ms, token_per_second = run_transformers(
                STATE["model"],
                STATE["tokenizer"],
                STATE["backend"],
                prompt,
                max_tokens,
            )
    except StageError as exc:
        return {
            "ok": False,
            "error": str(exc),
            "stage": exc.stage,
            "python": sys.executable,
            "runtime": RUNTIME,
        }
    except Exception as exc:
        return {
            "ok": False,
            "error": str(exc),
            "stage": "generate",
            "python": sys.executable,
            "runtime": RUNTIME,
        }

    return {
        "ok": True,
        "output": output.strip(),
        "python": sys.executable,
        "runtime": RUNTIME,
        "backend": backend,
        "tokenCount": token_count,
        "generationMs": generation_ms,
        "tokenPerSecond": token_per_second,
    }

def main():
    try:
        __import__("os").unlink(SOCKET_PATH)
    except FileNotFoundError:
        pass

    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(SOCKET_PATH)
    server.listen()
    server.settimeout(1.0)
    last_activity = time.time()

    while True:
        if (time.time() - last_activity) * 1000 > IDLE_MS:
            break

        try:
            conn, _ = server.accept()
        except TimeoutError:
            continue

        with conn:
            data = b""
            while True:
                chunk = conn.recv(65536)
                if not chunk:
                    break
                data += chunk
                if b"\\n" in data:
                    break

            if not data:
                continue

            last_activity = time.time()
            request = json.loads(data.split(b"\\n", 1)[0].decode("utf8"))
            response = handle_request(request)
            conn.sendall((json.dumps(response) + "\\n").encode("utf8"))
            conn.shutdown(socket.SHUT_WR)

            if request.get("mode") == "shutdown":
                break

    server.close()
    try:
        __import__("os").unlink(SOCKET_PATH)
    except FileNotFoundError:
        pass

if __name__ == "__main__":
    main()
`;
