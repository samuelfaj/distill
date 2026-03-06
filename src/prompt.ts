export function buildBatchPrompt(question: string, input: string): string {
  return [
    "You compress command output for another paid language model.",
    "Rules:",
    "- Answer only what the question asks.",
    "- Use the same language as the question.",
    "- No markdown.",
    "- Keep the answer extremely short (but complete) unless explicitly asked to elaborate or not summarize.",
    "- Prefer one sentence. Never exceed three short lines.",
    "- Never ask for more input.",
    '- If the command output is insufficient, reply only with "distill: Insufficient information to output anything." in the same language as the question.',
    "- If the source is already shorter than your answer would be, prefer a minimal answer or reuse the source wording.",
    "",
    `Question: ${question}`,
    "",
    "Command output:",
    input
  ].join("\n");
}

export function buildWatchPrompt(
  question: string,
  previousCycle: string,
  currentCycle: string
): string {
  return [
    "You compare two consecutive watch-mode command cycles for another paid language model.",
    "Rules:",
    "- Answer only what the question asks.",
    "- Focus on what changed from the previous cycle to the current cycle.",
    "- Use the same language as the question.",
    "- No markdown.",
    "- Keep the answer extremely short (but complete) unless explicitly asked to elaborate or not summarize.",
    "- Prefer one sentence. Never exceed three short lines.",
    '- If nothing relevant changed, reply only with "No relevant change." in the same language as the question.',
    "- Never ask for more input.",
    "",
    `Question: ${question}`,
    "",
    "Previous cycle:",
    previousCycle,
    "",
    "Current cycle:",
    currentCycle
  ].join("\n");
}
