import { readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { PLATFORM_TARGETS } from "./platform-targets";

const root = path.resolve(import.meta.dir, "..");

for (const target of PLATFORM_TARGETS) {
  const packageJsonPath = path.join(root, target.packageManifestPath);
  const current = JSON.parse(
    await readFile(packageJsonPath, "utf8")
  ) as Record<string, unknown>;

	current.os = [...target.os];
	current.cpu = [...target.cpu];
	current.repository = {
		type: "git",
		url: "https://github.com/samuelfaj/distill"
	};

  await writeFile(packageJsonPath, `${JSON.stringify(current, null, 2)}\n`);
}
