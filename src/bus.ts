import { EventEmitter } from "node:events";
import type { RudderEvent } from "./types.js";

// ---------------------------------------------------------------------------
// In-process event bus (Phase 4). The daemon owns one RudderBus; the scheduler
// and jj/merge ops publish onto it, and the board SSE broadcaster subscribes.
// The file substrate stays durable (run.json / events.ndjson); the bus is an
// accelerator so projections do not have to poll.
// ---------------------------------------------------------------------------

const CHANNEL = "rudder";

export class RudderBus extends EventEmitter {
  publish(event: RudderEvent): void {
    this.emit(CHANNEL, event);
  }

  /**
   * Subscribe to every published event. Returns an unsubscribe function.
   */
  subscribe(fn: (event: RudderEvent) => void): () => void {
    this.on(CHANNEL, fn);
    return () => {
      this.off(CHANNEL, fn);
    };
  }
}
