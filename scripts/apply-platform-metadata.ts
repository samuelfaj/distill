import { readFile, writeFile } from "node:fs/promises";
import path from "node:path";

const root = path.resolve(import.meta.dir, "..");
const allTargets = [
  {
    key: "darwin-arm64",
    packagePath: "packages/distill-darwin-arm64/package.json",
    os: ["darwin"],
    cpu: ["arm64"]
  },
  {
    key: "darwin-x64",
    packagePath: "packages/distill-darwin-x64/package.json",
    os: ["darwin"],
    cpu: ["x64"]
  },
  {
    key: "linux-arm64",
    packagePath: "packages/distill-linux-arm64/package.json",
    os: ["linux"],
    cpu: ["arm64"]
  },
  {
    key: "linux-x64",
    packagePath: "packages/distill-linux-x64/package.json",
    os: ["linux"],
    cpu: ["x64"]
  },
  {
    key: "win32-x64",
    packagePath: "packages/distill-win32-x64/package.json",
    os: ["win32"],
    cpu: ["x64"]
  }
] as const;

const currentTargetKey = `${process.platform}-${process.arch}`;
const requestedTargets = process.env.DISTILL_TARGETS
  ?.split(",")
  .map((value) => value.trim())
  .filter(Boolean);
const shouldApplyAll = process.env.DISTILL_BUILD_ALL === "1";

const targets = requestedTargets?.length
  ? allTargets.filter((target) => requestedTargets.includes(target.key))
  : shouldApplyAll
    ? allTargets
    : allTargets.filter((target) => target.key === currentTargetKey);

for (const target of targets) {
  const packageJsonPath = path.join(root, target.packagePath);
  const current = JSON.parse(
    await readFile(packageJsonPath, "utf8")
  ) as Record<string, unknown>;

  current.os = [...target.os];
  current.cpu = [...target.cpu];

  await writeFile(packageJsonPath, `${JSON.stringify(current, null, 2)}\n`);
}
