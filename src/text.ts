const ANSI_PATTERN =
  // eslint-disable-next-line no-control-regex
  /\u001B(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~])/g;

const PROMPT_PATTERN =
  /(?:\[[Yy]\/[Nn]\]|\[[Nn]\/[Yy]\]|\([Yy]\/[Nn]\)|\([Nn]\/[Yy]\)|password:|passphrase:|continue\?|proceed\?)\s*$/i;

export function normalizeForModel(input: string): string {
  return input
    .replace(/\r\n/g, "\n")
    .replace(/\r/g, "\n")
    .replace(ANSI_PATTERN, "")
    .replace(/[ \t]+\n/g, "\n")
    .replace(/\n{3,}/g, "\n\n")
    .trim();
}

export function hasPromptLikeTail(input: string): boolean {
  const tail = input.slice(-256);
  return PROMPT_PATTERN.test(tail.trimEnd());
}

export function hasRedrawSignal(input: string): boolean {
  return input.includes("\r") || input.includes("\u001b[2J") || input.includes("\u001bc");
}

function structuralSignature(input: string): string[] {
  return normalizeForModel(input)
    .split("\n")
    .map((line) =>
      line
        .toLowerCase()
        .replace(/\b\d+\b/g, "#")
        .replace(/[0-9a-f]{7,}/g, "<hex>")
        .replace(/\s+/g, " ")
        .trim()
    )
    .filter(Boolean)
    .slice(0, 24);
}

export function structuralSimilarity(a: string, b: string): number {
  const left = structuralSignature(a);
  const right = structuralSignature(b);

  if (left.length === 0 || right.length === 0) {
    return 0;
  }

  const leftSet = new Set(left);
  const rightSet = new Set(right);
  let overlap = 0;

  for (const value of leftSet) {
    if (rightSet.has(value)) {
      overlap += 1;
    }
  }

  return (2 * overlap) / (leftSet.size + rightSet.size);
}

export function looksLikeBadDistillation(
  source: string,
  candidate: string
): boolean {
  const normalizedSource = normalizeForModel(source);
  const normalizedCandidate = normalizeForModel(candidate);

  if (!normalizedCandidate) {
    return true;
  }

  const lowerCandidate = normalizedCandidate.toLowerCase();

  if (
    lowerCandidate.includes("please provide") ||
    lowerCandidate.includes("wish summarized") ||
    lowerCandidate.includes("provided command output")
  ) {
    return true;
  }

  if (normalizedSource.length >= 1024) {
    return normalizedCandidate.length >= normalizedSource.length * 0.8;
  }

  if (normalizedSource.length > 0) {
    if (normalizedCandidate === normalizedSource) {
      return true;
    }

    const trimmedCandidate = normalizedCandidate.trim();
    const looksStructured =
      trimmedCandidate.startsWith("[") ||
      trimmedCandidate.startsWith("{") ||
      trimmedCandidate.startsWith("```");

    if (looksStructured) {
      return false;
    }

    return normalizedCandidate.length > normalizedSource.length + 40;
  }

  return false;
}

export function ensureTrailingNewline(text: string): string {
  return text.endsWith("\n") ? text : `${text}\n`;
}
