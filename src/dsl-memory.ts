import { createHash } from "node:crypto";
import { mkdir, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";

import { UsageError } from "./config";
import { resolveConfigPath } from "./user-config";

export type DslScope = "global" | "stack" | "project";
export type DslKind = "alias" | "macro" | "default";
export type DslStatus = "candidate" | "active" | "stale" | "pinned";

export interface DslEntry {
  key: string;
  meaning: string;
  kind: DslKind;
  scope: DslScope;
  status: DslStatus;
  createdAt: string;
  lastSeenAt: string;
  useCount: number;
  windowUseCount: number;
  expiresAt: string;
  builtin?: boolean;
}

export interface DslMemoryFile {
  version: 1;
  scope: DslScope;
  entries: DslEntry[];
  updatedAt: string;
}

export interface ResolvedDslScope {
  scope: DslScope;
  path: string;
  id?: string;
}

interface DslCommandContext {
  env: NodeJS.ProcessEnv;
  cwd: string;
  now?: Date;
  promotionReviewer?: (entries: DslEntry[]) => Promise<DslPromotionReview[]>;
  threadLearnReviewer?: (request: DslThreadLearnReviewRequest) => Promise<DslThreadLearnReview[]>;
  readStdin?: () => Promise<string>;
}

export interface DslPromotionReview {
  key: string;
  decision: "promote" | "keep" | "reject";
  targetScope: DslScope;
  reason: string;
}

export interface DslThreadLearnCandidate {
  key: string;
  meaning: string;
  kind: DslKind;
  scope: DslScope;
  occurrenceCount: number;
  source: "dict" | "phrase" | "command";
}

export interface DslThreadLearnReview {
  key: string;
  meaning: string;
  kind: DslKind;
  scope: DslScope;
  reason: string;
  confidence: number;
}

export interface DslThreadLearnReviewRequest {
  transcript: string;
  candidates: DslThreadLearnCandidate[];
  dslMemory: DslEntry[];
  scope: DslScope;
  stack?: string;
}

const DAY_MS = 24 * 60 * 60 * 1000;
const CANDIDATE_TTL_DAYS = 14;
const ACTIVE_WINDOW_DAYS = 30;
const STALE_GRACE_DAYS = 30;
const SCOPE_CAPS: Record<DslScope, number> = {
  global: 20,
  stack: 30,
  project: 50
};

const BUILTIN_ENTRIES: Array<Pick<DslEntry, "key" | "meaning" | "kind">> = [
  { key: "A", meaning: "authentication or authorization", kind: "alias" },
  { key: "B", meaning: "backend", kind: "alias" },
  { key: "F", meaning: "frontend", kind: "alias" },
  { key: "D", meaning: "database", kind: "alias" },
  { key: "E", meaning: "end-to-end tests", kind: "alias" },
  { key: "C", meaning: "configuration", kind: "alias" },
  { key: "O", meaning: "documentation", kind: "alias" },
  { key: "V", meaning: "environment", kind: "alias" },
  { key: "X", meaning: "dependencies", kind: "alias" },
  { key: "P", meaning: "permissions", kind: "alias" },
  { key: "U", meaning: "user interface", kind: "alias" },
  { key: "1", meaning: "add failing regression test first", kind: "macro" },
  { key: "2", meaning: "run relevant tests", kind: "macro" },
  { key: "3", meaning: "report summary, files, tests, and status", kind: "macro" },
  { key: "4", meaning: "review for bugs, regressions, security, and risks", kind: "macro" },
  { key: "5", meaning: "implement smallest safe fix", kind: "macro" },
  { key: "6", meaning: "validate with tests or checks", kind: "macro" },
  { key: "7", meaning: "commit and push changes", kind: "macro" },
  { key: "8", meaning: "create or update pull request", kind: "macro" },
  { key: "9", meaning: "release or publish flow", kind: "macro" },
  { key: "0", meaning: "exact raw output required", kind: "macro" },
  { key: "N1", meaning: "do not change frontend", kind: "default" },
  { key: "N2", meaning: "do not change backend", kind: "default" },
  { key: "N3", meaning: "do not change UI", kind: "default" },
  { key: "N4", meaning: "do not do broad refactors", kind: "default" },
  { key: "N5", meaning: "preserve unrelated user changes", kind: "default" },
  { key: "N6", meaning: "interactive or TUI command", kind: "default" }
];

function addDays(date: Date, days: number): Date {
  return new Date(date.getTime() + days * DAY_MS);
}

function iso(date: Date): string {
  return date.toISOString();
}

function resolveDslBaseDir(env: NodeJS.ProcessEnv): string {
  return path.join(path.dirname(resolveConfigPath(env)), "dsl");
}

function sanitizeStack(input: string): string {
  const value = input.trim().toLowerCase().replace(/[^a-z0-9._-]+/g, "-");

  if (!value) {
    throw new UsageError("Stack name cannot be empty.");
  }

  return value;
}

export function hashProjectPath(projectPath: string): string {
  return createHash("sha256").update(path.resolve(projectPath)).digest("hex").slice(0, 16);
}

function detectStack(cwd: string): string {
  const basename = path.basename(cwd).toLowerCase();

  if (basename.includes("distill")) {
    return "node";
  }

  return "generic";
}

export function resolveDslScopePath(
  env: NodeJS.ProcessEnv,
  scope: DslScope,
  cwd: string,
  stack?: string
): ResolvedDslScope {
  const baseDir = resolveDslBaseDir(env);

  if (scope === "global") {
    return { scope, path: path.join(baseDir, "global.json") };
  }

  if (scope === "stack") {
    const id = sanitizeStack(stack ?? detectStack(cwd));
    return { scope, id, path: path.join(baseDir, "stacks", `${id}.json`) };
  }

  const id = hashProjectPath(cwd);
  return { scope, id, path: path.join(baseDir, "projects", `${id}.json`) };
}

function emptyMemory(scope: DslScope, now: Date): DslMemoryFile {
  return {
    version: 1,
    scope,
    entries: [],
    updatedAt: iso(now)
  };
}

function normalizeKey(key: string): string {
  const normalized = key.trim().toUpperCase();

  if (!/^[A-Z0-9][A-Z0-9._-]{0,31}$/.test(normalized)) {
    throw new UsageError("DSL key must be 1-32 chars: A-Z, 0-9, ., _, or -.");
  }

  return normalized;
}

function parseKind(kind: string | undefined): DslKind {
  if (kind === "alias" || kind === "macro" || kind === "default") {
    return kind;
  }

  throw new UsageError("DSL kind must be alias, macro, or default.");
}

function parseScope(scope: string | undefined): DslScope {
  if (!scope) {
    return "project";
  }

  if (scope === "global" || scope === "stack" || scope === "project") {
    return scope;
  }

  throw new UsageError("DSL scope must be global, stack, or project.");
}

function containsSensitiveValue(input: string): boolean {
  return (
    /(api[-_ ]?key|password|secret|token|sk-[a-z0-9]|-----BEGIN)/i.test(input) ||
    /https?:\/\//i.test(input) ||
    /[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}/i.test(input) ||
    /(^|\s)\/Users\/|\b[A-Fa-f0-9]{32,}\b/.test(input) ||
    /\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/i.test(input)
  );
}

function parseFlags(args: string[]): {
  positional: string[];
  scope: DslScope;
  scopeProvided: boolean;
  stack?: string;
  stale: boolean;
  candidates: boolean;
  dryRun: boolean;
  stdin: boolean;
} {
  const positional: string[] = [];
  let scope: DslScope = "project";
  let scopeProvided = false;
  let stack: string | undefined;
  let stale = false;
  let candidates = false;
  let dryRun = false;
  let stdin = false;

  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index];

    if (arg === "--scope") {
      scope = parseScope(args[index + 1]);
      scopeProvided = true;
      index += 1;
      continue;
    }

    if (arg === "--stack") {
      stack = args[index + 1];
      if (!stack) {
        throw new UsageError("Missing value for --stack.");
      }
      index += 1;
      continue;
    }

    if (arg === "--stale") {
      stale = true;
      continue;
    }

    if (arg === "--candidates") {
      candidates = true;
      continue;
    }

    if (arg === "--dry-run") {
      dryRun = true;
      continue;
    }

    if (arg === "--stdin") {
      stdin = true;
      continue;
    }

    if (arg.startsWith("-")) {
      throw new UsageError(`Unknown DSL flag: ${arg}`);
    }

    positional.push(arg);
  }

  return { positional, scope, scopeProvided, stack, stale, candidates, dryRun, stdin };
}

function gcMemory(memory: DslMemoryFile, now: Date): { memory: DslMemoryFile; changed: boolean } {
  let changed = false;
  const nextEntries: DslEntry[] = [];

  for (const entry of memory.entries) {
    if (entry.builtin || entry.status === "pinned") {
      nextEntries.push(entry);
      continue;
    }

    const expiresAt = new Date(entry.expiresAt);

    if (entry.status === "candidate" && expiresAt <= now) {
      changed = true;
      continue;
    }

    if (entry.status === "active" && expiresAt <= now && entry.windowUseCount < 2) {
      nextEntries.push({
        ...entry,
        status: "stale",
        windowUseCount: 0,
        expiresAt: iso(addDays(now, STALE_GRACE_DAYS))
      });
      changed = true;
      continue;
    }

    if (entry.status === "active" && expiresAt <= now) {
      nextEntries.push({
        ...entry,
        windowUseCount: 0,
        expiresAt: iso(addDays(now, ACTIVE_WINDOW_DAYS))
      });
      changed = true;
      continue;
    }

    if (entry.status === "stale" && expiresAt <= now && entry.windowUseCount < 2) {
      changed = true;
      continue;
    }

    nextEntries.push(entry);
  }

  const cap = SCOPE_CAPS[memory.scope];
  const activeLearned = nextEntries.filter(
    (entry) => !entry.builtin && entry.status === "active"
  );

  if (activeLearned.length > cap) {
    const removeKeys = new Set(
      activeLearned
        .sort(
          (a, b) =>
            a.windowUseCount - b.windowUseCount ||
            a.useCount - b.useCount ||
            new Date(a.lastSeenAt).getTime() - new Date(b.lastSeenAt).getTime()
        )
        .slice(0, activeLearned.length - cap)
        .map((entry) => entry.key)
    );
    memory = {
      ...memory,
      entries: nextEntries.filter((entry) => !removeKeys.has(entry.key))
    };
    changed = true;
  } else {
    memory = { ...memory, entries: nextEntries };
  }

  return {
    memory: {
      ...memory,
      updatedAt: changed ? iso(now) : memory.updatedAt
    },
    changed
  };
}

async function readMemoryFile(
  resolved: ResolvedDslScope,
  now: Date,
  writeGc = true
): Promise<DslMemoryFile> {
  try {
    const raw = await readFile(resolved.path, "utf8");
    const parsed = JSON.parse(raw) as DslMemoryFile;
    const memory = {
      ...emptyMemory(resolved.scope, now),
      ...parsed,
      scope: resolved.scope,
      entries: Array.isArray(parsed.entries) ? parsed.entries : []
    };
    const gc = gcMemory(memory, now);

    if (gc.changed) {
      if (writeGc) {
        await writeMemoryFile(resolved.path, gc.memory);
      } else {
        return memory;
      }
    }

    return gc.memory;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return emptyMemory(resolved.scope, now);
    }

    throw error;
  }
}

async function writeMemoryFile(filePath: string, memory: DslMemoryFile): Promise<void> {
  await mkdir(path.dirname(filePath), { recursive: true, mode: 0o700 });
  await writeFile(filePath, `${JSON.stringify(memory, null, 2)}\n`, { mode: 0o600 });
}

async function readScopedMemory(
  env: NodeJS.ProcessEnv,
  scope: DslScope,
  cwd: string,
  stack: string | undefined,
  now: Date,
  writeGc = true
): Promise<{ resolved: ResolvedDslScope; memory: DslMemoryFile }> {
  const resolved = resolveDslScopePath(env, scope, cwd, stack);
  const memory = await readMemoryFile(resolved, now, writeGc);
  return { resolved, memory };
}

function createEntry(
  scope: DslScope,
  kind: DslKind,
  key: string,
  meaning: string,
  now: Date,
  status: DslStatus = "candidate",
  builtin = false
): DslEntry {
  return {
    key,
    meaning,
    kind,
    scope,
    status,
    createdAt: iso(now),
    lastSeenAt: iso(now),
    useCount: builtin ? 0 : 1,
    windowUseCount: builtin ? 0 : 1,
    expiresAt: iso(addDays(now, status === "candidate" ? CANDIDATE_TTL_DAYS : ACTIVE_WINDOW_DAYS)),
    builtin
  };
}

function shouldExposeInPrompt(entry: DslEntry): boolean {
  return entry.status === "active" || entry.status === "pinned";
}

export function formatPromptDslMemory(
  entries: DslEntry[],
  maxEntries: number
): string {
  const selected = entries
    .filter(shouldExposeInPrompt)
    .sort((a, b) => {
      const aBuiltin = a.builtin ? 1 : 0;
      const bBuiltin = b.builtin ? 1 : 0;
      const aPinned = a.status === "pinned" ? 0 : 1;
      const bPinned = b.status === "pinned" ? 0 : 1;
      return (
        aBuiltin - bBuiltin ||
        aPinned - bPinned ||
        b.useCount - a.useCount ||
        a.key.localeCompare(b.key)
      );
    })
    .slice(0, maxEntries);

  return selected
    .map((entry) => `${entry.key} = ${entry.meaning} (${entry.kind}, ${entry.scope})`)
    .join("\n");
}

function inferKind(key: string, meaning: string): DslKind {
  const normalized = meaning.toLowerCase();

  if (key.startsWith("NO-") || normalized.startsWith("do not ")) {
    return "default";
  }

  if (
    /\b(add|run|report|verify|check|fix|review|test|deploy|build)\b/.test(normalized)
  ) {
    return "macro";
  }

  return "alias";
}

function parseDictEntries(output: string): Array<{ key: string; meaning: string; kind: DslKind }> {
  const entries: Array<{ key: string; meaning: string; kind: DslKind }> = [];
  let inDictBlock = false;

  for (const rawLine of output.split(/\r?\n/)) {
    const line = rawLine.trim();

    if (!line) {
      inDictBlock = false;
      continue;
    }

    const inline = line.match(/^Dict\+?:\s*([A-Za-z0-9._-]{1,32})\s*=\s*(.+)$/);

    if (inline) {
      const key = normalizeKey(inline[1]);
      const meaning = inline[2].trim();
      entries.push({ key, meaning, kind: inferKind(key, meaning) });
      inDictBlock = true;
      continue;
    }

    if (/^Dict\+?:?\s*$/i.test(line)) {
      inDictBlock = true;
      continue;
    }

    if (!inDictBlock) {
      continue;
    }

    if (/^[A-Z][A-Za-z+ -]*:/.test(line) && !line.includes("=")) {
      inDictBlock = false;
      continue;
    }

    const block = line.match(/^-?\s*([A-Za-z0-9._-]{1,32})\s*=\s*(.+)$/);

    if (block) {
      const key = normalizeKey(block[1]);
      const meaning = block[2].trim();
      entries.push({ key, meaning, kind: inferKind(key, meaning) });
    }
  }

  return entries;
}

function isReusableLearnedEntry(key: string, meaning: string): boolean {
  const trimmed = meaning.trim();

  if (!trimmed || trimmed.length > 120 || key.length > 32) {
    return false;
  }

  if (containsSensitiveValue(`${key} ${trimmed}`)) {
    return false;
  }

  if (/^\d+$/.test(key) || /^[A-Z]$/.test(key)) {
    return false;
  }

  if (/[/\\]/.test(trimmed) || /\b\d{4,}\b/.test(trimmed)) {
    return false;
  }

  return true;
}

function keyBase(rawKey: string, meaning: string): string {
  const raw = rawKey.replace(/[^A-Z0-9]/g, "");

  if (raw) {
    return raw[0];
  }

  const firstMeaningChar = meaning.toUpperCase().match(/[A-Z0-9]/)?.[0];
  return firstMeaningChar ?? "Z";
}

function compactKeyCandidates(rawKey: string, meaning: string): string[] {
  const base = keyBase(rawKey, meaning);
  const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZ".split("");
  const digits = "1234567890".split("");
  const preferred = [
    base,
    ...digits.map((digit) => `${base}${digit}`),
    ...alphabet,
    ...digits,
    ...alphabet.flatMap((letter) => digits.map((digit) => `${letter}${digit}`))
  ];

  return [...new Set(preferred)].filter((candidate) =>
    /^[A-Z0-9][A-Z0-9._-]{0,31}$/.test(candidate)
  );
}

function compactLearnedEntries(
  entries: Array<{ key: string; meaning: string; kind: DslKind }>,
  existingEntries: DslEntry[]
): Array<{ key: string; meaning: string; kind: DslKind }> {
  const usedKeys = new Set(existingEntries.map((entry) => entry.key));
  const compacted: Array<{ key: string; meaning: string; kind: DslKind }> = [];

  for (const entry of entries) {
    const existingSameMeaning = existingEntries.find(
      (candidate) =>
        candidate.meaning.toLowerCase() === entry.meaning.toLowerCase() &&
        candidate.kind === entry.kind
    );

    if (existingSameMeaning) {
      compacted.push({ ...entry, key: existingSameMeaning.key });
      continue;
    }

    const key = compactKeyCandidates(entry.key, entry.meaning).find(
      (candidate) => !usedKeys.has(candidate)
    );

    if (!key) {
      continue;
    }

    usedKeys.add(key);
    compacted.push({ ...entry, key });
  }

  return compacted;
}

const THREAD_STOP_WORDS = new Set([
  "the",
  "and",
  "for",
  "with",
  "that",
  "this",
  "from",
  "into",
  "will",
  "must",
  "should",
  "when",
  "then",
  "than",
  "only",
  "using",
  "return",
  "output",
  "user",
  "thread",
  "please",
  "implement",
  "plan"
]);

function normalizeMeaning(input: string): string {
  return input
    .trim()
    .replace(/[`"'*_>#()[\]{}]/g, " ")
    .replace(/\s+/g, " ")
    .replace(/[.,;:!?]+$/g, "")
    .toLowerCase();
}

function escapeRegExp(input: string): string {
  return input.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function countMeaningOccurrences(transcript: string, meaning: string): number {
  const normalizedTranscript = normalizeMeaning(transcript);
  const normalizedMeaning = normalizeMeaning(meaning);

  if (!normalizedMeaning) {
    return 0;
  }

  return normalizedTranscript.match(new RegExp(`\\b${escapeRegExp(normalizedMeaning)}\\b`, "g"))?.length ?? 0;
}

function looksPrivateOrNoisy(input: string): boolean {
  return (
    containsSensitiveValue(input) ||
    /[/\\]/.test(input) ||
    /@\w/.test(input) ||
    /\b[a-f0-9]{12,}\b/i.test(input) ||
    /\b\d{4,}\b/.test(input) ||
    /\b(samuel|samuelfaj|fajreldines)\b/i.test(input) ||
    /\b[A-Z][a-z]+ [A-Z][a-z]+\b/.test(input)
  );
}

function isReusableThreadMeaning(meaning: string, occurrenceCount: number): boolean {
  const normalized = normalizeMeaning(meaning);

  if (occurrenceCount < 2 || normalized.length < 3 || normalized.length > 120) {
    return false;
  }

  if (looksPrivateOrNoisy(meaning)) {
    return false;
  }

  const words = normalized.split(/\s+/).filter(Boolean);

  if (words.length === 1 && words[0].length < 4) {
    return false;
  }

  if (words.every((word) => THREAD_STOP_WORDS.has(word))) {
    return false;
  }

  return true;
}

function candidateKeyFromMeaning(meaning: string): string {
  const normalized = normalizeMeaning(meaning);
  const important = normalized
    .split(/\s+/)
    .filter((word) => word && !THREAD_STOP_WORDS.has(word));

  return (important[0] ?? normalized)[0]?.toUpperCase() ?? "Z";
}

function addThreadCandidate(
  byMeaning: Map<string, DslThreadLearnCandidate>,
  scope: DslScope,
  kind: DslKind,
  meaning: string,
  occurrenceCount: number,
  source: DslThreadLearnCandidate["source"],
  rawKey?: string
): void {
  const normalized = normalizeMeaning(meaning);

  if (!isReusableThreadMeaning(normalized, occurrenceCount)) {
    return;
  }

  const existing = byMeaning.get(normalized);

  if (existing && existing.occurrenceCount >= occurrenceCount) {
    return;
  }

  byMeaning.set(normalized, {
    key: rawKey ? normalizeKey(rawKey) : candidateKeyFromMeaning(normalized),
    meaning: normalized,
    kind,
    scope,
    occurrenceCount,
    source
  });
}

function extractRepeatedLineCommands(transcript: string): Array<{ meaning: string; count: number }> {
  const counts = new Map<string, number>();

  for (const rawLine of transcript.split(/\r?\n/)) {
    const line = rawLine
      .trim()
      .replace(/^\$+\s*/, "")
      .replace(/^>\s*/, "");

    if (!line || line.length > 120) {
      continue;
    }

    if (!/\b(bun|npm|pnpm|yarn|git|gh|glab|terraform|distill|pytest|cargo|go|swift|xcodebuild)\b/.test(line)) {
      continue;
    }

    if (looksPrivateOrNoisy(line)) {
      continue;
    }

    const normalized = normalizeMeaning(line);
    counts.set(normalized, (counts.get(normalized) ?? 0) + 1);
  }

  return [...counts.entries()]
    .filter(([, count]) => count >= 2)
    .map(([meaning, count]) => ({ meaning, count }));
}

function extractRepeatedPhrases(transcript: string): Array<{ meaning: string; count: number }> {
  const normalized = normalizeMeaning(transcript);
  const words = normalized.split(/\s+/).filter((word) => /^[a-z][a-z0-9-]{2,}$/.test(word));
  const counts = new Map<string, number>();

  for (let size = 2; size <= 4; size += 1) {
    for (let index = 0; index <= words.length - size; index += 1) {
      const phraseWords = words.slice(index, index + size);

      if (phraseWords.every((word) => THREAD_STOP_WORDS.has(word))) {
        continue;
      }

      if (phraseWords.some((word) => word.length > 40)) {
        continue;
      }

      const phrase = phraseWords.join(" ");
      counts.set(phrase, (counts.get(phrase) ?? 0) + 1);
    }
  }

  return [...counts.entries()]
    .filter(([meaning, count]) => isReusableThreadMeaning(meaning, count))
    .sort((a, b) => b[1] - a[1] || a[0].length - b[0].length)
    .slice(0, 40)
    .map(([meaning, count]) => ({ meaning, count }));
}

export function extractThreadLearnCandidates(
  transcript: string,
  scope: DslScope
): DslThreadLearnCandidate[] {
  const byMeaning = new Map<string, DslThreadLearnCandidate>();

  for (const entry of parseDictEntries(transcript)) {
    const count = Math.max(
      countMeaningOccurrences(transcript, entry.meaning),
      countMeaningOccurrences(transcript, entry.key)
    );
    addThreadCandidate(byMeaning, scope, entry.kind, entry.meaning, count, "dict", entry.key);
  }

  for (const command of extractRepeatedLineCommands(transcript)) {
    addThreadCandidate(byMeaning, scope, "macro", command.meaning, command.count, "command");
  }

  for (const phrase of extractRepeatedPhrases(transcript)) {
    addThreadCandidate(
      byMeaning,
      scope,
      inferKind(candidateKeyFromMeaning(phrase.meaning), phrase.meaning),
      phrase.meaning,
      phrase.count,
      "phrase"
    );
  }

  return [...byMeaning.values()]
    .sort((a, b) => b.occurrenceCount - a.occurrenceCount || a.meaning.length - b.meaning.length)
    .slice(0, 30);
}

function deterministicThreadReviews(
  candidates: DslThreadLearnCandidate[]
): DslThreadLearnReview[] {
  return candidates.map((candidate) => ({
    key: candidate.key,
    meaning: candidate.meaning,
    kind: candidate.kind,
    scope: candidate.scope,
    reason: `${candidate.source} repeated ${candidate.occurrenceCount} times`,
    confidence: 0.7
  }));
}

function validThreadReview(
  review: DslThreadLearnReview,
  targetScope: DslScope
): DslThreadLearnReview | undefined {
  if (!["alias", "macro", "default"].includes(review.kind)) {
    return undefined;
  }

  if (!["project", "stack", "global"].includes(review.scope) || review.scope !== targetScope) {
    return undefined;
  }

  const confidence = Number(review.confidence);

  if (!Number.isFinite(confidence) || confidence < 0.65) {
    return undefined;
  }

  const key = normalizeKey(review.key);
  const meaning = normalizeMeaning(review.meaning);

  if (!isReusableThreadMeaning(meaning, 2)) {
    return undefined;
  }

  return {
    key,
    meaning,
    kind: review.kind,
    scope: review.scope,
    reason: String(review.reason ?? "").slice(0, 160),
    confidence
  };
}

export async function seedGlobalDslMemory(
  env: NodeJS.ProcessEnv,
  now: Date = new Date()
): Promise<void> {
  const { resolved, memory } = await readScopedMemory(env, "global", process.cwd(), undefined, now);
  const currentBuiltinKeys = new Set(BUILTIN_ENTRIES.map((entry) => entry.key));
  const byKey = new Map(
    memory.entries
      .filter((entry) => !entry.builtin || currentBuiltinKeys.has(entry.key))
      .map((entry) => [entry.key, entry])
  );

  for (const builtin of BUILTIN_ENTRIES) {
    byKey.set(
      builtin.key,
      createEntry("global", builtin.kind, builtin.key, builtin.meaning, now, "pinned", true)
    );
  }

  await writeMemoryFile(resolved.path, {
    ...memory,
    entries: [...byKey.values()].sort((a, b) => a.key.localeCompare(b.key)),
    updatedAt: iso(now)
  });
}

async function addEntry(
  env: NodeJS.ProcessEnv,
  cwd: string,
  scope: DslScope,
  stack: string | undefined,
  kind: DslKind,
  rawKey: string,
  meaning: string,
  now: Date
): Promise<string> {
  const key = normalizeKey(rawKey);

  if (!meaning.trim()) {
    throw new UsageError("DSL meaning is required.");
  }

  if (containsSensitiveValue(`${key} ${meaning}`)) {
    throw new UsageError("Refusing to persist sensitive DSL memory.");
  }

  const { resolved, memory } = await readScopedMemory(env, scope, cwd, stack, now);
  const existing = memory.entries.find((entry) => entry.key === key);

  if (!existing) {
    memory.entries.push(createEntry(scope, kind, key, meaning.trim(), now));
    await writeMemoryFile(resolved.path, { ...memory, updatedAt: iso(now) });
    return `candidate ${key} added to ${scope}\n`;
  }

  existing.meaning = meaning.trim();
  existing.kind = kind;
  existing.lastSeenAt = iso(now);
  existing.useCount += 1;
  existing.windowUseCount += 1;

  if (existing.status === "candidate" && existing.windowUseCount >= 2) {
    existing.status = "active";
    existing.windowUseCount = 0;
    existing.expiresAt = iso(addDays(now, ACTIVE_WINDOW_DAYS));
  } else if (existing.status === "stale" && existing.windowUseCount >= 2) {
    existing.status = "active";
    existing.windowUseCount = 0;
    existing.expiresAt = iso(addDays(now, ACTIVE_WINDOW_DAYS));
  }

  await writeMemoryFile(resolved.path, { ...memory, updatedAt: iso(now) });
  return `${existing.status} ${key} updated in ${scope}\n`;
}

export async function learnFromDistillOutput(
  env: NodeJS.ProcessEnv,
  cwd: string,
  output: string,
  options: {
    dryRun?: boolean;
    stack?: string;
    now?: Date;
  } = {}
): Promise<string> {
  const now = options.now ?? new Date();
  const parsedEntries = parseDictEntries(output).filter((entry) =>
    isReusableLearnedEntry(entry.key, entry.meaning)
  );
  const { memory } = await readScopedMemory(
    env,
    "project",
    cwd,
    options.stack,
    now
  );
  const uniqueEntries = new Map(
    compactLearnedEntries(parsedEntries, [
      ...builtinMemory(now).entries,
      ...memory.entries
    ]).map((entry) => [entry.key, entry])
  );

  if (uniqueEntries.size === 0) {
    return `${options.dryRun ? "would learn" : "learned"} 0 entries\n`;
  }

  if (options.dryRun) {
    return `would learn ${uniqueEntries.size} entries in project\n${[...uniqueEntries.values()]
      .map((entry) => `${entry.key}\t${entry.kind}\t${entry.meaning}`)
      .join("\n")}\n`;
  }

  const results: string[] = [];

  for (const entry of uniqueEntries.values()) {
    results.push(
      (
        await addEntry(
          env,
          cwd,
          "project",
          options.stack,
          entry.kind,
          entry.key,
          entry.meaning,
          now
        )
      ).trim()
    );
  }

  return `${results.join("\n")}\n`;
}

export async function learnFromThreadTranscript(
  env: NodeJS.ProcessEnv,
  cwd: string,
  transcript: string,
  options: {
    scope?: DslScope;
    dryRun?: boolean;
    stack?: string;
    now?: Date;
    reviewer?: (request: DslThreadLearnReviewRequest) => Promise<DslThreadLearnReview[]>;
  } = {}
): Promise<string> {
  const now = options.now ?? new Date();
  const scope = options.scope ?? "project";

  if (!transcript.trim()) {
    throw new UsageError("DSL learn-thread requires non-empty stdin.");
  }

  const extracted = extractThreadLearnCandidates(transcript, scope);
  const mergedMemory = await readMergedDslMemory(env, cwd, options.stack, now);
  const { memory: targetMemory } = await readScopedMemory(
    env,
    scope,
    cwd,
    options.stack,
    now
  );
  const allExisting = [...mergedMemory, ...targetMemory.entries];
  const compacted = compactLearnedEntries(extracted, allExisting).map((entry) => {
    const source = extracted.find((candidate) => candidate.meaning === entry.meaning);
    return {
      ...entry,
      scope,
      occurrenceCount: source?.occurrenceCount ?? 2,
      source: source?.source ?? "phrase"
    } satisfies DslThreadLearnCandidate;
  });

  if (compacted.length === 0) {
    return `${options.dryRun ? "would learn-thread" : "learn-thread"} 0 entries\n`;
  }

  let reviewed = deterministicThreadReviews(compacted);

  if (options.reviewer) {
    try {
      reviewed = await options.reviewer({
        transcript,
        candidates: compacted,
        dslMemory: mergedMemory,
        scope,
        stack: options.stack
      });
    } catch {
      reviewed = deterministicThreadReviews(compacted);
    }
  }

  const pinnedKeys = new Set(
    allExisting
      .filter((entry) => entry.status === "pinned" || entry.builtin)
      .map((entry) => entry.key)
  );
  const approvedByMeaning = new Map<string, DslThreadLearnReview>();

  for (const review of reviewed) {
    const valid = validThreadReview(review, scope);

    if (!valid || pinnedKeys.has(valid.key)) {
      continue;
    }

    if (!extracted.some((candidate) => candidate.meaning === valid.meaning)) {
      continue;
    }

    approvedByMeaning.set(valid.meaning, valid);
  }

  const approved = compactLearnedEntries(
    [...approvedByMeaning.values()].map((entry) => ({
      key: entry.key,
      meaning: entry.meaning,
      kind: entry.kind
    })),
    allExisting
  )
    .filter((entry) => !pinnedKeys.has(entry.key))
    .map((entry) => ({
      ...entry,
      scope,
      reason: approvedByMeaning.get(entry.meaning)?.reason ?? "accepted",
      confidence: approvedByMeaning.get(entry.meaning)?.confidence ?? 0.7
    }));

  if (approved.length === 0) {
    return `${options.dryRun ? "would learn-thread" : "learn-thread"} 0 entries\n`;
  }

  if (options.dryRun) {
    return `would learn-thread ${approved.length} entries in ${scope}\n${approved
      .map(
        (entry) =>
          `${entry.key}\t${entry.kind}\t${entry.scope}\t${entry.confidence.toFixed(2)}\t${entry.meaning}\t${entry.reason}`
      )
      .join("\n")}\n`;
  }

  const results: string[] = [];

  for (const entry of approved) {
    results.push(
      (
        await addEntry(
          env,
          cwd,
          scope,
          options.stack,
          entry.kind,
          entry.key,
          entry.meaning,
          now
        )
      ).trim()
    );
  }

  return `${results.join("\n")}\n`;
}

async function pinEntry(
  env: NodeJS.ProcessEnv,
  cwd: string,
  scope: DslScope,
  stack: string | undefined,
  rawKey: string,
  now: Date
): Promise<string> {
  const key = normalizeKey(rawKey);
  const { resolved, memory } = await readScopedMemory(env, scope, cwd, stack, now);
  const entry = memory.entries.find((candidate) => candidate.key === key);

  if (!entry) {
    throw new UsageError(`Unknown DSL key: ${key}`);
  }

  entry.status = "pinned";
  entry.expiresAt = iso(addDays(now, 3650));
  await writeMemoryFile(resolved.path, { ...memory, updatedAt: iso(now) });
  return `pinned ${key} in ${scope}\n`;
}

async function showMemory(
  env: NodeJS.ProcessEnv,
  cwd: string,
  scope: DslScope,
  stack: string | undefined,
  includeStale: boolean,
  now: Date
): Promise<string> {
  const { resolved, memory } = await readScopedMemory(env, scope, cwd, stack, now);
  const entries = memory.entries.filter(
    (entry) => includeStale || entry.status !== "stale"
  );
  const header = [`scope=${scope}`, resolved.id ? `id=${resolved.id}` : undefined]
    .filter(Boolean)
    .join(" ");

  if (entries.length === 0) {
    return `${header}\n(empty)\n`;
  }

  return `${header}\n${entries
    .sort((a, b) => a.key.localeCompare(b.key))
    .map(
      (entry) =>
        `${entry.key}\t${entry.kind}\t${entry.status}\t${entry.meaning}\tuses=${entry.useCount}\texpires=${entry.expiresAt}`
    )
    .join("\n")}\n`;
}

function builtinMemory(now: Date): DslMemoryFile {
  return {
    version: 1,
    scope: "global",
    entries: BUILTIN_ENTRIES.map((entry) =>
      createEntry("global", entry.kind, entry.key, entry.meaning, now, "pinned", true)
    ),
    updatedAt: iso(now)
  };
}

export async function readMergedDslMemory(
  env: NodeJS.ProcessEnv,
  cwd: string,
  stack: string | undefined,
  now: Date = new Date()
): Promise<DslEntry[]> {
  const merged = new Map<string, DslEntry>();
  const memories = [
    builtinMemory(now),
    (await readScopedMemory(env, "global", cwd, stack, now)).memory,
    (await readScopedMemory(env, "stack", cwd, stack, now)).memory,
    (await readScopedMemory(env, "project", cwd, stack, now)).memory
  ];

  for (const memory of memories) {
    for (const entry of memory.entries) {
      if (entry.status !== "stale") {
        merged.set(entry.key, entry);
      }
    }
  }

  return [...merged.values()].sort((a, b) => a.key.localeCompare(b.key));
}

async function showMergedMemory(
  env: NodeJS.ProcessEnv,
  cwd: string,
  stack: string | undefined,
  now: Date
): Promise<string> {
  const entries = await readMergedDslMemory(env, cwd, stack, now);

  if (entries.length === 0) {
    return "scope=merged\n(empty)\n";
  }

  return `scope=merged\n${entries
    .map(
      (entry) =>
        `${entry.key}\t${entry.kind}\t${entry.status}\t${entry.meaning}\tscope=${entry.scope}`
    )
    .join("\n")}\n`;
}

async function pruneMemory(
  env: NodeJS.ProcessEnv,
  cwd: string,
  scope: DslScope,
  stack: string | undefined,
  dryRun: boolean,
  now: Date
): Promise<string> {
  const { resolved, memory } = await readScopedMemory(env, scope, cwd, stack, now, false);
  const before = memory.entries.length;
  const gc = gcMemory(memory, now);
  const removed = before - gc.memory.entries.length;

  if (!dryRun) {
    await writeMemoryFile(resolved.path, gc.memory);
  }

  return `${dryRun ? "would prune" : "pruned"} ${removed} entries from ${scope}\n`;
}

async function resetMemory(
  env: NodeJS.ProcessEnv,
  cwd: string,
  scope: DslScope,
  stack: string | undefined
): Promise<string> {
  const resolved = resolveDslScopePath(env, scope, cwd, stack);
  await rm(resolved.path, { force: true });
  return `reset ${scope}\n`;
}

function deterministicPromotionReviews(entries: DslEntry[]): DslPromotionReview[] {
  return entries.map((entry) => ({
    key: entry.key,
    decision: entry.useCount >= 3 ? "promote" : "keep",
    targetScope: "stack",
    reason:
      entry.useCount >= 3
        ? "stable project shorthand with repeated use"
        : "needs more repeated use before promotion"
  }));
}

function parsePromotionReviews(
  entries: DslEntry[],
  raw: string
): DslPromotionReview[] {
  try {
    const parsed = JSON.parse(raw) as DslPromotionReview[];
    const allowedKeys = new Set(entries.map((entry) => entry.key));

    if (!Array.isArray(parsed)) {
      return deterministicPromotionReviews(entries);
    }

    return parsed
      .filter(
        (review) =>
          allowedKeys.has(review.key) &&
          ["promote", "keep", "reject"].includes(review.decision) &&
          ["project", "stack", "global"].includes(review.targetScope)
      )
      .map((review) => ({
        key: normalizeKey(review.key),
        decision: review.decision,
        targetScope: review.targetScope,
        reason: String(review.reason ?? "").slice(0, 160)
      }));
  } catch {
    return deterministicPromotionReviews(entries);
  }
}

async function promoteMemory(
  context: DslCommandContext,
  stack: string | undefined,
  dryRun: boolean,
  now: Date
): Promise<string> {
  const { memory } = await readScopedMemory(
    context.env,
    "project",
    context.cwd,
    stack,
    now
  );
  const candidates = memory.entries.filter(
    (entry) =>
      !entry.builtin &&
      entry.status === "active" &&
      entry.useCount >= 2 &&
      !containsSensitiveValue(`${entry.key} ${entry.meaning}`)
  );

  if (candidates.length === 0) {
    return `${dryRun ? "would promote" : "promoted"} 0 entries\n`;
  }

  let rawReviews = JSON.stringify(deterministicPromotionReviews(candidates));

  if (context.promotionReviewer) {
    try {
      rawReviews = JSON.stringify(await context.promotionReviewer(candidates));
    } catch {
      rawReviews = JSON.stringify(deterministicPromotionReviews(candidates));
    }
  }

  const reviews = parsePromotionReviews(candidates, rawReviews);
  const byKey = new Map(candidates.map((entry) => [entry.key, entry]));
  const approved = reviews.filter(
    (review) =>
      review.decision === "promote" &&
      (review.targetScope === "stack" || review.targetScope === "global")
  );

  if (dryRun) {
    return `would promote ${approved.length} entries\n${reviews
      .map(
        (review) =>
          `${review.key}\t${review.decision}\t${review.targetScope}\t${review.reason}`
      )
      .join("\n")}\n`;
  }

  for (const review of approved) {
    const entry = byKey.get(review.key);

    if (!entry) {
      continue;
    }

    const { resolved, memory: targetMemory } = await readScopedMemory(
      context.env,
      review.targetScope,
      context.cwd,
      stack,
      now
    );
    const existing = targetMemory.entries.find((candidate) => candidate.key === entry.key);

    if (existing) {
      existing.kind = entry.kind;
      existing.meaning = entry.meaning;
      existing.status = "active";
      existing.lastSeenAt = iso(now);
      existing.useCount += 1;
      existing.windowUseCount += 1;
      existing.expiresAt = iso(addDays(now, ACTIVE_WINDOW_DAYS));
    } else {
      targetMemory.entries.push(
        createEntry(
          review.targetScope,
          entry.kind,
          entry.key,
          entry.meaning,
          now,
          "active"
        )
      );
    }

    await writeMemoryFile(resolved.path, { ...targetMemory, updatedAt: iso(now) });
  }

  return `promoted ${approved.length} entries\n`;
}

export async function runDslCommand(
  args: string[],
  context: DslCommandContext
): Promise<string> {
  const now = context.now ?? new Date();
  const action = args[0];
  const parsed = parseFlags(args.slice(1));

  if (!action || action === "show") {
    if (!parsed.scopeProvided) {
      return showMergedMemory(context.env, context.cwd, parsed.stack, now);
    }

    return showMemory(
      context.env,
      context.cwd,
      parsed.scope,
      parsed.stack,
      parsed.stale,
      now
    );
  }

  if (action === "add") {
    const [kindArg, key, ...meaningParts] = parsed.positional;
    return addEntry(
      context.env,
      context.cwd,
      parsed.scope,
      parsed.stack,
      parseKind(kindArg),
      key ?? "",
      meaningParts.join(" "),
      now
    );
  }

  if (action === "pin") {
    return pinEntry(
      context.env,
      context.cwd,
      parsed.scope,
      parsed.stack,
      parsed.positional[0] ?? "",
      now
    );
  }

  if (action === "prune") {
    return pruneMemory(
      context.env,
      context.cwd,
      parsed.scope,
      parsed.stack,
      parsed.dryRun,
      now
    );
  }

  if (action === "learn") {
    const output = parsed.positional.join(" ");

    if (!output.trim()) {
      throw new UsageError("DSL learn requires distill output text.");
    }

    return learnFromDistillOutput(context.env, context.cwd, output, {
      dryRun: parsed.dryRun,
      stack: parsed.stack,
      now
    });
  }

  if (action === "learn-thread") {
    if (!parsed.stdin) {
      throw new UsageError("DSL learn-thread requires --stdin.");
    }

    if (!context.readStdin) {
      throw new UsageError("DSL learn-thread cannot read stdin in this context.");
    }

    return learnFromThreadTranscript(context.env, context.cwd, await context.readStdin(), {
      scope: parsed.scope,
      dryRun: parsed.dryRun,
      stack: parsed.stack,
      now,
      reviewer: context.threadLearnReviewer
    });
  }

  if (action === "promote") {
    return promoteMemory(context, parsed.stack, parsed.dryRun, now);
  }

  if (action === "reset") {
    return resetMemory(context.env, context.cwd, parsed.scope, parsed.stack);
  }

  throw new UsageError(
    "DSL command must be show, add, pin, learn, learn-thread, promote, prune, or reset."
  );
}
