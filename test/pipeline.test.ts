import { describe, expect, it } from "bun:test";
import { spawnSync } from "node:child_process";
import path from "node:path";

const root = path.resolve(import.meta.dir, "..");
const cli = path.join(root, "src", "cli.ts");
const bunExe = process.execPath;
const isWindows = process.platform === "win32";

describe("pipeline exit behavior", () => {
  it("mirrors the upstream exit with pipefail", () => {
    if (isWindows) {
      return;
    }

    const result = spawnSync(
      "bash",
      [
        "-lc",
        `set -o pipefail; (exit 7) | "${bunExe}" run "${cli}" "is this safe?" >/dev/null; printf "%s" $?`
      ],
      {
        cwd: root,
        encoding: "utf8"
      }
    );

    expect(result.stdout).toBe("7");
  });

  it("returns the distill exit without pipefail", () => {
    if (isWindows) {
      return;
    }

    const result = spawnSync(
      "bash",
      [
        "-lc",
        `(exit 7) | "${bunExe}" run "${cli}" "is this safe?" >/dev/null; printf "%s" $?`
      ],
      {
        cwd: root,
        encoding: "utf8"
      }
    );

    expect(result.stdout).toBe("0");
  });
});
