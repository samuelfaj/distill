import { describe, expect, it } from "bun:test";

import { DistillSession } from "../src/stream-distiller";

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

function createWriter() {
  let value = "";

  return {
    write(chunk: string | Uint8Array) {
      value += typeof chunk === "string" ? chunk : Buffer.from(chunk).toString("utf8");
      return true;
    },
    read() {
      return value;
    }
  };
}

function createDelayedSummarizer(delayMs: number, response: string) {
  return {
    async summarizeBatch() {
      await sleep(delayMs);
      return response;
    },
    async summarizeWatch() {
      return "unused";
    }
  };
}

describe("DistillSession", () => {
  it("renders a batch summary", async () => {
    const writer = createWriter();
    const session = new DistillSession({
      stdout: writer,
      isTTY: false,
      idleMs: 10,
      interactiveGapMs: 5,
      summarizer: {
        summarizeBatch: async () => "All tests passed",
        summarizeWatch: async () => "unused"
      }
    });

    session.push(Buffer.from("test output\n"));
    await session.end();

    expect(writer.read()).toContain("All tests passed\n");
  });

  it("renders spinner progress and clears it before the final summary", async () => {
    const writer = createWriter();
    const progress = createWriter();
    const session = new DistillSession({
      stdout: writer,
      progress,
      isTTY: false,
      idleMs: 10,
      interactiveGapMs: 5,
      progressFrameMs: 10,
      summarizer: createDelayedSummarizer(50, "All tests passed")
    });

    await sleep(15);
    session.push(Buffer.from("test output\n"));
    await sleep(25);
    await session.end();

    expect(writer.read()).toContain("All tests passed\n");
    expect(progress.read()).toContain("distill: waiting");
    expect(progress.read()).toContain("distill: summarizing");
    expect(progress.read().endsWith("\r\u001b[2K")).toBe(true);
  });

  it("keeps output clean when progress is disabled", async () => {
    const writer = createWriter();
    const session = new DistillSession({
      stdout: writer,
      isTTY: false,
      idleMs: 10,
      interactiveGapMs: 5,
      progressFrameMs: 10,
      summarizer: createDelayedSummarizer(50, "All tests passed")
    });

    session.push(Buffer.from("test output\n"));
    await sleep(25);
    await session.end();

    expect(writer.read()).toContain("All tests passed\n");
  });

  it("falls back to the raw input when batch distillation is empty", async () => {
    const writer = createWriter();
    const session = new DistillSession({
      stdout: writer,
      isTTY: false,
      idleMs: 10,
      interactiveGapMs: 5,
      summarizer: {
        summarizeBatch: async () => "",
        summarizeWatch: async () => "unused"
      }
    });

    session.push(Buffer.from("raw payload\n"));
    await session.end();

    expect(writer.read()).toBe("raw payload\n");
  });

  it("switches to passthrough for interactive prompts", async () => {
    const writer = createWriter();
    let summarizeCalls = 0;
    const session = new DistillSession({
      stdout: writer,
      isTTY: false,
      idleMs: 50,
      interactiveGapMs: 10,
      summarizer: {
        summarizeBatch: async () => {
          summarizeCalls += 1;
          return "never";
        },
        summarizeWatch: async () => {
          summarizeCalls += 1;
          return "never";
        }
      }
    });

    session.push(Buffer.from("Continue? [y/N]"));
    await sleep(25);
    session.push(Buffer.from("\nyes\n"));
    await session.end();

    expect(writer.read()).toBe("Continue? [y/N]\nyes\n");
    expect(summarizeCalls).toBe(0);
  });

  it("promotes recurring bursts to watch mode", async () => {
    const writer = createWriter();
    let watchCalls = 0;
    const session = new DistillSession({
      stdout: writer,
      isTTY: false,
      idleMs: 15,
      interactiveGapMs: 5,
      summarizer: {
        summarizeBatch: async () => "unused",
        summarizeWatch: async () => {
          watchCalls += 1;
          return "failure count changed";
        }
      }
    });

    session.push(Buffer.from("watch run\nfailed: 0\n"));
    await sleep(25);
    session.push(Buffer.from("watch run\nfailed: 1\n"));
    await sleep(40);
    await session.end();

    expect(writer.read()).toBe("failure count changed\n");
    expect(watchCalls).toBe(1);
  });

  it("clears the terminal when rendering watch output on a tty", async () => {
    const writer = createWriter();
    const session = new DistillSession({
      stdout: writer,
      isTTY: true,
      idleMs: 15,
      interactiveGapMs: 5,
      summarizer: {
        summarizeBatch: async () => "unused",
        summarizeWatch: async () => "watch summary"
      }
    });

    session.push(Buffer.from("watch run\nfailed: 0\n"));
    await sleep(25);
    session.push(Buffer.from("watch run\nfailed: 1\n"));
    await sleep(40);
    await session.end();

    expect(writer.read()).toBe("\u001b[2J\u001b[Hwatch summary\n");
  });

  it("keeps ambiguous multi-burst output in batch mode", async () => {
    const writer = createWriter();
    let batchCalls = 0;
    const session = new DistillSession({
      stdout: writer,
      isTTY: false,
      idleMs: 10,
      interactiveGapMs: 5,
      summarizer: {
        summarizeBatch: async () => {
          batchCalls += 1;
          return "batch summary";
        },
        summarizeWatch: async () => "watch summary"
      }
    });

    session.push(Buffer.from("phase one\n"));
    await sleep(20);
    session.push(Buffer.from("totally different phase two\n"));
    await session.end();

    expect(writer.read()).toBe("batch summary\n");
    expect(batchCalls).toBe(1);
  });

  it("does not promote unrelated three-phase output to watch", async () => {
    const writer = createWriter();
    let batchCalls = 0;
    let watchCalls = 0;
    const session = new DistillSession({
      stdout: writer,
      isTTY: false,
      idleMs: 10,
      interactiveGapMs: 5,
      summarizer: {
        summarizeBatch: async () => {
          batchCalls += 1;
          return "batch summary";
        },
        summarizeWatch: async () => {
          watchCalls += 1;
          return "watch summary";
        }
      }
    });

    session.push(Buffer.from("alpha phase\n"));
    await sleep(20);
    session.push(Buffer.from("beta section\n"));
    await sleep(20);
    session.push(Buffer.from("gamma tail\n"));
    await session.end();

    expect(writer.read()).toBe("batch summary\n");
    expect(batchCalls).toBe(1);
    expect(watchCalls).toBe(0);
  });

  it("clears the progress line before switching to interactive passthrough", async () => {
    const writer = createWriter();
    const progress = createWriter();
    const session = new DistillSession({
      stdout: writer,
      progress,
      isTTY: false,
      idleMs: 50,
      interactiveGapMs: 12,
      progressFrameMs: 10,
      summarizer: {
        summarizeBatch: async () => "never",
        summarizeWatch: async () => "never"
      }
    });

    session.push(Buffer.from("Continue? [y/N]"));
    await sleep(35);
    session.push(Buffer.from("\nyes\n"));
    await session.end();

    expect(writer.read()).toBe("Continue? [y/N]\nyes\n");
    expect(progress.read()).toContain("distill: waiting");
    expect(progress.read().endsWith("\r\u001b[2K")).toBe(true);
  });
});
