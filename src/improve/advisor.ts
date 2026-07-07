import { callTextModel, resolveAnthropicApiKey } from "../task-summary.js";
import type { SpendMeter } from "./state.js";

/**
 * Advisor-pattern text call (Anthropic advisor tool, beta
 * advisor-tool-2026-03-01): a cheaper executor model generates the output and
 * consults a higher-intelligence advisor mid-generation for strategic
 * guidance. Most tokens bill at the executor rate; the loop gets near
 * advisor-grade judgment on its mining and judging calls at a fraction of the
 * cost of running the big model end to end.
 *
 * Degradation path: the deterministic test hook and the no-API-key case (CLI
 * auth) fall back to callTextModel, and any advisor-request failure falls
 * back to a plain executor-only call, so the loop never hard-depends on the
 * beta.
 */

const ADVISOR_BETA = "advisor-tool-2026-03-01";
const ADVISOR_MAX_TOKENS = 2048;
const MAX_PAUSE_RESUMES = 3;

const ADVISOR_SYSTEM_NUDGE = `

You have access to an \`advisor\` tool backed by a stronger reviewer model. It takes NO parameters; your full context is forwarded automatically. Consult it once before committing to your answer, weigh its guidance seriously, then produce the final output yourself in the requested format.`;

const ADVISOR_USER_SUFFIX =
  "\n\n(Advisor: please keep your guidance under 200 words; a focused verdict on the hard part beats a comprehensive plan.)";

type ContentBlock = { type?: string; text?: string } & Record<string, unknown>;

type MessagesResponse = {
  content?: ContentBlock[];
  stop_reason?: string;
  usage?: {
    iterations?: Array<{
      type?: string;
      model?: string;
      input_tokens?: number;
      output_tokens?: number;
    }>;
    input_tokens?: number;
    output_tokens?: number;
  };
};

export async function callAdvisedTextModel(params: {
  executorModel: string;
  advisorModel: string;
  system: string;
  user: string;
  maxTokens?: number;
  timeoutMs?: number;
  meter?: SpendMeter;
}): Promise<string> {
  const plainFallback = () =>
    callTextModel({
      model: params.executorModel,
      system: params.system,
      user: params.user,
      maxTokens: params.maxTokens,
      timeoutMs: params.timeoutMs,
    });

  // Deterministic test hook and disabled-advisor path go straight through.
  if (process.env.RUDDER_FAKE_MODEL_OUTPUT || !params.advisorModel) {
    return await plainFallback();
  }
  const apiKey = await resolveAnthropicApiKey();
  if (!apiKey) {
    // CLI-auth users have no raw key to hit the API with; use the CLI path.
    return await plainFallback();
  }

  const tools = [
    {
      type: "advisor_20260301",
      name: "advisor",
      model: params.advisorModel,
      max_tokens: ADVISOR_MAX_TOKENS,
    },
  ];
  const messages: Array<{ role: string; content: unknown }> = [
    { role: "user", content: params.user + ADVISOR_USER_SUFFIX },
  ];
  const timeoutMs = params.timeoutMs ?? 240000;
  const collected: string[] = [];

  for (let attempt = 0; attempt <= MAX_PAUSE_RESUMES; attempt += 1) {
    let data: MessagesResponse;
    try {
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), timeoutMs);
      const response = await fetch("https://api.anthropic.com/v1/messages", {
        method: "POST",
        headers: {
          "content-type": "application/json",
          "x-api-key": apiKey,
          "anthropic-version": "2023-06-01",
          "anthropic-beta": ADVISOR_BETA,
        },
        body: JSON.stringify({
          model: params.executorModel,
          max_tokens: params.maxTokens ?? 2048,
          system: params.system + ADVISOR_SYSTEM_NUDGE,
          tools,
          messages,
        }),
        signal: controller.signal,
      });
      clearTimeout(timer);
      if (!response.ok) {
        // Invalid pair / beta unavailable / overloaded: degrade to plain.
        return await plainFallback();
      }
      data = (await response.json()) as MessagesResponse;
    } catch {
      return await plainFallback();
    }

    meterIterations(params, data);
    for (const block of data.content ?? []) {
      if (block.type === "text" && typeof block.text === "string") {
        collected.push(block.text);
      }
    }
    if (data.stop_reason === "pause_turn" && attempt < MAX_PAUSE_RESUMES) {
      // Resume the paused turn: round-trip the assistant content verbatim.
      messages.push({ role: "assistant", content: data.content ?? [] });
      continue;
    }
    break;
  }

  const text = collected.join("").trim();
  return text || (await plainFallback());
}

function meterIterations(
  params: { executorModel: string; advisorModel: string; meter?: SpendMeter },
  data: MessagesResponse,
): void {
  if (!params.meter) return;
  const iterations = data.usage?.iterations;
  if (iterations && iterations.length > 0) {
    for (const iteration of iterations) {
      const model =
        iteration.type === "advisor_message"
          ? (iteration.model ?? params.advisorModel)
          : params.executorModel;
      params.meter.addModelTokens(model, iteration.input_tokens ?? 0, iteration.output_tokens ?? 0);
    }
    return;
  }
  params.meter.addModelTokens(
    params.executorModel,
    data.usage?.input_tokens ?? 0,
    data.usage?.output_tokens ?? 0,
  );
}
