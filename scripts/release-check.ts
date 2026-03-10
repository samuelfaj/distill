import { access, readFile } from "node:fs/promises";
import path from "node:path";

const root = path.resolve(import.meta.dir, "..");
const requirePublishMetadata = Bun.argv.includes("--publish");
const workspacePackages = [
  "packages/cli/package.json",
  "packages/distill-darwin-arm64/package.json",
  "packages/distill-darwin-x64/package.json",
  "packages/distill-linux-arm64/package.json",
  "packages/distill-linux-x64/package.json",
  "packages/distill-win32-x64/package.json"
];

const binaries = [
  "packages/distill-darwin-arm64/bin/distill",
  "packages/distill-darwin-x64/bin/distill",
  "packages/distill-linux-arm64/bin/distill",
  "packages/distill-linux-x64/bin/distill",
  "packages/distill-win32-x64/bin/distill.exe"
];

const manifests = await Promise.all(
  workspacePackages.map(async (relativePath) => {
    const content = await readFile(path.join(root, relativePath), "utf8");
    return JSON.parse(content) as { name: string; version: string };
  })
);

const versions = new Set(manifests.map((manifest) => manifest.version));

if (versions.size !== 1) {
  throw new Error("Workspace package versions are out of sync.");
}

for (const binary of binaries) {
  await access(path.join(root, binary));
}

const cliManifest = manifests[0];

if (cliManifest.name !== "@samuelfaj/distill") {
  throw new Error("Main package name must stay @samuelfaj/distill.");
}

if (requirePublishMetadata) {
  for (const manifest of manifests.slice(1) as Array<Record<string, unknown>>) {
    if (!Array.isArray(manifest.os) || !Array.isArray(manifest.cpu)) {
      throw new Error("Platform packages must include os/cpu metadata in publish mode.");
    }
  }
}
