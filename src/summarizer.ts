import type { RuntimeConfig } from "./config";
import { requestOllama } from "./ollama";
import { requestOpenAI } from "./openai";
import { buildBatchPrompt, buildWatchPrompt } from "./prompt";

export interface Summarizer {
  summarizeBatch(input: string): Promise<string>;
  summarizeWatch(previousCycle: string, currentCycle: string): Promise<string>;
}

function requestLLM(
  config: RuntimeConfig,
  prompt: string,
  fetchImpl?: typeof fetch
): Promise<string> {
  if (config.provider === "openai") {
    return requestOpenAI({
      baseUrl: config.host,
      apiKey: config.apiKey,
      model: config.model,
      prompt,
      timeoutMs: config.timeoutMs,
      fetchImpl
    });
  }

  return requestOllama({
    host: config.host,
    model: config.model,
    prompt,
    timeoutMs: config.timeoutMs,
    thinking: config.thinking,
    fetchImpl
  });
}

export function createOllamaSummarizer(
  config: RuntimeConfig,
  fetchImpl?: typeof fetch
): Summarizer {
  return {
    summarizeBatch(input: string) {
      return requestLLM(config, buildBatchPrompt(config.question, input), fetchImpl);
    },
    summarizeWatch(previousCycle: string, currentCycle: string) {
      return requestLLM(
        config,
        buildWatchPrompt(config.question, previousCycle, currentCycle),
        fetchImpl
      );
    }
  };
}
