// Shared contract types — mirror exactly what the daemon serves.
// See plan §7. Do not drift these from the server.

export type Column = "todo" | "running" | "review" | "done";

export type RunStatus =
  | "created"
  | "running"
  | "steering"
  | "verifying"
  | "completed"
  | "failed"
  | "cancelled"
  | "paused"
  | "orphaned"
  | "migrated"
  | "merge-conflict"
  | "merged";

export type MergeState = {
  status: string;
  conflictedFiles?: string[];
  operationId?: string;
  mergeChangeId?: string;
};

export type DeliveryEvidence = {
  required: boolean;
  kind?: string;
  status: "not-requested" | "pending" | "deployed" | "verified" | "blocked" | "failed";
  target?: string;
  url?: string;
  appPath?: string;
  provider?: string;
  revision?: string;
  deploymentId?: string;
  version?: string;
  verifiedAt?: string;
  checks: string[];
};

export type Tokens = { input: number; output: number };

export type TaskUpdate = {
  instruction: string;
  ts: string;
  source?: string;
};

export type BoardNode = {
  id: string;
  /** Stable graph identity is `id`; scheduled tasks expose their worker here. */
  runId?: string;
  title: string;
  status: string;
  column: Column;
  blocked: boolean;
  backend: string;
  model?: string;
  effort?: string;
  lastLine: string | null;
  tokens: Tokens | null;
  deps: { hard: string[]; soft: string[] };
  createdAt: string;
  updatedAt: string;
  worktree: { path: string; workspaceName?: string } | null;
  merge: MergeState | null;
  delivery?: DeliveryEvidence;
  updates: TaskUpdate[];
};

export type BoardEdge = { from: string; to: string; kind: "hard" | "soft" | "judge" };

export type PlanGate = {
  id: string;
  nodeId?: string;
  question: string;
  options?: string[];
  createdAt: string;
};

export type MemoryEntry = { text: string; owner?: string; ts?: string };

export type ActivityEntry = { text: string; kind: "action" | "heartbeat"; ts?: string };

export type BoardSnapshot = {
  slug: string;
  name: string;
  generatedAt: string;
  nodes: BoardNode[];
  edges: BoardEdge[];
  gates: PlanGate[];
  memory: MemoryEntry[];
  activity: ActivityEntry[];
};

export type ProjectSummary = {
  slug: string;
  name: string;
  repoRoot: string;
  counts: {
    todo: number;
    running: number;
    review: number;
    done: number;
    blocked: number;
    failed: number;
  };
  lastActivityAt: string;
};

// ---------------------------------------------------------------------------
// API client. Thin wrappers over fetch; throws on non-2xx.
// ---------------------------------------------------------------------------

async function jsonOrThrow<T>(res: Response): Promise<T> {
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`${res.status} ${res.statusText}${body ? `: ${body}` : ""}`);
  }
  return res.json() as Promise<T>;
}

// Headers for a mutating request: the per-daemon token the board shell injected.
// A custom header also forces a CORS preflight cross-origin (which the board never
// approves), so this both authenticates same-origin calls and blocks drive-by CSRF.
function mutationHeaders(extra?: Record<string, string>): Record<string, string> {
  const token = (typeof window !== "undefined" && window.__RUDDER_TOKEN__) || "";
  return { "x-rudder-token": token, ...(extra ?? {}) };
}

export async function fetchProjects(): Promise<{ projects: ProjectSummary[] }> {
  return jsonOrThrow(await fetch("/api/projects"));
}

export async function fetchState(slug: string): Promise<BoardSnapshot> {
  return jsonOrThrow(await fetch(`/api/projects/${encodeURIComponent(slug)}/state`));
}

export async function fetchLog(slug: string, id: string, tail = 200): Promise<string> {
  const res = await fetch(
    `/api/projects/${encodeURIComponent(slug)}/tasks/${encodeURIComponent(id)}/log?tail=${tail}`
  );
  if (!res.ok) {
    throw new Error(`${res.status} ${res.statusText}`);
  }
  return res.text();
}

export async function postTask(
  slug: string,
  prompt: string,
): Promise<{
  runId?: string;
  nodeId?: string;
  nodeIds?: string[];
  requestId?: string;
  status?: "queued" | "accepted" | "delivered" | "failed";
}> {
  const requestId = newClientRequestId();
  return jsonOrThrow(
    await fetchMutationWithRetry(`/api/projects/${encodeURIComponent(slug)}/tasks`, {
      method: "POST",
      headers: mutationHeaders({ "content-type": "application/json" }),
      body: JSON.stringify({ prompt, requestId }),
    })
  );
}

export async function postMerge(
  slug: string,
  id: string
): Promise<{ status: string; conflictedFiles?: string[] }> {
  return jsonOrThrow(
    await fetch(`/api/projects/${encodeURIComponent(slug)}/tasks/${encodeURIComponent(id)}/merge`, {
      method: "POST",
      headers: mutationHeaders(),
    })
  );
}

export async function postCancel(slug: string, id: string): Promise<void> {
  const res = await fetch(
    `/api/projects/${encodeURIComponent(slug)}/tasks/${encodeURIComponent(id)}/cancel`,
    { method: "POST", headers: mutationHeaders() }
  );
  if (!res.ok) {
    throw new Error(`${res.status} ${res.statusText}`);
  }
}

// Steer a running agent (id) or the conductor (id = "conductor"). The instruction
// is delivered straight into that agent's live terminal by the native TUI.
export type SteerReceipt = {
  requestId: string;
  status: "queued" | "accepted" | "delivered" | "failed";
  mode: "redirected" | "resumed" | "queued";
  error?: string;
};

export async function postSteer(
  slug: string,
  id: string,
  instruction: string,
): Promise<SteerReceipt> {
  const requestId = newClientRequestId();
  const target = id === "conductor"
    ? `/api/projects/${encodeURIComponent(slug)}/steer`
    : `/api/projects/${encodeURIComponent(slug)}/tasks/${encodeURIComponent(id)}/steer`;
  return jsonOrThrow(
    await fetchMutationWithRetry(target, {
      method: "POST",
      headers: mutationHeaders({ "content-type": "application/json" }),
      body: JSON.stringify({ instruction, requestId }),
    })
  );
}

function newClientRequestId(): string {
  return globalThis.crypto?.randomUUID?.() ??
    `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}-${Math.random().toString(36).slice(2)}`;
}

async function fetchMutationWithRetry(input: RequestInfo | URL, init: RequestInit): Promise<Response> {
  try {
    return await fetch(input, init);
  } catch {
    // Loopback responses can still be lost during a daemon/browser handoff.
    // Retry once with the same requestId so the server's durable claim makes
    // the mutation idempotent.
    await new Promise((resolve) => setTimeout(resolve, 150));
    return fetch(input, init);
  }
}

export async function fetchSteerReceipt(slug: string, requestId: string): Promise<SteerReceipt> {
  return jsonOrThrow(
    await fetch(
      `/api/projects/${encodeURIComponent(slug)}/steer-receipts/${encodeURIComponent(requestId)}`,
    ),
  );
}

export function eventsUrl(slug: string): string {
  return `/api/projects/${encodeURIComponent(slug)}/events`;
}
