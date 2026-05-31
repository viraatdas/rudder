import { useEffect, useReducer, useRef, useState } from "preact/hooks";
import {
  type BoardEdge,
  type BoardNode,
  type BoardSnapshot,
  type MemoryEntry,
  type PlanGate,
  eventsUrl,
  fetchState,
} from "./types";

// ---------------------------------------------------------------------------
// Reducer store. nodes are a Map keyed by id for O(1) upsert/remove; the rest
// are replaced wholesale on SNAPSHOT / MEMORY frames.
// ---------------------------------------------------------------------------

export type BoardState = {
  nodes: Map<string, BoardNode>;
  edges: BoardEdge[];
  gates: PlanGate[];
  memory: MemoryEntry[];
  loaded: boolean;
};

export type Action =
  | { type: "SNAPSHOT"; snapshot: BoardSnapshot }
  | { type: "NODE_UPSERT"; node: BoardNode }
  | { type: "NODE_REMOVE"; id: string }
  | { type: "MEMORY"; memory: MemoryEntry[] };

export function initialState(): BoardState {
  return { nodes: new Map(), edges: [], gates: [], memory: [], loaded: false };
}

export function reducer(state: BoardState, action: Action): BoardState {
  switch (action.type) {
    case "SNAPSHOT": {
      const nodes = new Map<string, BoardNode>();
      for (const n of action.snapshot.nodes) nodes.set(n.id, n);
      return {
        nodes,
        edges: action.snapshot.edges ?? [],
        gates: action.snapshot.gates ?? [],
        memory: action.snapshot.memory ?? [],
        loaded: true,
      };
    }
    case "NODE_UPSERT": {
      const nodes = new Map(state.nodes);
      nodes.set(action.node.id, action.node);
      return { ...state, nodes };
    }
    case "NODE_REMOVE": {
      if (!state.nodes.has(action.id)) return state;
      const nodes = new Map(state.nodes);
      nodes.delete(action.id);
      return { ...state, nodes };
    }
    case "MEMORY":
      return { ...state, memory: action.memory };
    default:
      return state;
  }
}

export type ConnState = "connecting" | "live" | "reconnecting";

export type UseBoard = {
  state: BoardState;
  conn: ConnState;
  name: string;
};

// useBoardState: one GET /state to seed, then an EventSource for live deltas.
// No polling. On error EventSource auto-reconnects; we flag "reconnecting" and
// on the next open refetch /state so we never drift after a gap.
export function useBoardState(slug: string): UseBoard {
  const [state, dispatch] = useReducer(reducer, undefined, initialState);
  const [conn, setConn] = useState<ConnState>("connecting");
  const [name, setName] = useState<string>("");
  const hadOpenedRef = useRef(false);

  useEffect(() => {
    let cancelled = false;
    hadOpenedRef.current = false;

    async function seed() {
      try {
        const snap = await fetchState(slug);
        if (cancelled) return;
        setName(snap.name || slug);
        dispatch({ type: "SNAPSHOT", snapshot: snap });
      } catch {
        // EventSource snapshot frame will still seed us if this fails.
      }
    }

    void seed();

    const es = new EventSource(eventsUrl(slug));

    es.addEventListener("open", () => {
      if (cancelled) return;
      // Reconnect after a drop: re-seed from /state to catch missed frames.
      if (hadOpenedRef.current) void seed();
      hadOpenedRef.current = true;
      setConn("live");
    });

    es.addEventListener("snapshot", (e) => {
      if (cancelled) return;
      try {
        const snap = JSON.parse((e as MessageEvent).data) as BoardSnapshot;
        setName(snap.name || slug);
        dispatch({ type: "SNAPSHOT", snapshot: snap });
      } catch {
        /* ignore malformed frame */
      }
    });

    es.addEventListener("node.added", (e) => {
      if (cancelled) return;
      try {
        dispatch({ type: "NODE_UPSERT", node: JSON.parse((e as MessageEvent).data) });
      } catch {
        /* ignore */
      }
    });

    es.addEventListener("node.updated", (e) => {
      if (cancelled) return;
      try {
        dispatch({ type: "NODE_UPSERT", node: JSON.parse((e as MessageEvent).data) });
      } catch {
        /* ignore */
      }
    });

    es.addEventListener("node.removed", (e) => {
      if (cancelled) return;
      try {
        const data = JSON.parse((e as MessageEvent).data);
        const id = typeof data === "string" ? data : data.id;
        if (id) dispatch({ type: "NODE_REMOVE", id });
      } catch {
        /* ignore */
      }
    });

    es.addEventListener("memory.updated", (e) => {
      if (cancelled) return;
      try {
        const data = JSON.parse((e as MessageEvent).data);
        const memory: MemoryEntry[] = Array.isArray(data) ? data : data.memory ?? [];
        dispatch({ type: "MEMORY", memory });
      } catch {
        /* ignore */
      }
    });

    es.addEventListener("error", () => {
      if (cancelled) return;
      // EventSource reconnects on its own; surface the state, never blank out.
      setConn(hadOpenedRef.current ? "reconnecting" : "connecting");
    });

    return () => {
      cancelled = true;
      es.close();
    };
  }, [slug]);

  return { state, conn, name };
}
