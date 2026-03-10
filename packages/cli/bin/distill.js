#!/usr/bin/env node

const { spawn } = require("node:child_process");
const path = require("node:path");
const { createRequire } = require("node:module");

const requireFromHere = createRequire(__filename);

const PACKAGE_BY_TARGET = {
  "darwin-arm64": "@samuelfaj/distill-darwin-arm64",
  "darwin-x64": "@samuelfaj/distill-darwin-x64",
  "linux-arm64": "@samuelfaj/distill-linux-arm64",
  "linux-x64": "@samuelfaj/distill-linux-x64",
  "win32-x64": "@samuelfaj/distill-win32-x64"
};

function resolveBinaryPath() {
  const target = `${process.platform}-${process.arch}`;
  const packageName = PACKAGE_BY_TARGET[target];

  if (!packageName) {
    console.error(
      `[distill] Unsupported platform: ${process.platform}/${process.arch}.`
    );
    process.exit(1);
  }

  try {
    const packageJsonPath = requireFromHere.resolve(`${packageName}/package.json`);
    const binaryName = process.platform === "win32" ? "distill.exe" : "distill";
    return path.join(path.dirname(packageJsonPath), "bin", binaryName);
  } catch (error) {
    console.error(
      `[distill] Missing platform package ${packageName}. Reinstall @samuelfaj/distill for this platform.`
    );
    process.exit(1);
  }
}

const PROGRESS_PREFIX = "__DISTILL_PROGRESS__:";
const PROGRESS_FRAMES = ["-", "\\", "|", "/"];
const PROGRESS_DOT_FRAMES = ["", ".", "..", "...", "..", "."];
const PROGRESS_LABELS = {
  collecting: "distill: waiting",
  summarizing: "distill: summarizing"
};

const binPath = resolveBinaryPath();
const progressWriter = process.stderr.isTTY ? process.stderr : process.stdout.isTTY ? process.stdout : null;
let progressPhase = "collecting";
let progressFrame = 0;
let progressTimer = null;
let progressVisible = false;
let childStderrBuffer = "";

function renderProgress() {
  if (!progressWriter) {
    return;
  }

  const frame = PROGRESS_FRAMES[progressFrame % PROGRESS_FRAMES.length];
  const dots =
    PROGRESS_DOT_FRAMES[
      Math.floor(progressFrame / PROGRESS_FRAMES.length) % PROGRESS_DOT_FRAMES.length
    ];
  progressFrame += 1;
  progressWriter.write(
    `\r\u001b[2K${frame} ${PROGRESS_LABELS[progressPhase] || PROGRESS_LABELS.collecting}${dots}`
  );
  progressVisible = true;
}

function startProgress() {
  if (!progressWriter || progressTimer) {
    return;
  }

  renderProgress();
  progressTimer = setInterval(renderProgress, 120);
}

function stopProgress() {
  if (progressTimer) {
    clearInterval(progressTimer);
    progressTimer = null;
  }

  if (progressVisible && progressWriter) {
    progressWriter.write("\r\u001b[2K");
    progressVisible = false;
  }
}

function handleChildStderrLine(line) {
  if (!line) {
    return;
  }

  if (!line.startsWith(PROGRESS_PREFIX)) {
    stopProgress();
    process.stderr.write(`${line}\n`);
    return;
  }

  if (line === `${PROGRESS_PREFIX}stop`) {
    stopProgress();
    return;
  }

  if (line.startsWith(`${PROGRESS_PREFIX}phase:`)) {
    progressPhase = line.slice(`${PROGRESS_PREFIX}phase:`.length) || "collecting";
    progressFrame = 0;
    renderProgress();
  }
}

function flushChildStderr(force = false) {
  if (!force && !childStderrBuffer.includes("\n")) {
    return;
  }

  const parts = childStderrBuffer.split("\n");
  childStderrBuffer = force ? "" : parts.pop() || "";

  for (const line of parts) {
    handleChildStderrLine(line);
  }

  if (force && childStderrBuffer) {
    handleChildStderrLine(childStderrBuffer);
    childStderrBuffer = "";
  }
}

const child = spawn(binPath, process.argv.slice(2), {
  stdio: ["inherit", "pipe", "pipe"],
  env: {
    ...process.env,
    DISTILL_PROGRESS_PROTOCOL: "stderr"
  }
});

startProgress();

child.stdout.on("data", (chunk) => {
  stopProgress();
  process.stdout.write(chunk);
});

child.stderr.on("data", (chunk) => {
  childStderrBuffer += chunk.toString("utf8");
  flushChildStderr();
});

const forwardSignal = (signal) => {
  if (!child.killed) {
    child.kill(signal);
  }
};

["SIGINT", "SIGTERM", "SIGHUP"].forEach((signal) => {
  process.on(signal, () => forwardSignal(signal));
});

child.on("error", (error) => {
  stopProgress();
  console.error(`[distill] Failed to launch native binary: ${error.message}`);
  process.exit(1);
});

child.on("exit", (code, signal) => {
  flushChildStderr(true);
  stopProgress();

  if (signal) {
    process.removeAllListeners(signal);
    process.kill(process.pid, signal);
    return;
  }

  process.exit(code ?? 1);
});
