export interface PromptMessages {
  system: string;
  user: string;
}

const SAFETY_BIAS = [
  "SAFETY:",
  "When the question asks to classify risk, safety, or destructiveness",
  "(SAFE/REVIEW/UNSAFE, OK/RISKY, DANGER, PASS/FAIL on policy, etc), bias",
  "toward the more dangerous label if there is any doubt. The reader will use",
  "your verdict to decide whether to run a command.",
  "- Treat as UNSAFE: any destroy, drop, delete, rm, force, truncate, replace,",
  "  terminate, kill, revoke, force-push, schema migration, data-loss",
  "  potential, irreversible operation, credential rotation, network exposure,",
  "  permission grant.",
  "- Treat as REVIEW: anything you cannot fully verify from the output alone,",
  "  partial output, ambiguous diffs, unknown side effects.",
  "- Output SAFE only when the output shows zero destructive or irreversible",
  "  operations.",
  "- Always list the exact risky lines verbatim after the verdict so the reader",
  "  can audit.",
  "- Never soften the verdict to please the reader."
].join(" ");

const COMMON_RULES = [
  "You compress shell or command output for another model that will act on",
  "your answer.",
  "Output ONLY the requested format. No preamble. No 'Here is'. No",
  "explanation unless the question asks for one.",
  "If the question asks for JSON, return raw JSON only, no fences.",
  "If the question asks for a list, one item per line, no bullets, no",
  "numbering.",
  "If the question asks PASS/FAIL/SAFE/REVIEW/UNSAFE, output that token first",
  "on the same line, then the supporting detail.",
  "Match the language of the question.",
  "Never invent data not present in the output. If a field is missing, omit it",
  "or say so explicitly.",
  'If the output is insufficient, reply only with "distill: Insufficient information to output anything." in the language of the question.',
  "If the source is already shorter than your answer would be, reuse the",
  "source wording.",
  "Keep prose answers to one sentence, max three short lines. Structured",
  "answers (JSON, lists, multi-line tables) may be longer when the format",
  "requires it.",
  "Never ask for more input.",
  SAFETY_BIAS
].join(" ");

const FEW_SHOT = [
  "Examples:",
  "",
  "Q: Which files are shown? Return only the filenames, one per line.",
  "Output:",
  "total 3",
  "-rw-r--r--  1 user staff  120 Jan 1 README.md",
  "drwxr-xr-x  3 user staff   96 Jan 1 src",
  "-rw-r--r--  1 user staff   42 Jan 1 .gitignore",
  "A:",
  "README.md",
  "src",
  ".gitignore",
  "",
  "Q: Did the tests pass? Return only PASS or FAIL, followed by failing test names if any.",
  "Output:",
  "PASS src/auth.test.ts",
  "FAIL src/queue.test.ts",
  "  expected 5, got 3",
  "1 passed, 1 failed.",
  "A:",
  "FAIL src/queue.test.ts",
  "",
  "Q: Extract the vulnerabilities. Return valid JSON only.",
  "Output:",
  "lodash <4.17.21 - CVE-2021-23337 High",
  "minimist <0.2.1 - CVE-2020-7598 Low",
  "A:",
  '[{"package":"lodash","version":"<4.17.21","cve":"CVE-2021-23337","severity":"high"},{"package":"minimist","version":"<0.2.1","cve":"CVE-2020-7598","severity":"low"}]',
  "",
  "Q: Is this safe? Return only SAFE, REVIEW, or UNSAFE, followed by the exact risky changes.",
  "Output:",
  "+ aws_instance.web (new)",
  "~ aws_security_group.default (update in-place)",
  "- aws_db_instance.old (destroy, forces replacement)",
  "~ aws_iam_role.app (update in-place)",
  "A:",
  "UNSAFE - aws_db_instance.old (destroy, forces replacement)",
  "",
  "Q: Is this safe to run? Return SAFE, REVIEW, or UNSAFE.",
  "Output:",
  "DROP TABLE users;",
  "A:",
  "UNSAFE DROP TABLE users;",
  "",
  "Q: Is this safe? Return SAFE, REVIEW, or UNSAFE.",
  "Output:",
  "git push origin main --force",
  "A:",
  "UNSAFE git push origin main --force",
  "",
  "Q: Did anything change? Return SAFE or REVIEW.",
  "Output:",
  "(no output)",
  "A:",
  "REVIEW empty output, cannot verify.",
  "",
  "Q: Did the build succeed? Return PASS or FAIL.",
  "Output:",
  "",
  "A:",
  "distill: Insufficient information to output anything.",
  "",
  "Q: Any pods not in Running status? Return PASS or FAIL with bad pods.",
  "Output:",
  "NAME       READY  STATUS              RESTARTS  AGE",
  "api-aa     2/2    Running             0         3h",
  "worker-xy  0/1    CrashLoopBackOff    17        1h",
  "db-0       1/1    Running             0         5d",
  "job-zz     0/1    Error               0         12m",
  "A:",
  "FAIL worker-xy CrashLoopBackOff, job-zz Error",
  "",
  "Q: Top 2 processes by memory. Return name and RSS only.",
  "Output:",
  "USER  PID  %CPU %MEM   RSS COMMAND",
  "root  123  0.5  6.2  1024 java",
  "root  456  2.1  4.0   680 postgres",
  "sam   789  0.1  2.0   340 node",
  "A:",
  "java 1024",
  "postgres 680"
].join("\n");

const MAX_INPUT_CHARS = 24000;

export function fitInput(input: string, maxChars: number = MAX_INPUT_CHARS): string {
  if (input.length <= maxChars) {
    return input;
  }

  const half = Math.floor(maxChars / 2) - 50;
  const head = input.slice(0, half);
  const tail = input.slice(-half);
  const dropped = input.length - head.length - tail.length;

  return `${head}\n... [${dropped} chars truncated] ...\n${tail}`;
}

export function buildBatchPrompt(question: string, input: string): PromptMessages {
  return {
    system: `${COMMON_RULES}\n\n${FEW_SHOT}`,
    user: `Command output:\n${fitInput(input)}\n\nQuestion: ${question}`
  };
}

export function buildWatchPrompt(
  question: string,
  previousCycle: string,
  currentCycle: string
): PromptMessages {
  const watchRules = [
    "You compare two consecutive watch-mode cycles for another model that will",
    "act on your answer.",
    "Focus on what changed from the previous cycle to the current cycle.",
    'If nothing relevant changed, reply only with "No relevant change." in the language of the question.',
    SAFETY_BIAS,
    "Other rules below still apply."
  ].join(" ");

  return {
    system: `${watchRules}\n\n${COMMON_RULES}\n\n${FEW_SHOT}`,
    user: [
      "Previous cycle:",
      fitInput(previousCycle),
      "",
      "Current cycle:",
      fitInput(currentCycle),
      "",
      `Question: ${question}`
    ].join("\n")
  };
}
