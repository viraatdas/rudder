import { useEffect, useMemo, useRef, useState } from "preact/hooks";
import {
  type BoardEdge,
  type BoardNode,
  type Column,
  type MemoryEntry,
  type ProjectSummary,
  fetchLog,
  fetchProjects,
  postCancel,
  postMerge,
  postTask,
} from "./types";
import { type ConnState, useBoardState } from "./store";

// ---------------------------------------------------------------------------
// Status helpers. Status is encoded by spine color + text label + column,
// never color alone. Labels are terse and machine-flavored.
// ---------------------------------------------------------------------------

const COLUMNS: { key: Column; label: string }[] = [
  { key: "todo", label: "Todo" },
  { key: "running", label: "Running" },
  { key: "review", label: "Review" },
  { key: "done", label: "Done" },
];

// Map a node's raw status string to a CSS status token (drives spine color).
function statusToken(node: BoardNode): string {
  if (node.merge && node.merge.status === "merge-conflict") return "conflict";
  if (node.blocked) return "blocked";
  switch (node.status) {
    case "running":
    case "steering":
    case "verifying":
    case "created":
      return "running";
    case "completed":
      return "review";
    case "merged":
      return "done";
    case "failed":
      return "failed";
    case "cancelled":
      return "failed";
    case "merge-conflict":
      return "conflict";
    default:
      return node.column; // todo / running / review / done
  }
}

function statusLabel(node: BoardNode): string {
  if (node.merge && node.merge.status === "merge-conflict") return "conflict";
  if (node.blocked) return "blocked";
  return node.status || node.column;
}

function isRunning(node: BoardNode): boolean {
  return ["running", "steering", "verifying", "created"].includes(node.status);
}

function fmtTokens(n: number): string {
  if (n >= 1000) return `${(n / 1000).toFixed(n >= 10000 ? 0 : 1)}k`;
  return String(n);
}

// ---------------------------------------------------------------------------
// ProjectIndex — landing view when no slug is set.
// ---------------------------------------------------------------------------

export function ProjectIndex() {
  const [projects, setProjects] = useState<ProjectSummary[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    fetchProjects()
      .then((r) => {
        if (!cancelled) setProjects(r.projects ?? []);
      })
      .catch((e) => {
        if (!cancelled) setError(String(e?.message ?? e));
      });
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <div class="page">
      <header class="topbar">
        <div class="brand">
          <span class="brand-mark">▰</span>
          <span class="brand-name">Rudder</span>
          <span class="brand-sub mono">board</span>
        </div>
      </header>

      <main class="index-main">
        <h1 class="index-title">Projects</h1>
        <p class="index-lede">Local agent runs, projected live. Pick a repo.</p>

        {error && <div class="banner banner-error mono">failed to load projects: {error}</div>}

        {projects === null && !error && (
          <div class="tile-grid">
            <SkeletonTile />
            <SkeletonTile />
            <SkeletonTile />
          </div>
        )}

        {projects !== null && projects.length === 0 && (
          <div class="empty">
            <div class="empty-title">no projects yet</div>
            <div class="empty-sub mono">start a run in any repo and it registers here</div>
          </div>
        )}

        {projects !== null && projects.length > 0 && (
          <div class="tile-grid">
            {projects.map((p) => (
              <ProjectTile key={p.slug} project={p} />
            ))}
          </div>
        )}
      </main>
    </div>
  );
}

function ProjectTile({ project }: { project: ProjectSummary }) {
  const c = project.counts;
  const open = () => {
    window.location.href = `/rudder/${encodeURIComponent(project.slug)}`;
  };
  const counts: { token: string; label: string; n: number }[] = [
    { token: "running", label: "running", n: c.running },
    { token: "review", label: "review", n: c.review },
    { token: "todo", label: "todo", n: c.todo },
    { token: "done", label: "done", n: c.done },
    { token: "blocked", label: "blocked", n: c.blocked },
    { token: "failed", label: "failed", n: c.failed },
  ];
  return (
    <button class="tile" onClick={open} type="button">
      <div class="tile-head">
        <span class="tile-name">{project.name}</span>
      </div>
      <div class="tile-path mono">{project.repoRoot}</div>
      <div class="tile-counts">
        {counts.map((x) => (
          <span key={x.label} class={`count count-${x.token}`} data-zero={x.n === 0}>
            <span class="count-n mono">{x.n}</span>
            <span class="count-label">{x.label}</span>
          </span>
        ))}
      </div>
    </button>
  );
}

function SkeletonTile() {
  return (
    <div class="tile tile-skeleton" aria-hidden="true">
      <div class="skel skel-line" style="width: 52%" />
      <div class="skel skel-line" style="width: 78%" />
      <div class="skel skel-row" />
    </div>
  );
}

// ---------------------------------------------------------------------------
// BoardView — the live board for one project.
// ---------------------------------------------------------------------------

type View = "board" | "nest" | "memory";

const VIEW_KEY = "rudder.board.view";

function loadView(): View {
  try {
    const v = localStorage.getItem(VIEW_KEY);
    if (v === "board" || v === "nest" || v === "memory") return v;
  } catch {
    /* localStorage unavailable (private mode, etc.) */
  }
  return "board";
}

export function BoardView({ slug }: { slug: string }) {
  const { state, conn, name } = useBoardState(slug);
  const [view, setViewRaw] = useState<View>(loadView);
  const [selected, setSelected] = useState<string | null>(null);

  const setView = (v: View) => {
    setViewRaw(v);
    try {
      localStorage.setItem(VIEW_KEY, v);
    } catch {
      /* ignore */
    }
  };

  const nodes = useMemo(() => Array.from(state.nodes.values()), [state.nodes]);
  const selectedNode = selected ? state.nodes.get(selected) ?? null : null;

  return (
    <div class="page board-page">
      <Toolbar
        slug={slug}
        name={name || slug}
        conn={conn}
        view={view}
        onView={setView}
        memoryCount={state.memory.length}
      />

      <main class="board-main">
        {!state.loaded && conn !== "reconnecting" ? (
          <BoardSkeleton />
        ) : view === "board" ? (
          <Board nodes={nodes} onOpen={setSelected} selected={selected} />
        ) : view === "nest" ? (
          <Nest nodes={nodes} edges={state.edges} onOpen={setSelected} selected={selected} />
        ) : (
          <MemoryView memory={state.memory} />
        )}
      </main>

      {selectedNode && (
        <CardDetail slug={slug} node={selectedNode} onClose={() => setSelected(null)} />
      )}
    </div>
  );
}

function Toolbar({
  slug,
  name,
  conn,
  view,
  onView,
  memoryCount,
}: {
  slug: string;
  name: string;
  conn: ConnState;
  view: View;
  onView: (v: View) => void;
  memoryCount: number;
}) {
  const [composerOpen, setComposerOpen] = useState(false);
  return (
    <header class="topbar board-topbar">
      <div class="brand">
        <a class="brand-link" href="/rudder" title="All projects">
          <span class="brand-mark">▰</span>
        </a>
        <span class="brand-name">{name}</span>
        <ConnPill conn={conn} />
      </div>

      <div class="toolbar-actions">
        <div class="view-toggle" role="tablist" aria-label="View">
          <button
            type="button"
            role="tab"
            aria-selected={view === "board"}
            class={`toggle ${view === "board" ? "toggle-on" : ""}`}
            onClick={() => onView("board")}
          >
            Board
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={view === "nest"}
            class={`toggle ${view === "nest" ? "toggle-on" : ""}`}
            onClick={() => onView("nest")}
            title="Nest / DAG view"
          >
            Nest
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={view === "memory"}
            class={`toggle ${view === "memory" ? "toggle-on" : ""}`}
            onClick={() => onView("memory")}
          >
            Memory
            {memoryCount > 0 && <span class="toggle-badge mono">{memoryCount}</span>}
          </button>
        </div>

        <button type="button" class="btn btn-accent" onClick={() => setComposerOpen(true)}>
          + Task
        </button>
      </div>

      {composerOpen && <Composer slug={slug} onClose={() => setComposerOpen(false)} />}
    </header>
  );
}

function ConnPill({ conn }: { conn: ConnState }) {
  if (conn === "live") {
    return (
      <span class="conn conn-live" title="Live">
        <span class="conn-dot" />
        <span class="conn-label mono">live</span>
      </span>
    );
  }
  const label = conn === "connecting" ? "connecting" : "reconnecting";
  return (
    <span class="conn conn-warn" title={label}>
      <span class="conn-dot conn-dot-pulse" />
      <span class="conn-label mono">{label}</span>
    </span>
  );
}

// ---------------------------------------------------------------------------
// Composer — POST a prompt to create a task. SSE surfaces the resulting node.
// ---------------------------------------------------------------------------

function Composer({ slug, onClose }: { slug: string; onClose: () => void }) {
  const [prompt, setPrompt] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const ref = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    ref.current?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const submit = async () => {
    const text = prompt.trim();
    if (!text || busy) return;
    setBusy(true);
    setError(null);
    try {
      await postTask(slug, text);
      onClose();
    } catch (e: any) {
      setError(String(e?.message ?? e));
      setBusy(false);
    }
  };

  const onKeyDown = (e: KeyboardEvent) => {
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
      e.preventDefault();
      void submit();
    }
  };

  return (
    <div class="overlay" onClick={onClose}>
      <div class="composer" onClick={(e) => e.stopPropagation()}>
        <div class="composer-head">
          <span class="composer-title">New task</span>
          <span class="composer-hint mono">⌘⏎ to run · esc to close</span>
        </div>
        <textarea
          ref={ref}
          class="composer-input mono"
          rows={5}
          placeholder="Describe the task. The agent decomposes and schedules it."
          value={prompt}
          onInput={(e) => setPrompt((e.target as HTMLTextAreaElement).value)}
          onKeyDown={onKeyDown}
          disabled={busy}
        />
        {error && <div class="banner banner-error mono">{error}</div>}
        <div class="composer-actions">
          <button type="button" class="btn" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button type="button" class="btn btn-accent" onClick={submit} disabled={busy || !prompt.trim()}>
            {busy ? "Running…" : "Run task"}
          </button>
        </div>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Board — 4 columns. Blocked is an overlay flag, not a 5th column.
// ---------------------------------------------------------------------------

function Board({
  nodes,
  onOpen,
  selected,
}: {
  nodes: BoardNode[];
  onOpen: (id: string) => void;
  selected: string | null;
}) {
  const byColumn = useMemo(() => {
    const m: Record<Column, BoardNode[]> = { todo: [], running: [], review: [], done: [] };
    for (const n of nodes) {
      (m[n.column] ?? m.todo).push(n);
    }
    for (const k of Object.keys(m) as Column[]) {
      m[k].sort((a, b) => (a.updatedAt < b.updatedAt ? 1 : -1));
    }
    return m;
  }, [nodes]);

  return (
    <div class="board">
      {COLUMNS.map((col) => {
        const items = byColumn[col.key];
        return (
          <section class="column" key={col.key} data-column={col.key}>
            <header class="column-head">
              <span class="column-title">{col.label}</span>
              <span class="column-count mono">{items.length}</span>
            </header>
            <div class="column-body">
              {items.length === 0 ? (
                <div class="column-empty mono">empty</div>
              ) : (
                items.map((n) => (
                  <Card key={n.id} node={n} onOpen={onOpen} selected={selected === n.id} />
                ))
              )}
            </div>
          </section>
        );
      })}
    </div>
  );
}

function Card({
  node,
  onOpen,
  selected,
}: {
  node: BoardNode;
  onOpen: (id: string) => void;
  selected: boolean;
}) {
  const token = statusToken(node);
  const running = isRunning(node);
  const depCount = node.deps.hard.length + node.deps.soft.length;
  return (
    <article
      class={`card ${selected ? "card-selected" : ""}`}
      data-status={token}
      tabIndex={0}
      onClick={() => onOpen(node.id)}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onOpen(node.id);
        }
      }}
    >
      <div class="card-spine" data-status={token} />
      <div class="card-body">
        <div class="card-top">
          <span class="card-status mono" data-status={token}>
            {statusLabel(node)}
          </span>
          {node.blocked && <span class="flag flag-blocked mono">blocked</span>}
        </div>

        <h3 class="card-title">{node.title}</h3>

        <div class="card-meta mono">
          <span class="card-backend">{node.backend}</span>
          {node.model && <span class="card-model">{node.model}</span>}
          {node.effort && <span class="card-effort">{node.effort}</span>}
        </div>

        {node.lastLine && (
          <div class="card-lastline mono">
            {running && <span class="pulse-dot" aria-hidden="true" />}
            <span class="lastline-text">{node.lastLine}</span>
          </div>
        )}

        <div class="card-foot">
          {node.tokens && (
            <span class="tokens mono" title="input / output tokens">
              <span class="tok-in">↓{fmtTokens(node.tokens.input)}</span>
              <span class="tok-out">↑{fmtTokens(node.tokens.output)}</span>
            </span>
          )}
          {depCount > 0 && (
            <span class="dep-badge mono" title="upstream dependencies">
              {node.deps.hard.length > 0 && (
                <span class="dep dep-hard" title={`${node.deps.hard.length} hard dependency(ies)`}>
                  ↑{node.deps.hard.length} hard
                </span>
              )}
              {node.deps.soft.length > 0 && (
                <span class="dep dep-soft" title={`${node.deps.soft.length} soft dependency(ies)`}>
                  ↑{node.deps.soft.length} soft
                </span>
              )}
            </span>
          )}
        </div>
      </div>
    </article>
  );
}

// ---------------------------------------------------------------------------
// Nest — the dependency DAG rendered as an SVG. Roots on the left, children
// after their hard parents. Layout is a simple longest-path layering over the
// HARD dependency edges; soft edges are drawn but never push a node deeper.
// ---------------------------------------------------------------------------

// Node box geometry (SVG user units, 1:1 with px since viewBox tracks size).
const NEST = {
  boxW: 220,
  boxH: 64,
  gapX: 96, // horizontal gap between layers
  gapY: 26, // vertical gap between stacked nodes in a layer
  padX: 40, // outer padding
  padY: 40,
  spineW: 5, // status spine on the left edge of each box
};

type NestLayout = {
  pos: Map<string, { x: number; y: number }>;
  width: number;
  height: number;
};

// Longest-path layering: layer(n) = 1 + max(layer of hard-parents), 0 for roots.
// Soft edges are ignored for layering. Cycles (which a DAG should not contain,
// but we never trust input) are broken by visited-tracking so we always halt.
function layerNodes(nodes: BoardNode[], present: Set<string>): Map<string, number> {
  // hard parents per node, restricted to nodes actually present on the board.
  const hardParents = new Map<string, string[]>();
  for (const n of nodes) {
    hardParents.set(
      n.id,
      n.deps.hard.filter((p) => present.has(p) && p !== n.id)
    );
  }

  const layer = new Map<string, number>();
  const computing = new Set<string>();

  const resolve = (id: string): number => {
    const cached = layer.get(id);
    if (cached !== undefined) return cached;
    if (computing.has(id)) return 0; // cycle guard: treat back-edge as a root
    computing.add(id);
    const parents = hardParents.get(id) ?? [];
    let max = -1;
    for (const p of parents) {
      max = Math.max(max, resolve(p));
    }
    computing.delete(id);
    const value = max + 1; // roots (no hard parents) -> 0
    layer.set(id, value);
    return value;
  };

  for (const n of nodes) resolve(n.id);
  return layer;
}

// Build absolute box positions: x by layer, y stacked within a layer. Within a
// layer, keep a stable order (by createdAt then id) so the graph does not jump.
function computeLayout(nodes: BoardNode[]): NestLayout {
  const present = new Set(nodes.map((n) => n.id));
  const layer = layerNodes(nodes, present);

  const byLayer = new Map<number, BoardNode[]>();
  for (const n of nodes) {
    const l = layer.get(n.id) ?? 0;
    const bucket = byLayer.get(l);
    if (bucket) bucket.push(n);
    else byLayer.set(l, [n]);
  }

  const pos = new Map<string, { x: number; y: number }>();
  const layers = Array.from(byLayer.keys()).sort((a, b) => a - b);
  let maxBottom = 0;

  for (const l of layers) {
    const bucket = byLayer.get(l)!;
    bucket.sort((a, b) => {
      if (a.createdAt !== b.createdAt) return a.createdAt < b.createdAt ? -1 : 1;
      return a.id < b.id ? -1 : 1;
    });
    const x = NEST.padX + l * (NEST.boxW + NEST.gapX);
    let y = NEST.padY;
    for (const n of bucket) {
      pos.set(n.id, { x, y });
      y += NEST.boxH + NEST.gapY;
    }
    maxBottom = Math.max(maxBottom, y - NEST.gapY);
  }

  const maxLayer = layers.length ? layers[layers.length - 1] : 0;
  const width = NEST.padX * 2 + (maxLayer + 1) * NEST.boxW + maxLayer * NEST.gapX;
  const height = Math.max(maxBottom + NEST.padY, NEST.padY * 2 + NEST.boxH);
  return { pos, width, height };
}

function truncate(s: string, max: number): string {
  if (s.length <= max) return s;
  return `${s.slice(0, Math.max(0, max - 1)).trimEnd()}…`;
}

function Nest({
  nodes,
  edges,
  onOpen,
  selected,
}: {
  nodes: BoardNode[];
  edges: BoardEdge[];
  onOpen: (id: string) => void;
  selected: string | null;
}) {
  const layout = useMemo(() => computeLayout(nodes), [nodes]);
  const present = useMemo(() => new Set(nodes.map((n) => n.id)), [nodes]);

  if (nodes.length === 0) {
    return (
      <div class="empty">
        <div class="empty-title">no tasks in the graph yet</div>
        <div class="empty-sub mono">the dependency DAG renders here once tasks exist</div>
      </div>
    );
  }

  // Only draw edges whose endpoints are both on the board.
  const drawEdges = edges.filter((e) => present.has(e.from) && present.has(e.to));

  // Edge path: from the right edge of the parent box to the left edge of the
  // child box. A flat cubic curve reads as a wire without needing a layout lib.
  const edgePath = (e: BoardEdge): string | null => {
    const a = layout.pos.get(e.from);
    const b = layout.pos.get(e.to);
    if (!a || !b) return null;
    const x1 = a.x + NEST.boxW;
    const y1 = a.y + NEST.boxH / 2;
    const x2 = b.x;
    const y2 = b.y + NEST.boxH / 2;
    const dx = Math.max(28, (x2 - x1) / 2);
    return `M ${x1} ${y1} C ${x1 + dx} ${y1}, ${x2 - dx} ${y2}, ${x2} ${y2}`;
  };

  return (
    <div class="nest">
      <svg
        class="nest-svg"
        width={layout.width}
        height={layout.height}
        viewBox={`0 0 ${layout.width} ${layout.height}`}
        role="img"
        aria-label="Task dependency graph"
      >
        <defs>
          <marker
            id="arrow"
            viewBox="0 0 10 10"
            refX="9"
            refY="5"
            markerWidth="7"
            markerHeight="7"
            orient="auto-start-reverse"
          >
            <path d="M 0 0 L 10 5 L 0 10 z" class="arrow-head" />
          </marker>
          <marker
            id="arrow-soft"
            viewBox="0 0 10 10"
            refX="9"
            refY="5"
            markerWidth="6"
            markerHeight="6"
            orient="auto-start-reverse"
          >
            <path d="M 0 0 L 10 5 L 0 10 z" class="arrow-head-soft" />
          </marker>
        </defs>

        {/* edges first so node boxes sit on top */}
        <g class="nest-edges">
          {drawEdges.map((e, i) => {
            const d = edgePath(e);
            if (!d) return null;
            return (
              <path
                key={`${e.from}->${e.to}:${e.kind}:${i}`}
                d={d}
                fill="none"
                class={e.kind === "hard" ? "edge edge-hard" : "edge edge-soft"}
              />
            );
          })}
        </g>

        <g class="nest-nodes">
          {nodes.map((n) => {
            const p = layout.pos.get(n.id);
            if (!p) return null;
            const token = statusToken(n);
            const isSel = selected === n.id;
            const onActivate = () => onOpen(n.id);
            return (
              <g
                key={n.id}
                class={`node ${isSel ? "node-selected" : ""}`}
                data-status={token}
                transform={`translate(${p.x} ${p.y})`}
                tabIndex={0}
                role="button"
                aria-label={`${statusLabel(n)}: ${n.title}`}
                onClick={onActivate}
                onKeyDown={(ev) => {
                  if (ev.key === "Enter" || ev.key === " ") {
                    ev.preventDefault();
                    onActivate();
                  }
                }}
              >
                <rect class="node-box" x="0" y="0" width={NEST.boxW} height={NEST.boxH} />
                <rect class="node-spine" x="0" y="0" width={NEST.spineW} height={NEST.boxH} />
                <text class="node-status mono" x={NEST.spineW + 12} y="22">
                  {statusLabel(n)}
                </text>
                <text class="node-title" x={NEST.spineW + 12} y="44">
                  {truncate(n.title, 30)}
                </text>
              </g>
            );
          })}
        </g>
      </svg>
    </div>
  );
}

// ---------------------------------------------------------------------------
// CardDetail — right drawer with live log tail, run info, merge/cancel.
// ---------------------------------------------------------------------------

function CardDetail({
  slug,
  node,
  onClose,
}: {
  slug: string;
  node: BoardNode;
  onClose: () => void;
}) {
  const [log, setLog] = useState<string | null>(null);
  const [logError, setLogError] = useState<string | null>(null);
  const [action, setAction] = useState<{ busy: boolean; msg: string | null; err: string | null }>({
    busy: false,
    msg: null,
    err: null,
  });
  const token = statusToken(node);
  const running = isRunning(node);
  const completed = node.status === "completed";

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  useEffect(() => {
    let cancelled = false;
    setLog(null);
    setLogError(null);
    fetchLog(slug, node.id, 200)
      .then((t) => {
        if (!cancelled) setLog(t);
      })
      .catch((e) => {
        if (!cancelled) setLogError(String(e?.message ?? e));
      });
    return () => {
      cancelled = true;
    };
  }, [slug, node.id]);

  // Append the live lastLine so the drawer feels live between log backfills.
  const liveLog = useMemo(() => {
    const base = log ?? "";
    if (!node.lastLine) return base;
    if (base.trimEnd().endsWith(node.lastLine.trimEnd())) return base;
    return base ? `${base}\n${node.lastLine}` : node.lastLine;
  }, [log, node.lastLine]);

  const doMerge = async () => {
    setAction({ busy: true, msg: null, err: null });
    try {
      const r = await postMerge(slug, node.id);
      if (r.status === "merge-conflict" && r.conflictedFiles?.length) {
        setAction({ busy: false, msg: null, err: `conflict in ${r.conflictedFiles.length} file(s)` });
      } else {
        setAction({ busy: false, msg: r.status, err: null });
      }
    } catch (e: any) {
      setAction({ busy: false, msg: null, err: String(e?.message ?? e) });
    }
  };

  const doCancel = async () => {
    setAction({ busy: true, msg: null, err: null });
    try {
      await postCancel(slug, node.id);
      setAction({ busy: false, msg: "cancel requested", err: null });
    } catch (e: any) {
      setAction({ busy: false, msg: null, err: String(e?.message ?? e) });
    }
  };

  return (
    <div class="drawer-overlay" onClick={onClose}>
      <aside class="drawer" onClick={(e) => e.stopPropagation()} data-status={token}>
        <div class="drawer-spine" data-status={token} />
        <header class="drawer-head">
          <div class="drawer-head-top">
            <span class="card-status mono" data-status={token}>
              {statusLabel(node)}
            </span>
            <button type="button" class="drawer-close" onClick={onClose} aria-label="Close">
              ✕
            </button>
          </div>
          <h2 class="drawer-title">{node.title}</h2>
          <div class="drawer-meta mono">
            <span>{node.backend}</span>
            {node.model && <span>{node.model}</span>}
            {node.effort && <span>{node.effort}</span>}
            {node.tokens && (
              <span>
                ↓{fmtTokens(node.tokens.input)} ↑{fmtTokens(node.tokens.output)}
              </span>
            )}
          </div>
          {node.worktree && (
            <div class="drawer-worktree mono" title={node.worktree.path}>
              {node.worktree.workspaceName ?? node.worktree.path}
            </div>
          )}
        </header>

        <div class="drawer-actions">
          {completed && (
            <button type="button" class="btn btn-accent" onClick={doMerge} disabled={action.busy}>
              {action.busy ? "Merging…" : "Merge"}
            </button>
          )}
          {running && (
            <button type="button" class="btn btn-danger" onClick={doCancel} disabled={action.busy}>
              {action.busy ? "…" : "Cancel"}
            </button>
          )}
          {action.msg && <span class="action-msg mono">{action.msg}</span>}
          {action.err && <span class="action-err mono">{action.err}</span>}
        </div>

        <div class="drawer-log">
          <div class="drawer-log-head mono">
            <span>log</span>
            {running && <span class="pulse-dot" aria-hidden="true" />}
          </div>
          <LogPane text={liveLog} error={logError} loading={log === null && !logError} />
        </div>
      </aside>
    </div>
  );
}

function LogPane({ text, error, loading }: { text: string; error: string | null; loading: boolean }) {
  const ref = useRef<HTMLPreElement>(null);
  useEffect(() => {
    const el = ref.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [text]);

  if (error) return <div class="banner banner-error mono">log unavailable: {error}</div>;
  if (loading) {
    return (
      <pre class="log-pre mono">
        <div class="skel skel-line" style="width:70%" />
        <div class="skel skel-line" style="width:90%" />
        <div class="skel skel-line" style="width:55%" />
      </pre>
    );
  }
  return (
    <pre ref={ref} class="log-pre mono">
      {text || "no output yet"}
    </pre>
  );
}

// ---------------------------------------------------------------------------
// MemoryView — beads-style shared decisions.
// ---------------------------------------------------------------------------

function MemoryView({ memory }: { memory: MemoryEntry[] }) {
  if (memory.length === 0) {
    return (
      <div class="empty">
        <div class="empty-title">no decisions recorded yet</div>
        <div class="empty-sub mono">agents append cross-cutting decisions here as they work</div>
      </div>
    );
  }
  return (
    <div class="memory">
      {memory.map((m, i) => (
        <article class="memory-entry" key={i}>
          <div class="memory-bead" aria-hidden="true" />
          <div class="memory-content">
            <p class="memory-text">{m.text}</p>
            <div class="memory-meta mono">
              {m.owner && <span class="memory-owner">{m.owner}</span>}
              {m.ts && <span class="memory-ts">{m.ts}</span>}
            </div>
          </div>
        </article>
      ))}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Loading skeletons — block-shadow outlines, no gradient shimmer.
// ---------------------------------------------------------------------------

function BoardSkeleton() {
  return (
    <div class="board" aria-hidden="true">
      {COLUMNS.map((col) => (
        <section class="column" key={col.key}>
          <header class="column-head">
            <span class="column-title">{col.label}</span>
            <span class="column-count mono">·</span>
          </header>
          <div class="column-body">
            <div class="card card-skeleton">
              <div class="skel skel-line" style="width:60%" />
              <div class="skel skel-line" style="width:85%" />
            </div>
            {col.key === "running" && (
              <div class="card card-skeleton">
                <div class="skel skel-line" style="width:70%" />
                <div class="skel skel-line" style="width:50%" />
              </div>
            )}
          </div>
        </section>
      ))}
    </div>
  );
}
