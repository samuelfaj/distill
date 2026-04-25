import type { RuntimeConfig } from "./config";
import { requestOpenAI } from "./openai";
import { buildBatchPrompt, buildWatchPrompt, type PromptMessages } from "./prompt";

export interface Summarizer {
  summarizeBatch(input: string): Promise<string>;
  summarizeWatch(previousCycle: string, currentCycle: string): Promise<string>;
}

function requestLLM(
  config: RuntimeConfig,
  prompt: string | PromptMessages,
  fetchImpl?: typeof fetch
): Promise<string> {
  return requestOpenAI({
    baseUrl: config.host,
    apiKey: config.apiKey,
    model: config.model,
    prompt,
    timeoutMs: config.timeoutMs,
    temperature: 0,
    maxTokens: 512,
    fetchImpl
  });
}

export function createSummarizer(
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
