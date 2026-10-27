import { markRaw } from 'vue';
import type { NotifyBatch, NotifyState } from '../../api/notify';
import { isNewer, pushCapped, stateMarkers } from './notifyLog';
import type { LogEntry } from './notifyLog';

/** The state of one notifications panel (one per tab), kept while the tab is open. */
export interface NotifyPanel {
  sessionId: string;
  /** The listener as last reported; `null` before the first answer. */
  state: NotifyState | null;
  /** The latest entries (capped); raw, never deeply reactive. */
  entries: LogEntry[];
  /** Bumped when `entries` changed in place. */
  version: number;
  /** Notifications received since the panel opened. */
  received: number;
  /** Notifications lost because the view fell behind: dropped by the backend, or let go while paused. */
  dropped: number;
  /** Entries pushed out of view by newer ones. */
  scrolledOut: number;
  paused: boolean;
  /** Entries that arrived while paused (capped like the view). */
  held: LogEntry[];
  heldDropped: number;
  /** Receive times of recent notifications, for the rate. */
  times: number[];
  busy: boolean;
}

let nextId = 1;

/** A new, empty panel for `sessionId`. */
export function newPanel(sessionId: string): NotifyPanel {
  return {
    sessionId,
    state: null,
    entries: markRaw([]),
    version: 0,
    received: 0,
    dropped: 0,
    scrolledOut: 0,
    paused: false,
    held: markRaw([]),
    heldDropped: 0,
    times: markRaw([]),
    busy: false,
  };
}

function add(panel: NotifyPanel, incoming: LogEntry[]): void {
  if (incoming.length === 0) return;
  if (panel.paused) panel.heldDropped += pushCapped(panel.held, incoming);
  else {
    panel.scrolledOut += pushCapped(panel.entries, incoming);
    panel.version += 1;
  }
}

/** Adds a batch of notifications (or holds it while paused). */
export function applyBatch(panel: NotifyPanel, batch: NotifyBatch, now = Date.now()): void {
  const incoming: LogEntry[] = batch.messages.map((m) => ({ ...m, type: 'message', id: nextId++ }));
  panel.received += batch.messages.length;
  panel.dropped += batch.dropped;
  pushCapped(
    panel.times,
    batch.messages.map(() => now),
    2000,
  );
  if (batch.dropped > 0) {
    incoming.push({
      type: 'gap',
      id: nextId++,
      at: now,
      text: `${batch.dropped} notification${batch.dropped === 1 ? ' was' : 's were'} dropped because the view fell behind.`,
    });
  }
  add(panel, incoming);
}

/**
 * Records a listener state, with markers for a reconnect or a stop. An older
 * state than the one shown (a command reply that lost the race with an event) is ignored.
 */
export function applyState(panel: NotifyPanel, state: NotifyState, now = Date.now()): void {
  if (!isNewer(panel.state, state)) return;
  const markers = stateMarkers(panel.state, state);
  panel.state = state;
  add(
    panel,
    markers.map((text) => ({ type: 'gap', id: nextId++, at: now, text })),
  );
}

/** Pauses or resumes the view; notifications keep arriving and are held (capped) while paused. */
export function setPaused(panel: NotifyPanel, paused: boolean): void {
  if (panel.paused === paused) return;
  panel.paused = paused;
  if (!paused) {
    panel.dropped += panel.heldDropped;
    panel.heldDropped = 0;
    panel.scrolledOut += pushCapped(panel.entries, panel.held.splice(0));
    panel.version += 1;
  }
}

/** Empties the view (the counters stay). */
export function clearPanel(panel: NotifyPanel): void {
  panel.entries.splice(0);
  panel.held.splice(0);
  panel.version += 1;
}

/** Notifications per second over the last `windowMs` (`times` ascending). */
export function rate(times: readonly number[], now: number, windowMs = 5000): number {
  let count = 0;
  for (let i = times.length - 1; i >= 0; i -= 1) {
    if ((times[i] ?? 0) < now - windowMs) break;
    count += 1;
  }
  return Math.round((count / (windowMs / 1000)) * 10) / 10;
}
