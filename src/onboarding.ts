import { cp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { createInterface } from "node:readline";
import { stdin as defaultInput, stdout as defaultOutput } from "node:process";

import { DEFAULT_HOST, DEFAULT_MODEL, DEFAULT_TIMEOUT_MS } from "./config";
import type { PersistedConfig } from "./config";
import { seedGlobalDslMemory } from "./dsl-memory";
import { writePersistedConfig } from "./user-config";

const AGENT_INSTRUCTIONS = [
  "<!-- distill skill: begin -->",
  "CRITICAL: Always communicate with the user in `/distill` compressed language in English: Military English + AR-0/AR-1, short command lines, explicit constraints, explicit pass criteria, and no filler. Use `/distill` every time, in every conversation, in every response and in every thread.",
  "Do not return a rewritten/compressed copy of the user's prompt. Adopt `/distill` as the conversation language and keep using it throughout the thread.",
  "Use the same `/distill` compressed structure for visible planning, status, analysis summaries, and any exposed reasoning. Keep hidden chain-of-thought private; never reveal it.",
  "Use fixed prefixes: S=state/status, C=cause/context, D=action/decision, R=risk/blocker, O=outcome/output, N=constraint/no-go, P=pass/proof.",
  "Every thread must use DSL/Dict when it helps compression. Start with `Dict:` when meanings are not already shared.",
  "Define short variables inline for repeated nouns: `workspace=#w3 version=#v1 patch=#p1 package=#p2`; after definition, use `#w3`, `#v1`, `#p1`, `#p2`.",
  "Variable keys are `#` plus letter+digit. Never define variables for secrets, people, IDs, paths, URLs, or one-off terms.",
  "Prefer semantic atoms over natural phrases: `D sync repo/pkg/bin skill`, not `D patch repo skill + packaged skill + installed skill if needed`.",
  "Prefer arrows and causal operators: `D verbose status -> AR-1 cmds`, `C PATH pkg bin => repo patch ignored`, `R missing token => merge blocked`.",
  "Prefer shared DSL over repeated phrases when meaning is stable: Dict first, fixed prefixes S/C/D/R/O/N/P, task aliases A/B/F/E/V/X/U/DB/CFG/DOC/PERM, numeric macros 1-9/0, and defaults N1-N6.",
  "Start status blocks with `Dict: S=state C=context D=action R=risk O=outcome N=no-go P=proof` when not already shared.",
  "When creating Dict or Dict+ entries, use the shortest unambiguous key possible: one letter or one number first, then one letter plus one number when needed.",
  "Create Dict/Dict+ only for terms that repeat or are likely to repeat. Do not grow DSL for rare, ambiguous, sensitive, or temporary terms.",
  "Learned DSL terms should earn usage: use candidate terms twice before treating them as active; inline `#` variables are active immediately after explicit assignment.",
  "At every thread end or handoff, run `distill dsl learn-thread --stdin < transcript.txt>` when transcript export is available.",
  "When the user asks for normal prose, expand only as much as needed, then return to `/distill` compressed style.",
  "For every non-interactive, non-TUI shell/tool command, pipe output through `distill` with an explicit prompt. Do not run plain shell commands when summarized output is enough.",
  "Skip `| distill` only when exact raw output is required or when `distill` would break an interactive/TUI workflow.",
  "Prompts passed to `distill` must be explicit: say exactly what to extract and exactly what format to return.",
  "Examples:",
  'bun test 2>&1 | distill "Did tests pass? Return PASS or FAIL, followed by failing test names if any."',
  'git diff | distill "What changed? Return only files changed and one-line summary for each."',
  'terraform plan 2>&1 | distill "Is this safe? Return SAFE, REVIEW, or UNSAFE, followed by risky changes."',
  "Wait for `distill` to finish before continuing.",
  "<!-- distill skill: end -->"
].join("\n");

interface OnboardingOptions {
  env: NodeJS.ProcessEnv;
  input?: NodeJS.ReadableStream;
  output?: NodeJS.WritableStream;
  persisted?: PersistedConfig;
}

function resolveHome(env: NodeJS.ProcessEnv): string {
  const home = env.HOME?.trim() || env.USERPROFILE?.trim();

  if (!home) {
    throw new Error("Could not resolve home directory for onboarding.");
  }

  return home;
}

function resolvePackageRoot(env: NodeJS.ProcessEnv): string {
  if (env.DISTILL_PACKAGE_ROOT?.trim()) {
    return env.DISTILL_PACKAGE_ROOT;
  }

  return path.resolve(import.meta.dir, "..");
}

function parseTimeout(input: string, fallback: number): number {
  const trimmed = input.trim();

  if (!trimmed) {
    return fallback;
  }

  const value = Number(trimmed);

  if (!Number.isFinite(value) || value <= 0) {
    throw new Error("timeout-ms must be a positive number.");
  }

  return Math.floor(value);
}

function parseInstallChoice(input: string): boolean {
  const normalized = input.trim().toLowerCase();

  if (!normalized) {
    return true;
  }

  return !["n", "no", "false", "0"].includes(normalized);
}

async function upsertInstructions(filePath: string): Promise<void> {
  await mkdir(path.dirname(filePath), { recursive: true });

  let current = "";

  try {
    current = await readFile(filePath, "utf8");
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") {
      throw error;
    }
  }

  const blockPattern =
    /(?:\n{0,2})?<!-- distill skill: begin -->[\s\S]*?<!-- distill skill: end -->(?:\n{0,2})?/;

  if (blockPattern.test(current)) {
    const next = current.replace(blockPattern, `\n\n${AGENT_INSTRUCTIONS}\n\n`).trim();
    await writeFile(filePath, `${next}\n`);
    return;
  }

  const prefix = current.trim().length > 0 ? `${current.trimEnd()}\n\n` : "";
  await writeFile(filePath, `${prefix}${AGENT_INSTRUCTIONS}\n`);
}

async function installSkill(env: NodeJS.ProcessEnv): Promise<void> {
  const home = resolveHome(env);
  const packageRoot = resolvePackageRoot(env);
  const codexSource = path.join(packageRoot, "skills", "distill");
  const claudeSource = path.join(packageRoot, ".claude", "skills", "distill");
  const codexTarget = path.join(home, ".codex", "skills", "distill");
  const claudeTarget = path.join(home, ".claude", "skills", "distill");

  await mkdir(path.dirname(codexTarget), { recursive: true });
  await mkdir(path.dirname(claudeTarget), { recursive: true });
  await rm(codexTarget, { recursive: true, force: true });
  await rm(claudeTarget, { recursive: true, force: true });
  await cp(codexSource, codexTarget, { recursive: true });
  await cp(claudeSource, claudeTarget, { recursive: true });
  await upsertInstructions(path.join(home, ".codex", "AGENTS.md"));
  await upsertInstructions(path.join(home, ".claude", "CLAUDE.md"));
}

export async function runOnboarding({
  env,
  input = defaultInput,
  output = defaultOutput,
  persisted = {}
}: OnboardingOptions): Promise<void> {
  const rl = createInterface({ input, crlfDelay: Infinity });
  const lines = rl[Symbol.asyncIterator]();
  const ask = async (query: string): Promise<string> => {
    output.write(query);
    const next = await lines.next();

    return next.done ? "" : String(next.value);
  };
  const currentHost = persisted.host ?? DEFAULT_HOST;
  const currentModel = persisted.model ?? DEFAULT_MODEL;
  const currentTimeout = persisted.timeoutMs ?? DEFAULT_TIMEOUT_MS;

  try {
    output.write("distill onboarding\n");
    const host =
      (await ask(`host [${currentHost}]: `)).trim() || currentHost;
    const model =
      (await ask(`model [${currentModel}]: `)).trim() || currentModel;
    const apiKey = await ask("api-key optional []: ");
    const timeoutMs = parseTimeout(
      await ask(`timeout-ms optional [${currentTimeout}]: `),
      currentTimeout
    );
    const shouldInstall = parseInstallChoice(
      await ask("install /distill skill for Codex and Claude? [Y/n]: ")
    );
    const config: PersistedConfig = {
      ...persisted,
      host,
      model,
      timeoutMs
    };

    if (apiKey.trim()) {
      config.apiKey = apiKey.trim();
    }

    await writePersistedConfig(env, config);
    await seedGlobalDslMemory(env);
    output.write("config saved\n");

    if (shouldInstall) {
      await installSkill(env);
      output.write("/distill skill installed for Codex and Claude\n");
      output.write("AGENTS.md and CLAUDE.md updated\n");
    } else {
      output.write("skill install skipped\n");
    }
  } finally {
    rl.close();
  }
}
