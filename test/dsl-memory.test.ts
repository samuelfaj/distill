import { describe, expect, it } from "bun:test";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

import {
  formatPromptDslMemory,
  hashProjectPath,
  learnFromThreadTranscript,
  learnFromDistillOutput,
  readMergedDslMemory,
  resolveDslScopePath,
  runDslCommand,
  seedGlobalDslMemory
} from "../src/dsl-memory";

function daysFromNow(days: number): Date {
  return new Date(Date.UTC(2026, 0, 1 + days));
}

async function withEnv<T>(fn: (env: NodeJS.ProcessEnv, cwd: string) => Promise<T>): Promise<T> {
  const dir = await mkdtemp(path.join(tmpdir(), "distill-dsl-"));
  const cwd = path.join(dir, "project");
  const env = {
    ...process.env,
    DISTILL_CONFIG_PATH: path.join(dir, "config.json")
  };

  try {
    return await fn(env, cwd);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

describe("dsl memory", () => {
  it("keeps persisted DSL empty by default", async () => {
    await withEnv(async (env, cwd) => {
      await seedGlobalDslMemory(env, daysFromNow(0));

      const global = await runDslCommand(["show", "--scope", "global"], {
        env,
        cwd,
        now: daysFromNow(0)
      });
      const project = await runDslCommand(["show", "--scope", "project"], {
        env,
        cwd,
        now: daysFromNow(0)
      });
      const merged = await runDslCommand(["show"], {
        env,
        cwd,
        now: daysFromNow(0)
      });

      expect(global).toContain("(empty)");
      expect(project).toContain("(empty)");
      expect(merged).toContain("(empty)");
      expect(global).not.toContain("\tpinned\t");
    });
  });

  it("promotes candidates after two uses within the promotion window", async () => {
    await withEnv(async (env, cwd) => {
      const first = await runDslCommand(
        ["add", "alias", "AUTH-FIX", "authentication bug fix", "--scope", "project"],
        { env, cwd, now: daysFromNow(0) }
      );
      const second = await runDslCommand(
        ["add", "alias", "AUTH-FIX", "authentication bug fix", "--scope", "project"],
        { env, cwd, now: daysFromNow(1) }
      );
      const output = await runDslCommand(["show", "--scope", "project"], {
        env,
        cwd,
        now: daysFromNow(1)
      });

      expect(first).toContain("candidate AUTH-FIX");
      expect(second).toContain("active AUTH-FIX");
      expect(output).toContain("AUTH-FIX\talias\tactive");
    });
  });

  it("learns reusable Dict+ entries from distill output into project candidates", async () => {
    await withEnv(async (env, cwd) => {
      const first = await learnFromDistillOutput(
        env,
        cwd,
        "Out: done\nDict+:\nAUTH = authentication fix\n",
        { now: daysFromNow(0) }
      );
      const second = await learnFromDistillOutput(
        env,
        cwd,
        "Out: done\nDict+: AUTH=authentication fix\n",
        { now: daysFromNow(1) }
      );
      const output = await runDslCommand(["show", "--scope", "project"], {
        env,
        cwd,
        now: daysFromNow(1)
      });

      expect(first).toContain("candidate A added to project");
      expect(second).toContain("active A updated in project");
      expect(output).toContain("A\tmacro\tactive\tauthentication fix");
    });
  });

  it("does not persist inline variable assignments from single distill output", async () => {
    await withEnv(async (env, cwd) => {
      const output = await learnFromDistillOutput(
        env,
        cwd,
        [
          "S cache=#c1 warmed model=#m1",
          "D inspect #c1 hit rate",
          "D compare #m1 latency"
        ].join("\n"),
        { now: daysFromNow(0) }
      );
      const memory = await runDslCommand(["show", "--scope", "project"], {
        env,
        cwd,
        now: daysFromNow(0)
      });

      expect(output).toContain("learned 0 entries");
      expect(memory).toContain("(empty)");
    });
  });

  it("does not learn sensitive, path-heavy, or value-like Dict+ entries", async () => {
    await withEnv(async (env, cwd) => {
      const output = await learnFromDistillOutput(
        env,
        cwd,
        [
          "Dict+:",
          "TOKEN = secret token value",
          "PATH = /Users/person/project/file.ts",
          "ID = 123456789",
          "path=#p1",
          "OK = stable meaning"
        ].join("\n"),
        { now: daysFromNow(0) }
      );
      const memory = await runDslCommand(["show", "--scope", "project"], {
        env,
        cwd,
        now: daysFromNow(0)
      });

      expect(output).toContain("candidate O added to project");
      expect(memory).toContain("O\talias\tcandidate\tstable meaning");
      expect(memory).not.toContain("TOKEN");
      expect(memory).not.toContain("PATH");
      expect(memory).not.toContain("ID");
      expect(memory).not.toContain("#p1");
    });
  });

  it("expires candidates, stales inactive entries, deletes stale entries, and preserves pinned entries", async () => {
    await withEnv(async (env, cwd) => {
      await runDslCommand(
        ["add", "macro", "RARE", "rare temporary action", "--scope", "project"],
        { env, cwd, now: daysFromNow(0) }
      );
      expect(
        await runDslCommand(["show", "--scope", "project"], {
          env,
          cwd,
          now: daysFromNow(15)
        })
      ).not.toContain("RARE");

      await runDslCommand(
        ["add", "macro", "KEEP", "keep repeated action", "--scope", "project"],
        { env, cwd, now: daysFromNow(0) }
      );
      await runDslCommand(
        ["add", "macro", "KEEP", "keep repeated action", "--scope", "project"],
        { env, cwd, now: daysFromNow(1) }
      );
      expect(
        await runDslCommand(["show", "--scope", "project", "--stale"], {
          env,
          cwd,
          now: daysFromNow(32)
        })
      ).toContain("KEEP\tmacro\tstale");
      expect(
        await runDslCommand(["show", "--scope", "project", "--stale"], {
          env,
          cwd,
          now: daysFromNow(63)
        })
      ).not.toContain("KEEP");

      await runDslCommand(
        ["add", "alias", "PINNED", "pinned meaning", "--scope", "project"],
        { env, cwd, now: daysFromNow(0) }
      );
      await runDslCommand(
        ["add", "alias", "PINNED", "pinned meaning", "--scope", "project"],
        { env, cwd, now: daysFromNow(1) }
      );
      await runDslCommand(["pin", "PINNED", "--scope", "project"], {
        env,
        cwd,
        now: daysFromNow(1)
      });
      expect(
        await runDslCommand(["show", "--scope", "project", "--stale"], {
          env,
          cwd,
          now: daysFromNow(365)
        })
      ).toContain("PINNED\talias\tpinned");
    });
  });

  it("merges global, stack, and project entries by nearest scope", async () => {
    await withEnv(async (env, cwd) => {
      await seedGlobalDslMemory(env, daysFromNow(0));
      await runDslCommand(["add", "alias", "APP", "global app", "--scope", "global"], {
        env,
        cwd,
        now: daysFromNow(0)
      });
      await runDslCommand(["add", "alias", "APP", "global app", "--scope", "global"], {
        env,
        cwd,
        now: daysFromNow(1)
      });
      await runDslCommand(
        ["add", "alias", "APP", "stack app", "--scope", "stack", "--stack", "node"],
        { env, cwd, now: daysFromNow(2) }
      );
      await runDslCommand(
        ["add", "alias", "APP", "stack app", "--scope", "stack", "--stack", "node"],
        { env, cwd, now: daysFromNow(3) }
      );
      await runDslCommand(["add", "alias", "APP", "project app", "--scope", "project"], {
        env,
        cwd,
        now: daysFromNow(4)
      });
      await runDslCommand(["add", "alias", "APP", "project app", "--scope", "project"], {
        env,
        cwd,
        now: daysFromNow(5)
      });

      const merged = await readMergedDslMemory(env, cwd, "node", daysFromNow(5));

      expect(merged.find((entry) => entry.key === "APP")?.meaning).toBe("project app");
      expect(merged.find((entry) => entry.key === "B")).toBeUndefined();
    });
  });

  it("formats prompt memory with only pinned and active learned entries", async () => {
    await withEnv(async (env, cwd) => {
      await seedGlobalDslMemory(env, daysFromNow(0));
      await runDslCommand(["add", "alias", "AUTH", "authentication fix"], {
        env,
        cwd,
        now: daysFromNow(0)
      });
      await runDslCommand(["add", "alias", "AUTH", "authentication fix"], {
        env,
        cwd,
        now: daysFromNow(1)
      });
      await runDslCommand(["add", "alias", "TEMP", "temporary candidate"], {
        env,
        cwd,
        now: daysFromNow(1)
      });

      const formatted = formatPromptDslMemory(
        await readMergedDslMemory(env, cwd, undefined, daysFromNow(1)),
        13
      );

      expect(formatted).toContain("AUTH = authentication fix");
      expect(formatted).not.toContain("B = backend");
      expect(formatted).not.toContain("S = state");
      expect(formatted).not.toContain("TEMP");
      expect(formatted.split("\n")).toHaveLength(1);
    });
  });

  it("promotes active project entries to stack via dry-run and apply", async () => {
    await withEnv(async (env, cwd) => {
      for (let index = 0; index < 3; index += 1) {
        await runDslCommand(["add", "alias", "AUTH", "authentication fix"], {
          env,
          cwd,
          now: daysFromNow(index)
        });
      }

      const dryRun = await runDslCommand(["promote", "--dry-run"], {
        env,
        cwd,
        now: daysFromNow(3)
      });
      const apply = await runDslCommand(["promote"], {
        env,
        cwd,
        now: daysFromNow(3)
      });
      const stack = await runDslCommand(["show", "--scope", "stack"], {
        env,
        cwd,
        now: daysFromNow(3)
      });

      expect(dryRun).toContain("AUTH\tpromote\tstack");
      expect(apply).toContain("promoted 1 entries");
      expect(stack).toContain("AUTH\talias\tactive\tauthentication fix");
    });
  });

  it("resolves stable project paths and rejects sensitive values", async () => {
    await withEnv(async (env, cwd) => {
      const first = hashProjectPath(cwd);
      const second = hashProjectPath(cwd);
      const resolved = resolveDslScopePath(env, "project", cwd);

      expect(first).toBe(second);
      expect(resolved.path).toContain(first);
      await expect(
        runDslCommand(["add", "alias", "TOKEN", "secret token value", "--scope", "project"], {
          env,
          cwd,
          now: daysFromNow(0)
        })
      ).rejects.toThrow("Refusing to persist sensitive DSL memory.");
    });
  });

  it("prunes and resets scoped memory", async () => {
    await withEnv(async (env, cwd) => {
      await runDslCommand(["add", "alias", "TEMP", "temporary value", "--scope", "project"], {
        env,
        cwd,
        now: daysFromNow(0)
      });

      expect(
        await runDslCommand(["prune", "--scope", "project", "--dry-run"], {
          env,
          cwd,
          now: daysFromNow(20)
        })
      ).toContain("would prune 1 entries");
      expect(
        await runDslCommand(["prune", "--scope", "project"], {
          env,
          cwd,
          now: daysFromNow(20)
        })
      ).toContain("pruned 1 entries");

      await runDslCommand(["add", "alias", "TEMP", "temporary value", "--scope", "project"], {
        env,
        cwd,
        now: daysFromNow(21)
      });
      await runDslCommand(["reset", "--scope", "project"], {
        env,
        cwd,
        now: daysFromNow(21)
      });

      const resolved = resolveDslScopePath(env, "project", cwd);
      await expect(readFile(resolved.path, "utf8")).rejects.toThrow();
    });
  });

  it("caps active learned global entries by removing lowest-use entries", async () => {
    await withEnv(async (env, cwd) => {
      for (let index = 0; index < 22; index += 1) {
        const key = `TERM${index}`;
        await runDslCommand(["add", "alias", key, `meaning ${index}`, "--scope", "global"], {
          env,
          cwd,
          now: daysFromNow(index)
        });
        await runDslCommand(["add", "alias", key, `meaning ${index}`, "--scope", "global"], {
          env,
          cwd,
          now: daysFromNow(index + 0.1)
        });
      }

      const output = await runDslCommand(["show", "--scope", "global"], {
        env,
        cwd,
        now: daysFromNow(30)
      });
      const activeLearned = output
        .split("\n")
        .filter((line) => /^TERM\d+\talias\tactive/.test(line));

      expect(activeLearned).toHaveLength(20);
      expect(output).not.toContain("TERM0\talias\tactive");
      expect(output).not.toContain("TERM1\talias\tactive");
    });
  });

  it("dry-runs explicit inline variable promotion after more than five thread uses", async () => {
    await withEnv(async (env, cwd) => {
      const transcript = [
        "S cache=#c1 prepared",
        "D inspect #c1",
        "D warm #c1",
        "D compare #c1",
        "D reuse #c1",
        "D keep #c1"
      ].join("\n");
      const output = await learnFromThreadTranscript(env, cwd, transcript, {
        dryRun: true,
        now: daysFromNow(0)
      });

      expect(output).toContain("would learn-thread 1 entries in project");
      expect(output).toContain("#c1\talias\tproject\t0.70\tcache");
      expect(
        await runDslCommand(["show", "--scope", "project"], {
          env,
          cwd,
          now: daysFromNow(0)
        })
      ).toContain("(empty)");
    });
  });

  it("persists explicit inline variables used more than five times in one thread", async () => {
    await withEnv(async (env, cwd) => {
      const transcript = [
        "S cache=#c1 prepared",
        "D inspect #c1",
        "D warm #c1",
        "D compare #c1",
        "D reuse #c1",
        "D keep #c1"
      ].join("\n");
      const result = await learnFromThreadTranscript(env, cwd, transcript, {
        now: daysFromNow(0)
      });
      const output = await runDslCommand(["show", "--scope", "project"], {
        env,
        cwd,
        now: daysFromNow(0)
      });

      expect(result).toContain("active #c1 added to project");
      expect(output).toContain("#c1\talias\tactive\tcache");
    });
  });

  it("does not persist explicit inline variables used only five times", async () => {
    await withEnv(async (env, cwd) => {
      const transcript = [
        "S cache=#c1 prepared",
        "D inspect #c1",
        "D warm #c1",
        "D compare #c1",
        "D reuse #c1"
      ].join("\n");
      const result = await learnFromThreadTranscript(env, cwd, transcript, {
        now: daysFromNow(0)
      });
      const output = await runDslCommand(["show", "--scope", "project"], {
        env,
        cwd,
        now: daysFromNow(0)
      });

      expect(result).toContain("learn-thread 0 entries");
      expect(output).toContain("(empty)");
    });
  });

  it("does not persist repeated phrases without explicit inline variables", async () => {
    await withEnv(async (env, cwd) => {
      const transcript = [
        "release flow",
        "release flow",
        "release flow",
        "release flow",
        "release flow",
        "release flow"
      ].join("\n");
      const result = await learnFromThreadTranscript(env, cwd, transcript, {
        now: daysFromNow(0)
      });
      const output = await runDslCommand(["show", "--scope", "project"], {
        env,
        cwd,
        now: daysFromNow(0)
      });

      expect(result).toContain("learn-thread 0 entries");
      expect(output).toContain("(empty)");
    });
  });

  it("rejects sensitive thread candidates even when the reviewer approves them", async () => {
    await withEnv(async (env, cwd) => {
      const transcript = [
        "token sk-1234567890abcdef repeated",
        "token sk-1234567890abcdef repeated",
        "https://example.com/path",
        "https://example.com/path"
      ].join("\n");
      const output = await learnFromThreadTranscript(env, cwd, transcript, {
        now: daysFromNow(0)
      });

      expect(output).toContain("learn-thread 0 entries");
      expect(
        await runDslCommand(["show", "--scope", "project"], {
          env,
          cwd,
          now: daysFromNow(0)
        })
      ).toContain("(empty)");
    });
  });

  it("evicts learned entries missing from the next thread", async () => {
    await withEnv(async (env, cwd) => {
      await learnFromThreadTranscript(
        env,
        cwd,
        [
          "S cache=#c1 prepared",
          "D inspect #c1",
          "D warm #c1",
          "D compare #c1",
          "D reuse #c1",
          "D keep #c1"
        ].join("\n"),
        { now: daysFromNow(0) }
      );

      const evicted = await learnFromThreadTranscript(
        env,
        cwd,
        "S unrelated thread only\nD no relevant mention",
        { now: daysFromNow(1) }
      );
      const output = await runDslCommand(["show", "--scope", "project"], {
        env,
        cwd,
        now: daysFromNow(1)
      });

      expect(evicted).toContain("evicted 1 entries");
      expect(output).toContain("(empty)");
    });
  });

  it("keeps learned entries used by key or meaning in the next thread", async () => {
    await withEnv(async (env, cwd) => {
      await learnFromThreadTranscript(
        env,
        cwd,
        [
          "S cache=#c1 prepared",
          "D inspect #c1",
          "D warm #c1",
          "D compare #c1",
          "D reuse #c1",
          "D keep #c1"
        ].join("\n"),
        { now: daysFromNow(0) }
      );

      await learnFromThreadTranscript(env, cwd, "S cache still relevant", {
        now: daysFromNow(1)
      });
      const keptByMeaning = await runDslCommand(["show", "--scope", "project"], {
        env,
        cwd,
        now: daysFromNow(1)
      });

      expect(keptByMeaning).toContain("#c1\talias\tactive\tcache");

      await learnFromThreadTranscript(env, cwd, "D use #c1 again", {
        now: daysFromNow(2)
      });
      const keptByKey = await runDslCommand(["show", "--scope", "project"], {
        env,
        cwd,
        now: daysFromNow(2)
      });

      expect(keptByKey).toContain("#c1\talias\tactive\tcache");
    });
  });

  it("does not overwrite pinned entries during thread learning", async () => {
    await withEnv(async (env, cwd) => {
      await runDslCommand(["add", "alias", "Z", "pinned meaning", "--scope", "project"], {
        env,
        cwd,
        now: daysFromNow(0)
      });
      await runDslCommand(["pin", "Z", "--scope", "project"], {
        env,
        cwd,
        now: daysFromNow(0)
      });

      const output = await learnFromThreadTranscript(
        env,
        cwd,
        "release flow release flow release flow",
        {
          now: daysFromNow(1),
          reviewer: async () => [
            {
              key: "Z",
              meaning: "release flow",
              kind: "macro",
              scope: "project",
              reason: "would overwrite pinned key",
              confidence: 0.95
            }
          ]
        }
      );
      const memory = await runDslCommand(["show", "--scope", "project"], {
        env,
        cwd,
        now: daysFromNow(1)
      });

      expect(output).toContain("learn-thread 0 entries");
      expect(memory).toContain("Z\talias\tpinned\tpinned meaning");
      expect(memory).not.toContain("release flow");
    });
  });
});
