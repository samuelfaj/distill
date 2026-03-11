import type { RuntimeConfig } from "./config";
import {
  ensureDistillDaemonRunning,
  isDistillDaemonCompatible,
  isDistillDaemonAutostartDisabled,
  requestDistillDaemon,
  tryRequestDistillDaemon
} from "./daemon";
import { requestOllama } from "./ollama";
import { buildBatchPrompt, buildWatchPrompt } from "./prompt";

export interface Summarizer {
  summarizeBatch(input: string): Promise<string>;
  summarizeWatch(previousCycle: string, currentCycle: string): Promise<string>;
}

export function createSummarizer(
  config: RuntimeConfig,
  fetchImpl?: typeof fetch
): Summarizer {
  const summarize = async (prompt: string) => {
    if (config.provider === "bitnet") {
      const daemonProbe = await tryRequestDistillDaemon({ type: "ping" }, 250);

      if (
        !daemonProbe ||
        !daemonProbe.ok ||
        (daemonProbe.ok &&
          (daemonProbe.status === "starting" ||
            !isDistillDaemonCompatible(daemonProbe, config)))
      ) {
        if (isDistillDaemonAutostartDisabled(process.env)) {
          throw new Error("distill daemon is not running.");
        }

        await ensureDistillDaemonRunning(
          {
            provider: config.provider,
            model: config.model,
            host: config.host,
            timeoutMs: config.timeoutMs,
            thinking: config.thinking
          },
          process.env
        );
      }

      const daemonResponse = await requestDistillDaemon(
        {
          type: "summarize",
          config,
          prompt
        },
        config.timeoutMs
      );

      if (!daemonResponse.ok) {
        throw new Error(daemonResponse.error);
      }

      return daemonResponse.output ?? "";
    }

    return requestOllama({
      host: config.host,
      model: config.model,
      prompt,
      timeoutMs: config.timeoutMs,
      thinking: config.thinking,
      fetchImpl
    });
  };

  return {
    summarizeBatch(input: string) {
      return summarize(buildBatchPrompt(config.question, input));
    },
    summarizeWatch(previousCycle: string, currentCycle: string) {
      return summarize(
        buildWatchPrompt(config.question, previousCycle, currentCycle)
      );
    }
  };
}
