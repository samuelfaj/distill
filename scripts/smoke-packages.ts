import { mkdtemp, rm } from "node:fs/promises";
import { readdirSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import cliPackage from "../packages/cli/package.json";

const root = path.resolve(import.meta.dir, "..");
const packDir = await mkdtemp(path.join(tmpdir(), "distill-pack-"));
const installDir = await mkdtemp(path.join(tmpdir(), "distill-install-"));

const currentPlatformPackage = (() => {
  const key = `${process.platform}-${process.arch}`;
  const mapping: Record<string, string> = {
    "darwin-arm64": "@sampurnamo/distill-darwin-arm64",
    "darwin-x64": "@sampurnamo/distill-darwin-x64",
    "linux-arm64": "@sampurnamo/distill-linux-arm64",
    "linux-x64": "@sampurnamo/distill-linux-x64",
    "win32-x64": "@sampurnamo/distill-win32-x64"
  };

  const value = mapping[key];

  if (!value) {
    throw new Error(`Unsupported smoke-pack target: ${key}`);
  }

  return value;
})();

function runOrThrow(command: string, args: string[], cwd: string, env?: NodeJS.ProcessEnv) {
  const result = spawnSync(command, args, {
    cwd,
    env: {
      ...process.env,
      ...env
    },
    encoding: "utf8"
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

try {
  runOrThrow("npm", ["pack", "--workspace", currentPlatformPackage, "--pack-destination", packDir], root);
  runOrThrow("npm", ["pack", "--workspace", "@sampurnamo/distill", "--pack-destination", packDir], root);
  runOrThrow("npm", ["init", "-y"], installDir);

  const tarballs = readdirSync(packDir)
    .sort()
    .map((entry) => path.join(packDir, entry));
  runOrThrow("npm", ["install", ...tarballs], installDir);

  const shimPath = path.join(installDir, "node_modules", ".bin", "distill");
  const versionOutput = runOrThrow(shimPath, ["--version"], installDir);

  if (versionOutput !== cliPackage.version) {
    throw new Error(`Unexpected version smoke output: ${versionOutput}`);
  }

  const fallbackProcess = spawnSync(shimPath, ["summarize briefly"], {
    cwd: installDir,
    env: {
      ...process.env,
      OLLAMA_HOST: "http://127.0.0.1:9"
    },
    encoding: "utf8",
    input: "fallback smoke\n"
  });

  if (fallbackProcess.error) {
    throw fallbackProcess.error;
  }

  if (fallbackProcess.status !== 0) {
    throw new Error(
      [fallbackProcess.stdout, fallbackProcess.stderr].filter(Boolean).join("\n") ||
        "distill fallback smoke failed"
    );
  }

  if (fallbackProcess.stdout.trim() !== "fallback smoke") {
    throw new Error(`Unexpected fallback smoke output: ${fallbackProcess.stdout.trim()}`);
  }
} finally {
  await rm(packDir, { recursive: true, force: true });
  await rm(installDir, { recursive: true, force: true });
}
