import { mkdtemp, rm } from "node:fs/promises";
import { readdirSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import cliPackage from "../packages/cli/package.json";
import { getCurrentPlatformKey, getPlatformTarget } from "./platform-targets";

const root = path.resolve(import.meta.dir, "..");
const packDir = await mkdtemp(path.join(tmpdir(), "distill-pack-"));
const installDir = await mkdtemp(path.join(tmpdir(), "distill-install-"));
const npmCommand = process.platform === "win32" ? "npm.cmd" : "npm";

const currentPlatformPackage = (() => {
  const value = getPlatformTarget(getCurrentPlatformKey())?.packageName;

  if (!value) {
    throw new Error(`Unsupported smoke-pack target: ${getCurrentPlatformKey()}`);
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

function resolveInstalledShimPath(installDir: string): string {
  return path.join(
    installDir,
    "node_modules",
    ".bin",
    process.platform === "win32" ? "distill.cmd" : "distill"
  );
}

try {
  runOrThrow(npmCommand, ["pack", "--workspace", currentPlatformPackage, "--pack-destination", packDir], root);
  runOrThrow(npmCommand, ["pack", "--workspace", "@samuelfaj/distill", "--pack-destination", packDir], root);
  runOrThrow(npmCommand, ["init", "-y"], installDir);

  const tarballs = readdirSync(packDir)
    .sort()
    .map((entry) => path.join(packDir, entry));
  runOrThrow(npmCommand, ["install", ...tarballs], installDir);

  const shimPath = resolveInstalledShimPath(installDir);
  const versionOutput = runOrThrow(shimPath, ["--version"], installDir);

  if (versionOutput !== cliPackage.version) {
    throw new Error(`Unexpected version smoke output: ${versionOutput}`);
  }

  const fallbackProcess = spawnSync(shimPath, ["summarize briefly"], {
    cwd: installDir,
    env: {
      ...process.env,
      DISTILL_PROVIDER: "external",
      DISTILL_HOST: "http://127.0.0.1:9"
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
