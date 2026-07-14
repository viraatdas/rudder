export type JsonValue =
  | null
  | boolean
  | number
  | string
  | JsonValue[]
  | { [key: string]: JsonValue };

export type CredentialType = "api_key" | "oauth" | "token";

export type ApiKeyCredential = {
  type: "api_key";
  provider: string;
  key?: string;
  keyRef?: SecretRef;
  email?: string;
  metadata?: Record<string, string>;
};

export type TokenCredential = {
  type: "token";
  provider: string;
  token?: string;
  tokenRef?: SecretRef;
  expires?: number;
  email?: string;
};

export type OAuthCredential = {
  type: "oauth";
  provider: string;
  access: string;
  refresh: string;
  expires: number;
  clientId?: string;
  email?: string;
  accountId?: string;
  enterpriseUrl?: string;
  projectId?: string;
};

export type SecretRef = {
  source: "env" | "file" | "exec";
  provider: string;
  id: string;
};

export type AuthProfileCredential = ApiKeyCredential | TokenCredential | OAuthCredential;

export type ProfileUsageStats = {
  lastUsed?: number;
  cooldownUntil?: number;
  disabledUntil?: number;
  disabledReason?: string;
  errorCount?: number;
  lastFailureAt?: number;
};

export type AuthProfileStore = {
  version: 1;
  profiles: Record<string, AuthProfileCredential>;
  order?: Record<string, string[]>;
  lastGood?: Record<string, string>;
  usageStats?: Record<string, ProfileUsageStats>;
};

export type RudderConfig = {
  version: 1;
  vcs?: VcsMode;
  defaultBackend: BackendId;
  lastUsedBackend?: BackendId;
  mergeStrategy: MergeStrategy;
  colorMode?: "terminal" | "paper";
  runPolicy: {
    sameCheckout: "single-active";
    concurrentPromptMode: "worktree" | "queue";
    mergeMode: "manual-on-conflict";
  };
  acpx: {
    install: "latest";
  };
  backends: {
    claude?: BackendConfig;
    codex?: BackendConfig;
    acpx?: BackendConfig;
  };
  board?: {
    port?: number;
  };
  orchestrator?: {
    maxParallel: number;
    budget?: {
      maxTokens?: number;
    };
  };
  /** Continual improvement loop (`rudder improve`); see docs/continual-improvement.md. */
  improve?: {
    enabled?: boolean;
    autonomy?: "observe" | "propose" | "ship";
    budgetUsd?: number;
    maxFindings?: number;
    repoPath?: string;
    allowedRemote?: string;
    excludeProjects?: string[];
    minerModel?: string;
    judgeModel?: string;
    advisorModel?: string;
    workerModel?: string;
    agentTimeoutMs?: number;
  };
};

export const DEFAULT_BOARD_PORT = 4774;

export type BackendId = "claude" | "codex" | "acpx";

export type VcsMode = "git" | "jj";

export type EffortLevel = "low" | "medium" | "high" | "xhigh" | "max";

export type RunMode = "execute" | "plan";

export type MergeStrategy = "merge" | "rebase";

export type BackendConfig = {
  profileId?: string;
  model?: string;
  effort?: EffortLevel;
  reasoningEffort?: EffortLevel;
};

export type RunStatus =
  | "created"
  | "running"
  | "steering"
  | "verifying"
  | "completed"
  | "failed"
  | "cancelled"
  | "orphaned"
  | "migrated"
  | "merge-conflict"
  | "merged";

export type UndoEntry = {
  opId: string;
  label: string;
  ts: string;
  runIds: string[];
};

// ---------------------------------------------------------------------------
// Orchestrated DAG (Phase 3). graph.json is the daemon-owned topology; the
// per-run execution state stays in run.json. NodeStatus is the DAG-level
// vocabulary the daemon projects from each node's worker-owned RunStatus.
// nodes/edges are keyed objects (a mergeable key union, not arrays).
// ---------------------------------------------------------------------------

export type NodeStatus =
  | "planned" // in graph, deps not satisfied / not yet scheduled
  | "ready" // all hard-dep parents merged, eligible to launch
  | "running" // worker active
  | "review" // worker completed, awaiting verify/merge gate
  | "blocked" // a hard-dep parent failed/cancelled
  | "merged" // landed into the integration change
  | "failed"; // worker failed verification or crashed

export type DepType = "hard" | "soft";
// hard: child cannot START until parent is `merged`.
// soft: child runs in parallel; when parent MERGES, its diff is piped into the child's context.

// EdgeType is the graph-level superset of DepType. The planner/parser only ever
// emit hard/soft (DepType); "judge" is an orchestration-shape edge the
// fan-out-and-judge builder adds directly. A judge edge from variant V -> judge
// J means J becomes ready when V REACHES review (the variant finished its work),
// not when V merges. Variant diffs are delivered to the judge on review, like a
// soft edge but gated on review rather than merge.
export type EdgeType = DepType | "judge";

export type GraphEdge = {
  id: string;
  from: string;
  to: string;
  type: EdgeType;
  why?: string;
  delivered?: boolean; // soft/judge: parent diff already injected
};

export type TaskNode = {
  id: string; // content hash (mergeable key): shortHash(title+createdAt+nonce)
  title: string;
  prompt: string;
  goal?: string; // one-line OBJECTIVE for the launch Objective line
  success?: string; // verifiable SUCCESS / DONE-WHEN condition
  backend: BackendId;
  model?: string;
  effort?: EffortLevel;
  status: NodeStatus;
  runId?: string; // links to .rudder/runs/<runId> once scheduled
  worktree?: { path: string; workspaceName?: string };
  jjChangeId?: string; // the node's jj change
  deps: string[]; // edge ids where this node is `to` (fast ready-check)
  lastLine?: string;
  tokens?: { input: number; output: number };
  reviewState?: "pending" | "approved" | "changes-requested";
  resolverRunId?: string; // Phase 5a: the resolver-agent run handling a merge conflict
  merge?: MergeState; // Phase 5a: last merge attempt for this node (conflict state)
  // Phase 5b (fan-out-and-judge): a variant node whose work the judge node
  // selected/superseded. The variant ends in "review" (it is never merged); the
  // board can render it as superseded by the judge node id stored here.
  supersededBy?: string;
  source: "planner" | "injection";
  createdAt: string;
  updatedAt: string;
};

export type RudderGraph = {
  version: 1;
  repoRoot: string;
  integrationChangeId?: string; // the jj "trunk" all merged nodes stack onto
  nodes: Record<string, TaskNode>; // keyed by id -> mergeable union, no line conflicts
  edges: Record<string, GraphEdge>; // keyed by id
  updatedAt: string;
};

// ---------------------------------------------------------------------------
// Planner (Phase 3). The planner LLM emits a RUDDER_PLAN_TASKS block; the TS
// parser ports the native tasks.rs parser into a PlanDag the daemon scaffolds.
// ---------------------------------------------------------------------------

export type PlanEdge = { on: string; type: EdgeType; why?: string };

export type PlanNode = {
  id: string;
  title: string;
  prompt: string;
  goal?: string; // one-line OBJECTIVE for the launch Objective line
  success?: string; // verifiable SUCCESS / DONE-WHEN condition
  deps: PlanEdge[];
  backend?: BackendId;
  model?: string;
  effort?: EffortLevel;
  fileScope?: string[];
};

export type PlanDag = {
  nodes: PlanNode[];
  edges: Array<{ from: string; to: string; type: EdgeType; why?: string }>;
};

export type InferredDep = { node: string; type: DepType };

export type ReconcileResult = { node: PlanNode; inferredDeps: InferredDep[] };

// ---------------------------------------------------------------------------
// Conflict resolver (Phase 5a). When a node merge records conflicts, the daemon
// spawns a resolver-agent node whose working copy IS the conflicted merge
// change. This context is written to .rudder/runs/<resolverRunId>/resolver.json
// so the worker (and any UI) can read what it is resolving.
// ---------------------------------------------------------------------------

export type ResolverContext = {
  mergeChangeId: string;
  parentChangeIds: string[];
  conflictedFiles: string[];
  nodeTitle: string;
  intoTitle: string;
  workspacePath: string;
};

export type RunRecord = {
  id: string;
  status: RunStatus;
  vcs?: VcsMode;
  mode?: RunMode;
  resolverFor?: string;
  resolverRunId?: string;
  // Best-effort token usage captured from the backend's own stream output
  // (claude stream-json usage / codex token_count events). Summed by the
  // scheduler's budget cap. May be absent when the backend emits no usage.
  tokens?: { input: number; output: number };
  task: string;
  taskSummary?: string;
  taskSummaryLlm?: boolean;
  backend: BackendId;
  model?: string;
  effort?: EffortLevel;
  createdAt: string;
  updatedAt: string;
  repoRoot: string;
  targetBranch: string;
  baseCommit: string;
  worktree: {
    enabled: boolean;
    path: string;
    branch?: string;
    workspaceName?: string;
    jjChangeId?: string;
  };
  process?: {
    /** Detached Rudder __worker process that owns verification/lifecycle. */
    controllerPid?: number;
    /** Backend CLI child (Claude/Codex/acpx) when one is active. */
    backendPid?: number;
    pid?: number;
    /** Identifies the worker pass that currently owns this run record. A web
     * redirect starts a new attempt so the superseded worker cannot write a
     * stale terminal status over the new turn. */
    attemptId?: string;
    startedAt?: string;
    endedAt?: string;
    exitCode?: number | null;
    signal?: NodeJS.Signals | null;
  };
  currentPrompt?: string;
  turns?: Array<{
    ts: string;
    prompt: string;
    source: "user" | "steerer" | "regoal";
  }>;
  lastUserInputAt?: string;
  autoSteer?: {
    count: number;
    max: number;
    waitingSince?: string;
  };
  session?: {
    nativeSessionId?: string;
    acpxSessionId?: string;
    sessionName?: string;
  };
  terminal?: {
    kind: "tmux";
    sessionName: string;
    paneId: string;
    paneTitle?: string;
    logPath?: string;
    launchedAt: string;
  };
  verification?: VerificationResult;
  merge?: MergeState;
  sync?: SyncState;
  /** Durable "an integration conflict happened at some point this run" marker.
   * `merge.status`/`status` only show the LIVE conflict state and are rewritten
   * once a resolver or re-merge succeeds, so telemetry (the improve loop's
   * mergeConflictRate) reads this instead. Set on first conflict, never cleared. */
  hadMergeConflict?: boolean;
};

export type MergeState = {
  status: "not-started" | "merged" | "conflict" | "failed";
  attemptedAt?: string;
  targetBranch?: string;
  strategy?: MergeStrategy;
  conflictKind?: "merge" | "rebase";
  conflictedFiles?: string[];
  operationId?: string;
  mergeChangeId?: string;
  /** Git commit exported for the integrated jj change. */
  localCommit?: string;
  /** Whether the configured remote bookmark contains localCommit. */
  pushed?: boolean;
  pushedAt?: string;
  error?: string;
};

export type SyncState = {
  status: "not-started" | "synced" | "conflict" | "failed";
  attemptedAt?: string;
  baseBranch?: string;
  conflictedFiles?: string[];
  error?: string;
};

export type RudderEvent = {
  ts: string;
  runId: string;
  nodeId?: string;
  type:
    | "run.created"
    | "run.started"
    | "run.continued"
    | "run.detached"
    | "steerer.waiting"
    | "steerer.prompt"
    | "planner.spec"
    | "backend.output"
    | "backend.error"
    | "backend.exit"
    | "verifier.result"
    | "run.completed"
    | "run.failed"
    | "run.cancelled"
    | "merge.result"
    | "sync.result"
    // Orchestrated DAG (Phase 4): node lifecycle, planning, scheduler, merge.
    | "node.created"
    | "node.ready"
    | "node.running"
    | "node.review"
    | "node.merged"
    | "node.blocked"
    | "node.failed"
    | "plan.proposed"
    | "plan.reconciled"
    | "schedule.tick"
    | "schedule.launched"
    | "schedule.softDelivered"
    | "merge.attempt"
    | "merge.merged"
    | "merge.conflict"
    // Phase 5a: conflict-resolver agent flow.
    | "resolver.spawned"
    | "resolver.resolved";
  message?: string;
  data?: JsonValue;
};

export type RunRequest = {
  run: RunRecord;
  prompt: string;
  contract: string;
};

export type BackendAdapter = {
  id: BackendId;
  verify(): Promise<{ ok: boolean; message: string }>;
  run(request: RunRequest, emit: (event: RudderEvent) => Promise<void>): Promise<number>;
};

export type SpecContract = {
  runId: string;
  task: string;
  goal: string; // one-line OBJECTIVE for the launch Objective line
  success: string; // verifiable SUCCESS / DONE-WHEN condition
  createdAt: string;
  repo: {
    root: string;
    branch: string;
    baseCommit: string;
    status: string[];
  };
  instructionsFiles: Array<{ path: string; content: string }>;
  acceptanceCriteria: string[];
  suggestedTests: string[];
};

export type VerificationResult = {
  satisfied: string[];
  missing: string[];
  notes: string;
  shouldContinue: boolean;
};

export type CloudAuthState = {
  version: 1;
  token: string;
  cloudUrl: string;
  defaultRuntime?: "fly" | "byo-vm";
  defaultRegion?: string;
  byocSshHost?: string;
  accountId?: string;
  email?: string;
  expiresAt?: string;
  updatedAt: string;
};

export type CloudSail = {
  id: string;
  status?: string;
  url?: string;
  branch?: string;
  createdAt?: string;
  updatedAt?: string;
  [key: string]: JsonValue | undefined;
};

// ---------------------------------------------------------------------------
// Localhost board (Phase 2): a read-only projection of the flat run list.
// These types are the authoritative contract shared with the Preact SPA.
// ---------------------------------------------------------------------------

export type BoardColumn = "todo" | "running" | "review" | "done";

export type ProjectEntry = {
  slug: string;
  repoRoot: string;
  name: string;
  addedAt: string;
};

export type ProjectsRegistry = {
  version: 1;
  projects: ProjectEntry[];
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

export type BoardNode = {
  /** Stable graph node id when this run belongs to the DAG; otherwise run id. */
  id: string;
  /** Concrete run id once a worker has launched. */
  runId?: string;
  title: string;
  status: RunStatus | NodeStatus;
  column: BoardColumn;
  blocked: boolean;
  backend: BackendId;
  model?: string;
  effort?: EffortLevel;
  lastLine: string | null;
  tokens: { input: number; output: number } | null;
  deps: { hard: string[]; soft: string[] };
  createdAt: string;
  updatedAt: string;
  worktree: { path: string; workspaceName?: string } | null;
  merge: MergeState | null;
  /** User-authored follow-up turns, oldest first. The initial task is omitted. */
  updates: Array<{ instruction: string; ts: string; source?: string }>;
};

export type BoardEdge = {
  from: string;
  to: string;
  kind: "hard" | "soft" | "judge";
};

export type PlanGate = {
  id: string;
  nodeId?: string;
  question: string;
  options?: string[];
  createdAt: string;
};

export type MemoryEntry = {
  text: string;
  owner?: string;
  ts?: string;
};

// One line of the live narration feed (.rudder/activity.jsonl), surfaced in the
// web board's Activity panel. `kind` distinguishes real conductor/steer events
// from the periodic liveness heartbeat so the UI can style them differently.
export type ActivityEntry = {
  text: string;
  kind: "action" | "heartbeat";
  ts?: string;
};

export type BoardSnapshot = {
  slug: string;
  name: string;
  generatedAt: string;
  nodes: BoardNode[];
  edges: BoardEdge[];
  gates: PlanGate[];
  memory: MemoryEntry[];
  activity?: ActivityEntry[];
};
