import { parseCommand, formatUsage, DISTILL_VERSION, UsageError } from "./config";
import { createOllamaSummarizer } from "./summarizer";
import { DistillSession, type ProgressPhase } from "./stream-distiller";
import {
  getPersistedConfigValue,
  readPersistedConfig,
  resolveConfigPath,
  setPersistedConfigValue
} from "./user-config";

async function run(): Promise<number> {
  const persisted = await readPersistedConfig(process.env);
  const command = parseCommand(process.argv.slice(2), process.env, persisted);

  if (command.kind === "help") {
    process.stdout.write(`${formatUsage()}\n`);
    return 0;
  }

  if (command.kind === "version") {
    process.stdout.write(`${DISTILL_VERSION}\n`);
    return 0;
  }

  if (command.kind === "configShow") {
    process.stdout.write(
      [
        `path=${resolveConfigPath(process.env)}`,
        `provider=${persisted.provider ?? ""}`,
        `model=${persisted.model ?? ""}`,
        `host=${persisted.host ?? ""}`,
        `api-key=${persisted.apiKey ? "***" : ""}`,
        `timeout-ms=${persisted.timeoutMs ?? ""}`,
        `thinking=${persisted.thinking ?? ""}`
      ].join("\n") + "\n"
    );
    return 0;
  }

  if (command.kind === "configGet") {
    const value = getPersistedConfigValue(persisted, command.key);
    process.stdout.write(`${value ?? ""}\n`);
    return 0;
  }

  if (command.kind === "configSet") {
    await setPersistedConfigValue(process.env, command.key, command.value);
    process.stdout.write(`${command.key}=${String(command.value)}\n`);
    return 0;
  }

  if (process.stdin.isTTY) {
    throw new UsageError("stdin is required.");
  }

  const progressProtocol = process.env.DISTILL_PROGRESS_PROTOCOL === "stderr";
  const progress =
    progressProtocol
      ? undefined
      : process.stderr.isTTY
        ? process.stderr
        : process.stdout.isTTY
          ? process.stdout
          : undefined;
  const emitProgressPhase = progressProtocol
    ? (phase: ProgressPhase) => {
        process.stderr.write(`__DISTILL_PROGRESS__:phase:${phase}\n`);
      }
    : undefined;
  const emitProgressStop = progressProtocol
    ? () => {
        process.stderr.write("__DISTILL_PROGRESS__:stop\n");
      }
    : undefined;
  const session = new DistillSession({
    summarizer: createOllamaSummarizer(command.config),
    stdout: process.stdout,
    isTTY: Boolean(process.stdout.isTTY),
    progress,
    onProgressPhase: emitProgressPhase,
    onProgressStop: emitProgressStop
  });

  await new Promise<void>((resolve, reject) => {
    process.stdin.on("data", (chunk) => {
      session.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
    });
    process.stdin.on("end", resolve);
    process.stdin.on("error", reject);
    process.stdin.resume();
  });

  await session.end();
  return 0;
}

run()
  .then((code) => {
    process.exit(code);
  })
  .catch((error) => {
    if (error instanceof UsageError) {
      process.stderr.write(`${error.message}\n\n${formatUsage()}\n`);
      process.exit(error.exitCode);
    }

    process.stderr.write(
      error instanceof Error ? `${error.message}\n` : "Unexpected error.\n"
    );
    process.exit(1);
  });
