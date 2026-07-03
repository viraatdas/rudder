import fsp from "node:fs/promises";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import type { BackendId } from "./types.js";
import type { AuthProfileCredential, AuthProfileStore } from "./types.js";
import { loadAuthStore, loadConfig } from "./state.js";
import { rudderHome } from "./util.js";

const MODELS_DEV_URL = "https://models.dev/api.json";
const OPENAI_MODELS_URL = "https://api.openai.com/v1/models";
const ANTHROPIC_MODELS_URL = "https://api.anthropic.com/v1/models?limit=100";
const ANTHROPIC_VERSION = "2023-06-01";
const MODELS_DEV_CACHE_MAX_AGE_MS = 24 * 60 * 60 * 1000;
const MODEL_PICKER_LIMIT = 24;

export type ModelOption = {
  label: string;
  value?: string;
  detail?: string;
  backend?: BackendId;
};

type CodexCache = {
  models?: Array<{
    slug?: string;
    display_name?: string;
    description?: string;
    visibility?: string;
  }>;
};

type ModelsDevModel = {
  id?: string;
  name?: string;
  reasoning?: boolean;
  tool_call?: boolean;
  release_date?: string;
  last_updated?: string;
  limit?: {
    context?: number;
    output?: number;
  };
  modalities?: {
    input?: string[];
    output?: string[];
  };
  source?: string;
  created?: number;
  owned_by?: string;
};

type ModelsDevProvider = {
  id?: string;
  name?: string;
  models?: Record<string, ModelsDevModel>;
};

type ModelsDevCache = Record<string, ModelsDevProvider>;

type OpenAIModelsResponse = {
  data?: Array<{
    id?: string;
    created?: number;
    owned_by?: string;
    object?: string;
  }>;
};

type AnthropicModelsResponse = {
  data?: Array<{
    id?: string;
    display_name?: string;
    created_at?: string;
    type?: string;
  }>;
};

let modelCatalogInFlight: Promise<ModelsDevCache> | undefined;

export async function discoverModelOptions(
  backend: BackendId,
  configuredDefault?: string,
): Promise<ModelOption[]> {
  const defaultDetail = configuredDefault || "CLI default";
  const options: ModelOption[] = [{ label: "Default", value: undefined, detail: defaultDetail }];
  const discovered = backend === "claude" ? await discoverClaudeModelsDev() : await discoverCodexModelsDev();
  for (const option of discovered) {
    pushUnique(options, option);
    if (options.length >= MODEL_PICKER_LIMIT) {
      break;
    }
  }
  if (backend === "claude") {
    for (const option of claudeCodeAliasOptions()) {
      pushUnique(options, option);
    }
  }
  if (options.length <= 1) {
    const fallback = backend === "claude" ? await discoverClaudeModelsLocal() : await discoverCodexModelsLocal();
    for (const option of fallback) {
      pushUnique(options, option);
    }
  }
  return options;
}

export function fallbackModelOptions(backend: BackendId, configuredDefault?: string): ModelOption[] {
  const defaultDetail = configuredDefault || "CLI default";
  if (backend === "claude") {
    return [
      { label: "Default", value: undefined, detail: defaultDetail },
      ...claudeCodeAliasOptions(),
    ];
  }
  return [
    { label: "Default", value: undefined, detail: defaultDetail },
    { label: "gpt-5.5", value: "gpt-5.5" },
    { label: "gpt-5.4-codex", value: "gpt-5.4-codex" },
  ];
}

async function discoverCodexModelsDev(): Promise<ModelOption[]> {
  const data = await readModelsDev();
  const provider = data.openai;
  const entries = Object.entries(provider?.models ?? {})
    .filter(([id, model]) => isUsableTextModel(id, model) && isCodexRelevantModel(id))
    .sort(compareModelEntries("codex"));
  return entries.map(([id, model]) => ({
    label: id,
    value: id,
    detail: shortModelDetail(model),
  }));
}

async function discoverClaudeModelsDev(): Promise<ModelOption[]> {
  const data = await readModelsDev();
  const provider = data.anthropic;
  const entries = Object.entries(provider?.models ?? {})
    .filter(([id, model]) => id.startsWith("claude-") && isUsableTextModel(id, model) && isClaudePickerModel(id))
    .sort(compareModelEntries("claude"));
  return entries.map(([id, model]) => ({
    label: id,
    value: id,
    detail: model.name || prettyClaudeModel(id),
  }));
}

async function discoverCodexModelsLocal(): Promise<ModelOption[]> {
  const file = path.join(os.homedir(), ".codex", "models_cache.json");
  const raw = await fsp.readFile(file, "utf8").catch(() => "");
  if (!raw) {
    return [];
  }
  let parsed: CodexCache;
  try {
    parsed = JSON.parse(raw) as CodexCache;
  } catch {
    // A partially-written cache (Codex writing concurrently) should degrade to
    // "no local models" for this source, not reject the whole discovery.
    return [];
  }
  return (parsed.models ?? [])
    .filter((model) => model.slug && model.slug !== "codex-auto-review")
    .map((model) => ({
      label: model.slug || model.display_name || "model",
      value: model.slug,
      detail: model.display_name || model.description || "local Codex cache",
    }));
}

async function discoverClaudeModelsLocal(): Promise<ModelOption[]> {
  const counts = new Map<string, number>();
  await collectClaudeProjectModels(path.join(os.homedir(), ".claude", "projects"), counts);
  const recent = [...counts.entries()]
    .filter(([model]) => model !== "<synthetic>")
    .sort((a, b) => b[1] - a[1])
    .map(([model]) => ({
      label: model,
      value: model,
      detail: prettyClaudeModel(model),
    }));
  const options: ModelOption[] = claudeCodeAliasOptions();
  for (const option of recent) {
    pushUnique(options, option);
  }
  return options;
}

async function readModelsDev(): Promise<ModelsDevCache> {
  if (modelCatalogInFlight) {
    return modelCatalogInFlight;
  }
  modelCatalogInFlight = readMergedModelCatalog().finally(() => {
    modelCatalogInFlight = undefined;
  });
  return modelCatalogInFlight;
}

async function readMergedModelCatalog(): Promise<ModelsDevCache> {
  const cached = await readModelsDevCache();
  let base: ModelsDevCache | undefined;
  let shouldWrite = false;
  if (cached && Date.now() - cached.mtimeMs < MODELS_DEV_CACHE_MAX_AGE_MS) {
    base = cached.data;
  } else {
    const fresh = await fetchModelsDev().catch(() => null);
    if (fresh) {
      base = fresh;
      shouldWrite = true;
    } else {
      base = cached?.data ?? {};
    }
  }

  const merged = cloneModelCatalog(base);
  const added = await mergeLiveAndLocalModels(merged);
  if (shouldWrite || added) {
    await writeModelsDevCache(merged);
  }
  return merged;
}

async function fetchModelsDev(): Promise<ModelsDevCache> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 2500);
  try {
    const response = await fetch(MODELS_DEV_URL, { signal: controller.signal });
    if (!response.ok) {
      throw new Error(`models.dev returned ${response.status}`);
    }
    return await response.json() as ModelsDevCache;
  } finally {
    clearTimeout(timer);
  }
}

async function readModelsDevCache(): Promise<{ data: ModelsDevCache; mtimeMs: number } | null> {
  const file = modelsDevCachePath();
  const [raw, stat] = await Promise.all([
    fsp.readFile(file, "utf8").catch(() => ""),
    fsp.stat(file).catch(() => null),
  ]);
  if (!raw || !stat) {
    return null;
  }
  try {
    return { data: JSON.parse(raw) as ModelsDevCache, mtimeMs: stat.mtimeMs };
  } catch {
    return null;
  }
}

async function writeModelsDevCache(data: ModelsDevCache): Promise<void> {
  const file = modelsDevCachePath();
  await fsp.mkdir(path.dirname(file), { recursive: true });
  await fsp.writeFile(file, JSON.stringify(data), { mode: 0o600 });
}

function modelsDevCachePath(): string {
  // Resolve under RUDDER_HOME (falling back to ~/.rudder) so this TS writer and the
  // native Rust reader (models.rs models_dev_cache_path) agree on the same file when
  // RUDDER_HOME is set — otherwise dynamic model refresh + cached reasoning break.
  return path.join(rudderHome(), "models-dev.json");
}

function isUsableTextModel(id: string, model: ModelsDevModel): boolean {
  if (model.tool_call === false) {
    return false;
  }
  if (isExcludedOpenAiTextModel(id)) {
    return false;
  }
  const output = model.modalities?.output;
  return !output || output.includes("text");
}

function claudeCodeAliasOptions(): ModelOption[] {
  return [
    { label: "fable", value: "fable", detail: "most capable model" },
    { label: "fable[1m]", value: "fable[1m]", detail: "most capable · large context" },
    { label: "sonnet", value: "sonnet" },
    { label: "sonnet[1m]", value: "sonnet[1m]" },
    { label: "opus", value: "opus" },
    { label: "opus[1m]", value: "opus[1m]" },
    { label: "haiku", value: "haiku" },
  ];
}

function isClaudePickerModel(id: string): boolean {
  // No family-name allowlist: a brand-new model family (fable, and whatever
  // comes after it) must show up in the picker without a Rudder release. Only
  // the legacy 3.x-generation ids are excluded.
  return !id.includes("3-");
}

function isCodexRelevantModel(id: string): boolean {
  if (id.includes("deep-research") || id.includes("chat-latest")) {
    return false;
  }
  return id.includes("codex") || isGptTextModel(id) || isOSeriesModel(id);
}

function compareModelEntries(backend: "claude" | "codex") {
  return (a: [string, ModelsDevModel], b: [string, ModelsDevModel]) => {
    const score = backend === "claude" ? scoreClaudeModel : scoreCodexModel;
    const diff = score(b[0], b[1]) - score(a[0], a[1]);
    if (diff !== 0) {
      return diff;
    }
    return (b[1].release_date ?? "").localeCompare(a[1].release_date ?? "") || a[0].localeCompare(b[0]);
  };
}

function scoreClaudeModel(id: string, model: ModelsDevModel): number {
  let score = 0;
  if (id.includes("fable")) score += 50;
  else if (id.includes("sonnet")) score += 40;
  else if (id.includes("opus")) score += 35;
  else if (id.includes("haiku")) score += 20;
  // An unknown family is likely a NEW tier; rank it above haiku rather than
  // letting it score zero and fall off the picker cut.
  else score += 30;
  if (model.reasoning) score += 20;
  score += recencyScore(model);
  return score;
}

function scoreCodexModel(id: string, model: ModelsDevModel): number {
  let score = 0;
  if (id.includes("codex")) score += 60;
  score += gptVersionScore(id);
  if (isOSeriesModel(id)) score += 25;
  if (model.reasoning) score += 20;
  score += recencyScore(model);
  return score;
}

async function mergeLiveAndLocalModels(data: ModelsDevCache): Promise<boolean> {
  let changed = false;
  const [openai, anthropic, codexLocal, claudeLocal] = await Promise.all([
    fetchOpenAIModelCatalog().catch(() => []),
    fetchAnthropicModelCatalog().catch(() => []),
    readCodexLocalModels().catch(() => []),
    readClaudeLocalModelIds().catch(() => []),
  ]);

  for (const model of openai) {
    changed = mergeProviderModel(data, "openai", model.id, model.meta) || changed;
  }
  for (const model of anthropic) {
    changed = mergeProviderModel(data, "anthropic", model.id, model.meta) || changed;
  }
  for (const model of codexLocal) {
    changed = mergeProviderModel(data, "openai", model.id, model.meta) || changed;
  }
  for (const model of claudeLocal) {
    changed = mergeProviderModel(data, "anthropic", model.id, model.meta) || changed;
  }
  return changed;
}

async function fetchOpenAIModelCatalog(): Promise<Array<{ id: string; meta: ModelsDevModel }>> {
  const authHeaders = await openAIAuthHeaders();
  for (const headers of authHeaders) {
    const response = await fetchJsonWithTimeout<OpenAIModelsResponse>(OPENAI_MODELS_URL, { headers }).catch(() => null);
    const rows = response?.data ?? [];
    if (rows.length === 0) {
      continue;
    }
    return rows
      .filter((model) => typeof model.id === "string" && isCodexRelevantModel(model.id) && !isExcludedOpenAiTextModel(model.id))
      .map((model) => ({
        id: model.id!,
        meta: {
          id: model.id,
          name: model.id,
          tool_call: true,
          reasoning: isLikelyReasoningModel("codex", model.id!),
          release_date: model.created ? new Date(model.created * 1000).toISOString().slice(0, 10) : undefined,
          modalities: { output: ["text"] },
          source: "openai-api",
          created: model.created,
          owned_by: model.owned_by,
        },
      }));
  }
  return [];
}

async function fetchAnthropicModelCatalog(): Promise<Array<{ id: string; meta: ModelsDevModel }>> {
  const authHeaders = await anthropicAuthHeaders();
  for (const headers of authHeaders) {
    const response = await fetchJsonWithTimeout<AnthropicModelsResponse>(ANTHROPIC_MODELS_URL, { headers }).catch(() => null);
    const rows = response?.data ?? [];
    if (rows.length === 0) {
      continue;
    }
    return rows
      .filter((model) => typeof model.id === "string" && model.id.startsWith("claude-") && isClaudePickerModel(model.id))
      .map((model) => ({
        id: model.id!,
        meta: {
          id: model.id,
          name: model.display_name || prettyClaudeModel(model.id!),
          tool_call: true,
          reasoning: isLikelyReasoningModel("claude", model.id!),
          release_date: typeof model.created_at === "string" ? model.created_at.slice(0, 10) : undefined,
          modalities: { output: ["text"] },
          source: "anthropic-api",
        },
      }));
  }
  return [];
}

async function readCodexLocalModels(): Promise<Array<{ id: string; meta: ModelsDevModel }>> {
  return (await discoverCodexModelsLocal())
    .filter((model) => model.value && isCodexRelevantModel(model.value) && !isExcludedOpenAiTextModel(model.value))
    .map((model) => ({
      id: model.value!,
      meta: {
        id: model.value,
        name: model.detail || model.label,
        tool_call: true,
        reasoning: isLikelyReasoningModel("codex", model.value!),
        modalities: { output: ["text"] },
        source: "codex-local-cache",
      },
    }));
}

async function readClaudeLocalModelIds(): Promise<Array<{ id: string; meta: ModelsDevModel }>> {
  const counts = new Map<string, number>();
  await collectClaudeProjectModels(path.join(os.homedir(), ".claude", "projects"), counts);
  return [...counts.keys()]
    .filter((id) => id.startsWith("claude-") && isClaudePickerModel(id))
    .map((id) => ({
      id,
      meta: {
        id,
        name: prettyClaudeModel(id),
        tool_call: true,
        reasoning: isLikelyReasoningModel("claude", id),
        modalities: { output: ["text"] },
        source: "claude-local-history",
      },
    }));
}

async function openAIAuthHeaders(): Promise<Array<Record<string, string>>> {
  const values = await providerCredentialValues("openai");
  return values.map((value) => ({ Authorization: `Bearer ${value}` }));
}

async function anthropicAuthHeaders(): Promise<Array<Record<string, string>>> {
  const credentials = await providerCredentials("anthropic");
  const headers: Array<Record<string, string>> = [];
  for (const credential of credentials) {
    if (credential.kind === "api_key") {
      headers.push({
        "x-api-key": credential.value,
        "anthropic-version": ANTHROPIC_VERSION,
      });
    } else {
      headers.push({
        Authorization: `Bearer ${credential.value}`,
        "anthropic-version": ANTHROPIC_VERSION,
      });
    }
  }
  return headers;
}

async function providerCredentialValues(provider: "openai" | "anthropic"): Promise<string[]> {
  return (await providerCredentials(provider)).map((credential) => credential.value);
}

async function providerCredentials(provider: "openai" | "anthropic"): Promise<Array<{ kind: "api_key" | "token"; value: string }>> {
  const values: Array<{ kind: "api_key" | "token"; value: string }> = [];
  const envKey = provider === "openai" ? process.env.OPENAI_API_KEY?.trim() : process.env.ANTHROPIC_API_KEY?.trim();
  if (envKey) {
    values.push({ kind: "api_key", value: envKey });
  }
  const [store, config] = await Promise.all([
    loadAuthStore().catch((): AuthProfileStore => ({ version: 1, profiles: {} })),
    loadConfig().catch(() => undefined),
  ]);
  const ids = provider === "openai"
    ? [
        config?.backends?.codex?.profileId,
        "openai-codex:default",
        "openai:env",
        "openai:default",
      ]
    : [
        config?.backends?.claude?.profileId,
        "anthropic:claude-code",
        "anthropic:env",
        "anthropic:default",
      ];
  for (const id of ids) {
    if (!id) {
      continue;
    }
    addCredential(values, store.profiles[id]);
  }
  const seen = new Set<string>();
  return values.filter((credential) => {
    const key = `${credential.kind}:${credential.value}`;
    if (seen.has(key)) {
      return false;
    }
    seen.add(key);
    return true;
  });
}

function addCredential(values: Array<{ kind: "api_key" | "token"; value: string }>, credential: AuthProfileCredential | undefined): void {
  if (!credential) {
    return;
  }
  if (credential.type === "api_key" && credential.key) {
    values.push({ kind: "api_key", value: credential.key });
  } else if (credential.type === "token" && credential.token) {
    values.push({ kind: "token", value: credential.token });
  } else if (credential.type === "oauth" && credential.access) {
    values.push({ kind: "token", value: credential.access });
  }
}

async function fetchJsonWithTimeout<T>(url: string, init: RequestInit): Promise<T> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 2500);
  try {
    const response = await fetch(url, { ...init, signal: controller.signal });
    if (!response.ok) {
      throw new Error(`${url} returned ${response.status}`);
    }
    return await response.json() as T;
  } finally {
    clearTimeout(timer);
  }
}

function cloneModelCatalog(data: ModelsDevCache): ModelsDevCache {
  return JSON.parse(JSON.stringify(data)) as ModelsDevCache;
}

function mergeProviderModel(data: ModelsDevCache, providerId: "openai" | "anthropic", modelId: string, meta: ModelsDevModel): boolean {
  if (!modelId) {
    return false;
  }
  const provider = data[providerId] ?? { id: providerId, models: {} };
  const models = provider.models ?? {};
  const previous = models[modelId] ?? {};
  models[modelId] = {
    ...previous,
    ...meta,
    id: modelId,
  };
  data[providerId] = {
    ...provider,
    id: provider.id || providerId,
    models,
  };
  return JSON.stringify(previous) !== JSON.stringify(models[modelId]);
}

function isExcludedOpenAiTextModel(id: string): boolean {
  return /\b(embedding|image|audio|tts|whisper|transcribe|translation|moderation|rerank|realtime|dall-e|search)\b/i.test(id);
}

function isGptTextModel(id: string): boolean {
  return /^gpt-\d/.test(id);
}

function isOSeriesModel(id: string): boolean {
  return /^o\d/.test(id);
}

function isLikelyReasoningModel(backend: "claude" | "codex", id: string): boolean {
  if (backend === "claude") {
    if (!id.startsWith("claude-") || id.includes("haiku") || id.includes("3-")) {
      return id.includes("opus") || id.includes("sonnet") || id.includes("fable");
    }
    return true;
  }
  return isGptTextModel(id) || id.includes("codex") || isOSeriesModel(id);
}

function gptVersionScore(id: string): number {
  const match = /^gpt-(\d+)(?:[.-](\d+))?/.exec(id);
  if (!match) {
    return 0;
  }
  const major = Number(match[1]);
  const minor = Number(match[2] ?? "0");
  if (!Number.isFinite(major) || !Number.isFinite(minor)) {
    return 0;
  }
  return 20 + major * 20 + minor;
}

function recencyScore(model: ModelsDevModel): number {
  const date = model.release_date || model.last_updated;
  if (!date) {
    return 0;
  }
  const timestamp = Date.parse(date);
  if (Number.isNaN(timestamp)) {
    return 0;
  }
  return Math.min(30, Math.max(0, Math.round((timestamp - Date.parse("2024-01-01")) / (30 * 24 * 60 * 60 * 1000))));
}

function shortModelDetail(model: ModelsDevModel): string {
  const parts: string[] = [];
  if (model.name) {
    parts.push(model.name);
  }
  if (model.limit?.context) {
    parts.push(formatNumber(model.limit.context));
  }
  return parts.join("  ");
}

function formatNumber(value: number): string {
  if (value >= 1_000_000) {
    return `${Math.round(value / 100_000) / 10}M`;
  }
  if (value >= 1_000) {
    return `${Math.round(value / 1000)}k`;
  }
  return String(value);
}

async function collectClaudeProjectModels(dir: string, counts: Map<string, number>, depth = 0): Promise<void> {
  if (depth > 2 || !fs.existsSync(dir)) {
    return;
  }
  const entries = await fsp.readdir(dir, { withFileTypes: true }).catch(() => []);
  for (const entry of entries.slice(0, 700)) {
    const fullPath = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      await collectClaudeProjectModels(fullPath, counts, depth + 1);
      continue;
    }
    if (!entry.name.endsWith(".jsonl")) {
      continue;
    }
    const raw = await fsp.readFile(fullPath, "utf8").catch(() => "");
    const lines = raw.split(/\r?\n/).slice(-250);
    for (const line of lines) {
      if (!line.trim()) {
        continue;
      }
      try {
        collectModelFields(JSON.parse(line), counts);
      } catch {
        // Ignore corrupt or partial JSONL rows from active sessions.
      }
    }
  }
}

function collectModelFields(value: unknown, counts: Map<string, number>, depth = 0): void {
  if (!value || typeof value !== "object" || depth > 5) {
    return;
  }
  if (Array.isArray(value)) {
    for (const item of value) {
      collectModelFields(item, counts, depth + 1);
    }
    return;
  }
  const record = value as Record<string, unknown>;
  if (typeof record.model === "string") {
    counts.set(record.model, (counts.get(record.model) ?? 0) + 1);
  }
  if (record.model && typeof record.model === "object") {
    const model = record.model as Record<string, unknown>;
    for (const key of ["id", "name", "model"]) {
      if (typeof model[key] === "string") {
        counts.set(model[key], (counts.get(model[key]) ?? 0) + 1);
      }
    }
  }
  for (const child of Object.values(record)) {
    collectModelFields(child, counts, depth + 1);
  }
}

function prettyClaudeModel(model: string): string {
  if (model === "opus" || model === "sonnet") {
    return `${capitalize(model)} latest`;
  }
  return model
    .replace(/^claude-/, "")
    .split("-")
    .map((part) => (/^\d+$/.test(part) ? part : capitalize(part)))
    .join(" ");
}

function capitalize(value: string): string {
  return `${value.slice(0, 1).toUpperCase()}${value.slice(1)}`;
}

function pushUnique(options: ModelOption[], option: ModelOption): void {
  if (options.some((existing) => existing.value === option.value)) {
    return;
  }
  options.push(option);
}
