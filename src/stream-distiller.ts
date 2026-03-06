import {
  DEFAULT_IDLE_MS,
  DEFAULT_INTERACTIVE_GAP_MS,
  DEFAULT_PROGRESS_FRAME_MS
} from "./config";
import type { Summarizer } from "./summarizer";
import {
  ensureTrailingNewline,
  hasPromptLikeTail,
  hasRedrawSignal,
  looksLikeBadDistillation,
  normalizeForModel,
  structuralSimilarity
} from "./text";

type Mode = "undecided" | "watch" | "interactive";
export type ProgressPhase = "collecting" | "summarizing";

interface Burst {
  id: number;
  raw: string;
  normalized: string;
}

const PROGRESS_FRAMES = ["-", "\\", "|", "/"];
const PROGRESS_DOT_FRAMES = ["", ".", "..", "...", "..", "."];
const PROGRESS_LABELS: Record<ProgressPhase, string> = {
  collecting: "distill: waiting",
  summarizing: "distill: summarizing"
};

export interface DistillSessionOptions {
  summarizer: Summarizer;
  stdout: Pick<NodeJS.WriteStream, "write">;
  isTTY: boolean;
  progress?: Pick<NodeJS.WriteStream, "write">;
  onProgressPhase?: (phase: ProgressPhase) => void;
  onProgressStop?: () => void;
  idleMs?: number;
  interactiveGapMs?: number;
  progressFrameMs?: number;
}

export class DistillSession {
  private readonly summarizer: Summarizer;
  private readonly stdout: Pick<NodeJS.WriteStream, "write">;
  private readonly isTTY: boolean;
  private readonly progress: Pick<NodeJS.WriteStream, "write"> | null;
  private readonly onProgressPhase: ((phase: ProgressPhase) => void) | null;
  private readonly onProgressStop: (() => void) | null;
  private readonly idleMs: number;
  private readonly interactiveGapMs: number;
  private readonly progressFrameMs: number;
  private readonly rawBuffers: Buffer[] = [];
  private readonly completedBursts: Burst[] = [];
  private currentBurstBuffers: Buffer[] = [];
  private mode: Mode = "undecided";
  private progressPhase: ProgressPhase = "collecting";
  private sawRedraw = false;
  private idleTimer: ReturnType<typeof setTimeout> | null = null;
  private interactiveTimer: ReturnType<typeof setTimeout> | null = null;
  private progressTimer: ReturnType<typeof setInterval> | null = null;
  private queue: Promise<void> = Promise.resolve();
  private nextBurstId = 1;
  private renderedPairs = new Set<string>();
  private emittedWatchOutput = false;
  private passthrough = false;
  private progressVisible = false;
  private progressFrameIndex = 0;
  private lastProgressRenderAt = 0;

  constructor(options: DistillSessionOptions) {
    this.summarizer = options.summarizer;
    this.stdout = options.stdout;
    this.isTTY = options.isTTY;
    this.progress = options.progress ?? null;
    this.onProgressPhase = options.onProgressPhase ?? null;
    this.onProgressStop = options.onProgressStop ?? null;
    this.idleMs = options.idleMs ?? DEFAULT_IDLE_MS;
    this.interactiveGapMs = options.interactiveGapMs ?? DEFAULT_INTERACTIVE_GAP_MS;
    this.progressFrameMs = options.progressFrameMs ?? DEFAULT_PROGRESS_FRAME_MS;
    this.onProgressPhase?.(this.progressPhase);
    this.startProgress();
  }

  push(chunk: Buffer): void {
    if (chunk.length === 0) {
      return;
    }

    if (this.passthrough) {
      this.stdout.write(chunk);
      return;
    }

    if (this.mode !== "watch") {
      this.rawBuffers.push(chunk);
    }

    this.currentBurstBuffers.push(chunk);
    this.sawRedraw ||= hasRedrawSignal(chunk.toString("utf8"));

    this.restartIdleTimer();
    this.restartInteractiveTimer();
    this.renderProgressIfDue();
  }

  async end(): Promise<void> {
    this.clearTimers();

    if (this.passthrough) {
      this.stopProgress(true);
      return;
    }

    this.closeCurrentBurst();

    if (this.mode === "watch") {
      this.scheduleLatestWatchRender();
      await this.queue;
      return;
    }

    const rawInput = Buffer.concat(this.rawBuffers).toString("utf8");

    if (!rawInput) {
      this.stopProgress(true);
      return;
    }

    try {
      this.setProgressPhase("summarizing");
      const summary = await this.summarizer.summarizeBatch(
        normalizeForModel(rawInput)
      );

      if (looksLikeBadDistillation(rawInput, summary)) {
        this.stopProgress(true);
        this.stdout.write(Buffer.concat(this.rawBuffers));
        return;
      }

      this.stopProgress(true);
      this.stdout.write(ensureTrailingNewline(summary.trim()));
    } catch {
      this.stopProgress(true);
      this.stdout.write(Buffer.concat(this.rawBuffers));
    }
  }

  private restartIdleTimer(): void {
    if (this.idleTimer) {
      clearTimeout(this.idleTimer);
    }

    this.idleTimer = setTimeout(() => {
      this.closeCurrentBurst();

      if (this.mode === "undecided" && this.shouldPromoteToWatch()) {
        this.promoteToWatch();
        this.scheduleLatestWatchRender();
      }
    }, this.idleMs);
  }

  private restartInteractiveTimer(): void {
    if (this.mode !== "undecided") {
      return;
    }

    if (this.interactiveTimer) {
      clearTimeout(this.interactiveTimer);
    }

    const tail = this.getTail();

    if (!hasPromptLikeTail(tail)) {
      return;
    }

    this.interactiveTimer = setTimeout(() => {
      if (this.mode !== "undecided") {
        return;
      }

      if (!hasPromptLikeTail(this.getTail())) {
        return;
      }

      this.mode = "interactive";
      this.passthrough = true;
      this.clearTimers();
      this.stopProgress(true);
      this.stdout.write(Buffer.concat(this.rawBuffers));
    }, this.interactiveGapMs);
  }

  private clearTimers(): void {
    if (this.idleTimer) {
      clearTimeout(this.idleTimer);
      this.idleTimer = null;
    }

    if (this.interactiveTimer) {
      clearTimeout(this.interactiveTimer);
      this.interactiveTimer = null;
    }
  }

  private startProgress(): void {
    if (!this.progress || this.progressFrameMs <= 0 || this.progressTimer) {
      return;
    }

    this.renderProgress();
    this.progressTimer = setInterval(() => {
      if (this.progressTimer === null || this.mode === "watch" || this.passthrough) {
        return;
      }

      this.renderProgress();
    }, this.progressFrameMs);
  }

  private setProgressPhase(phase: ProgressPhase): void {
    if (this.progressPhase === phase) {
      return;
    }

    this.progressPhase = phase;
    this.progressFrameIndex = 0;
    this.onProgressPhase?.(phase);
    this.renderProgress();
  }

  private renderProgressIfDue(): void {
    if (!this.progress || this.mode === "watch" || this.passthrough) {
      return;
    }

    if (Date.now() - this.lastProgressRenderAt < this.progressFrameMs) {
      return;
    }

    this.renderProgress();
  }

  private renderProgress(): void {
    if (!this.progress) {
      return;
    }

    const frame = PROGRESS_FRAMES[this.progressFrameIndex % PROGRESS_FRAMES.length];
    const dots = PROGRESS_DOT_FRAMES[Math.floor(this.progressFrameIndex / PROGRESS_FRAMES.length) % PROGRESS_DOT_FRAMES.length];

    this.progressFrameIndex += 1;
    this.lastProgressRenderAt = Date.now();
    this.progress.write(`\r\u001b[2K${frame} ${PROGRESS_LABELS[this.progressPhase]}${dots}`);
    this.progressVisible = true;
  }

  private stopProgress(clearLine = false): void {
    if (this.progressTimer) {
      clearInterval(this.progressTimer);
      this.progressTimer = null;
    }

    this.onProgressStop?.();

    if (!clearLine || !this.progressVisible || !this.progress) {
      return;
    }

    this.progress.write("\r\u001b[2K");
    this.progressVisible = false;
  }

  private closeCurrentBurst(): void {
    if (this.currentBurstBuffers.length === 0 || this.passthrough) {
      return;
    }

    const raw = Buffer.concat(this.currentBurstBuffers).toString("utf8");
    this.currentBurstBuffers = [];

    if (!raw) {
      return;
    }

    this.completedBursts.push({
      id: this.nextBurstId,
      raw,
      normalized: normalizeForModel(raw)
    });
    this.nextBurstId += 1;
  }

  private shouldPromoteToWatch(): boolean {
    if (this.completedBursts.length < 2) {
      return false;
    }

    const previous = this.completedBursts[this.completedBursts.length - 2];
    const current = this.completedBursts[this.completedBursts.length - 1];
    const similarity = structuralSimilarity(previous.raw, current.raw);

    return this.sawRedraw || similarity >= 0.55;
  }

  private promoteToWatch(): void {
    if (this.mode === "watch") {
      return;
    }

    this.mode = "watch";
    this.rawBuffers.length = 0;
    this.clearTimers();
    this.stopProgress(true);
  }

  private scheduleLatestWatchRender(): void {
    if (this.completedBursts.length < 2) {
      return;
    }

    const previous = this.completedBursts[this.completedBursts.length - 2];
    const current = this.completedBursts[this.completedBursts.length - 1];
    const key = `${previous.id}:${current.id}`;

    if (this.renderedPairs.has(key)) {
      return;
    }

    this.renderedPairs.add(key);
    this.queue = this.queue.then(async () => {
      try {
        const summary = await this.summarizer.summarizeWatch(
          previous.normalized,
          current.normalized
        );

        if (looksLikeBadDistillation(current.raw, summary)) {
          this.renderWatchFallback(current.raw);
          return;
        }

        this.renderWatchSummary(summary.trim());
        this.trimWatchHistory();
      } catch {
        this.renderWatchFallback(current.raw);
      }
    });
  }

  private renderWatchSummary(summary: string): void {
    const output = ensureTrailingNewline(summary);

    if (this.isTTY) {
      this.stdout.write(`\u001b[2J\u001b[H${output}`);
      this.emittedWatchOutput = true;
      return;
    }

    if (this.emittedWatchOutput) {
      this.stdout.write("\n");
    }

    this.stdout.write(output);
    this.emittedWatchOutput = true;
  }

  private renderWatchFallback(raw: string): void {
    this.mode = "interactive";
    this.passthrough = true;
    this.stopProgress(true);
    this.stdout.write(raw);
  }

  private getTail(): string {
    const tailBuffers: Buffer[] = [];
    let remaining = 256;

    for (let index = this.rawBuffers.length - 1; index >= 0 && remaining > 0; index -= 1) {
      const chunk = this.rawBuffers[index];

      if (chunk.length <= remaining) {
        tailBuffers.unshift(chunk);
        remaining -= chunk.length;
        continue;
      }

      tailBuffers.unshift(chunk.subarray(chunk.length - remaining));
      remaining = 0;
    }

    return Buffer.concat(tailBuffers).toString("utf8");
  }

  private trimWatchHistory(): void {
    if (this.mode !== "watch" || this.completedBursts.length <= 2) {
      return;
    }

    this.completedBursts.splice(0, this.completedBursts.length - 2);
  }
}
