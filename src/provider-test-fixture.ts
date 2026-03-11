import { existsSync, readFileSync } from "node:fs";
import path from "node:path";

import { buildBatchPrompt } from "./prompt";

export const DISTILL_TEST_QUESTION =
  "find where terminal and permission UI are implemented in chat screen";

function resolveRawFixturePath(): string | null {
  const override = process.env.DISTILL_TEST_RAW_PATH?.trim();

  if (override && existsSync(override)) {
    return override;
  }

  const candidates = [
    path.resolve(process.cwd(), "test", "raw.txt"),
    path.resolve(import.meta.dir, "..", "test", "raw.txt")
  ];

  for (const candidate of candidates) {
    if (existsSync(candidate)) {
      return candidate;
    }
  }

  return null;
}

function loadRawFixture(): string {
  const fixturePath = resolveRawFixturePath();

  if (!fixturePath) {
    return [
      "chat screen",
      "terminal panel",
      "permission dialog",
      "permission ui"
    ].join("\n");
  }

  return readFileSync(fixturePath, "utf8");
}

export const DISTILL_TEST_INPUT = loadRawFixture();
export const DISTILL_TEST_PROMPT = buildBatchPrompt(
  DISTILL_TEST_QUESTION,
  DISTILL_TEST_INPUT
);
export const DISTILL_TEST_DISPLAY_PROMPT = DISTILL_TEST_QUESTION;
