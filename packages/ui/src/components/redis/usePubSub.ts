import { listen } from '@tauri-apps/api/event';
import type { UnlistenFn } from '@tauri-apps/api/event';
import { markRaw, nextTick, reactive } from 'vue';
import {
  PUBSUB_EVENT,
  redisPubsubAck,
  redisPubsubChange,
  redisPubsubSubscribe,
  redisPubsubSubscriptions,
  redisPubsubUnsubscribe,
} from '../../api/redis';
import type { PubSubEvent, PubSubNameKind, PubSubNames } from '../../api/redis';
import { MAX_VIEW_MESSAGES, pushCapped } from '../../lib/redisPubSub';
import type { ReceivedMessage, ViewEntry } from '../../lib/redisPubSub';
import { redisErrorView } from '../../lib/redisErrors';
import { useTabs } from '../../composables/useTabs';

/** One channel or pattern of a panel. */
export interface PubSubName {
  kind: PubSubNameKind;
  name: string;
  /** True once the server confirmed the subscription. */
  confirmed: boolean;
  /** Messages received through it. */
  messages: number;
}

/** The state of one Pub/Sub panel (one per tab), kept while the tab is open. */
export interface PubSubPanel {
  sessionId: string;
  subscriptionId: string | null;
  names: PubSubName[];
  /** The latest messages and gap markers (at most `MAX_VIEW_MESSAGES`); raw, never deeply reactive. */
  messages: ViewEntry[];
  /** Bumped when `messages` changed in place. */
  version: number;
  received: number;
  /** Messages the backend dropped because the view fell behind, plus those the view let go. */
  droppedByBackend: number;
  paused: boolean;
  /** Messages and gaps that arrived while paused (capped like the view). */
  held: ViewEntry[];
  heldDropped: number;
  /** Receive times of recent messages, for the rate. */
  times: number[];
  startedAt: number | null;
  busy: boolean;
  errors: string[];
  /** Why the subscription ended, when it did. */
  closed: string | null;
  /** Set while Varve opens a lost connection again. */
  reconnecting: { attempt: number; reason: string } | null;
}

const panels = reactive(new Map<string, PubSubPanel>());
let unlisten: UnlistenFn | null = null;
let listening: Promise<void> | null = null;
let nextMessageId = 1;
/** Events for subscriptions whose subscribe call has not returned yet. */
const early = new Map<string, PubSubEvent[]>();

function panelOf(subscriptionId: string): PubSubPanel | null {
  for (const panel of panels.values()) if (panel.subscriptionId === subscriptionId) return panel;
  return null;
}

function nameOf(panel: PubSubPanel, kind: PubSubNameKind, name: string): PubSubName | undefined {
  return panel.names.find((n) => n.kind === kind && n.name === name);
}

/** Applies one backend event to its panel; batches are acknowledged once shown. */
function apply(panel: PubSubPanel, event: PubSubEvent): void {
  switch (event.kind) {
    case 'batch': {
      const now = Date.now();
      const incoming: ReceivedMessage[] = event.messages.map((m) => ({ ...m, id: nextMessageId++, entry: 'message' as const }));
      panel.received += incoming.length;
      panel.droppedByBackend += event.dropped;
      for (const m of incoming) {
        const via =
          m.pattern === null ? (nameOf(panel, 'channel', m.channel) ?? nameOf(panel, 'shardChannel', m.channel)) : nameOf(panel, 'pattern', m.pattern);
        if (via) via.messages += 1;
      }
      pushCapped(panel.times, incoming.map(() => now), 2000);
      if (panel.paused) panel.heldDropped += pushCapped(panel.held, incoming);
      else {
        pushCapped(panel.messages, incoming);
        panel.version += 1;
      }
      const id = event.subscriptionId;
      const seq = event.seq;
      // Acknowledge after the batch was rendered, so the backend never runs ahead of the view.
      void nextTick().then(() => redisPubsubAck(id, seq).catch(() => undefined));
      break;
    }
    case 'subscribed': {
      const hit = nameOf(panel, event.nameKind, event.name);
      if (hit) hit.confirmed = true;
      else panel.names.push({ kind: event.nameKind, name: event.name, confirmed: true, messages: 0 });
      break;
    }
    case 'unsubscribed':
      panel.names = panel.names.filter((n) => !(n.kind === event.nameKind && n.name === event.name));
      break;
    case 'reconnecting':
      panel.reconnecting = { attempt: event.attempt, reason: event.reason };
      for (const n of panel.names) n.confirmed = false;
      break;
    case 'gap': {
      panel.reconnecting = null;
      const marker: ViewEntry = { id: nextMessageId++, entry: 'gap', reason: event.reason, lostAt: event.lostAt, resumedAt: event.resumedAt };
      if (panel.paused) panel.heldDropped += pushCapped(panel.held, [marker]);
      else {
        pushCapped(panel.messages, [marker]);
        panel.version += 1;
      }
      break;
    }
    case 'error': {
      const text = event.code ? `${event.message} (${event.code})` : event.message;
      const refused = event.nameKind && event.name !== undefined && event.name !== null ? { kind: event.nameKind, name: event.name } : null;
      // A refused name is no longer part of the subscription.
      if (refused) panel.names = panel.names.filter((n) => !(n.kind === refused.kind && n.name === refused.name));
      panel.errors = [...panel.errors.slice(-4), refused ? `${refused.name}: ${text}` : text];
      break;
    }
    case 'closed':
      panel.subscriptionId = null;
      panel.reconnecting = null;
      panel.closed = event.reason === 'unsubscribed' ? null : event.reason;
      for (const n of panel.names) n.confirmed = false;
      if (event.reason === 'unsubscribed') panel.names = [];
      break;
  }
}

function dispatch(event: PubSubEvent): void {
  const panel = panelOf(event.subscriptionId);
  if (panel) {
    apply(panel, event);
    return;
  }
  // The subscribe call may not have returned yet: keep the events briefly.
  const queue = early.get(event.subscriptionId) ?? [];
  queue.push(event);
  early.set(event.subscriptionId, queue);
  setTimeout(() => early.delete(event.subscriptionId), 5000);
}

function ensureListening(): Promise<void> {
  if (!listening) {
    listening = listen<PubSubEvent>(PUBSUB_EVENT, (e) => dispatch(e.payload)).then((fn) => {
      unlisten = fn;
    });
  }
  return listening;
}

/** The message of a failed call; a lost session or reconnect is reported to the tab `key`. */
function describe(key: string, e: unknown): string {
  const view = redisErrorView(useTabs().reportTabError(key, e));
  return view.hint ? `${view.message} ${view.hint}` : view.message;
}

/** The panel of `key` (a tab id), created on first use for `sessionId`. */
function panel(key: string, sessionId: string): PubSubPanel {
  const existing = panels.get(key);
  if (existing && existing.sessionId === sessionId) return existing;
  if (existing?.subscriptionId) void redisPubsubUnsubscribe(existing.subscriptionId).catch(() => undefined);
  panels.set(key, {
    sessionId,
    subscriptionId: null,
    names: [],
    messages: markRaw([]),
    version: 0,
    received: 0,
    droppedByBackend: 0,
    paused: false,
    held: markRaw([]),
    heldDropped: 0,
    times: markRaw([]),
    startedAt: null,
    busy: false,
    errors: [],
    closed: null,
    reconnecting: null,
  });
  return panels.get(key) as PubSubPanel;
}

const NO_NAMES: PubSubNames = { channels: [], patterns: [], shardChannels: [] };

/** The names of one kind in a `PubSubNames`. */
function listOf(names: PubSubNames, kind: PubSubNameKind): string[] {
  if (kind === 'channel') return names.channels;
  if (kind === 'pattern') return names.patterns;
  return names.shardChannels ?? [];
}

/** Panel names as `PubSubNames`. */
export function namesOf(list: readonly Pick<PubSubName, 'kind' | 'name'>[]): Required<PubSubNames> {
  return {
    channels: list.filter((n) => n.kind === 'channel').map((n) => n.name),
    patterns: list.filter((n) => n.kind === 'pattern').map((n) => n.name),
    shardChannels: list.filter((n) => n.kind === 'shardChannel').map((n) => n.name),
  };
}

/** Subscribes to more channels or patterns, opening the subscription connection on first use. */
async function subscribe(key: string, names: PubSubNames): Promise<string | null> {
  const p = panels.get(key);
  if (!p || p.busy) return null;
  p.busy = true;
  try {
    return await subscribeNames(key, p, names);
  } finally {
    p.busy = false;
  }
}

/** The work of {@link subscribe}; the caller holds `p.busy`. */
async function subscribeNames(key: string, p: PubSubPanel, names: PubSubNames): Promise<string | null> {
  await ensureListening();
  const fresh = {
    channels: names.channels.filter((n) => !nameOf(p, 'channel', n)),
    patterns: names.patterns.filter((n) => !nameOf(p, 'pattern', n)),
    shardChannels: (names.shardChannels ?? []).filter((n) => !nameOf(p, 'shardChannel', n)),
  };
  if (fresh.channels.length === 0 && fresh.patterns.length === 0 && fresh.shardChannels.length === 0) return null;
  for (const n of fresh.channels) p.names.push({ kind: 'channel', name: n, confirmed: false, messages: 0 });
  for (const n of fresh.patterns) p.names.push({ kind: 'pattern', name: n, confirmed: false, messages: 0 });
  for (const n of fresh.shardChannels) p.names.push({ kind: 'shardChannel', name: n, confirmed: false, messages: 0 });
  try {
    if (p.subscriptionId) await redisPubsubChange(p.subscriptionId, fresh, NO_NAMES);
    else {
      // Names that lost their connection subscribe again together with the new ones.
      const all = namesOf(p.names);
      const sub = await redisPubsubSubscribe(p.sessionId, all);
      p.subscriptionId = sub.subscriptionId;
      p.closed = null;
      p.startedAt ??= Date.now();
      for (const event of early.get(sub.subscriptionId) ?? []) apply(p, event);
      early.delete(sub.subscriptionId);
    }
    return null;
  } catch (e: unknown) {
    // The names this call added and the server never confirmed go away again.
    p.names = p.names.filter((n) => n.confirmed || !listOf(fresh, n.kind).includes(n.name));
    return describe(key, e);
  }
}

/** Sessions whose backend subscriptions were already checked in this webview. */
const checked = new Set<string>();

/**
 * After the panel store was lost (a reloaded UI module; a reloaded page closes its sessions in the backend) the panel store is empty while the backend still
 * runs the tab's subscriptions (they count against the limits and are never
 * acknowledged). The first time a session's panel is shown, the backend's
 * list is read: those subscriptions are closed and their names subscribed
 * again on one fresh subscription of this panel (a fresh one, because the
 * acknowledgements the old view owed were lost with it), with a note that
 * messages in between were missed. Resolves with an error message or `null`.
 */
async function reattach(key: string): Promise<string | null> {
  const p = panels.get(key);
  // A subscribe in flight: check again the next time the panel shows.
  if (!p || p.busy || checked.has(p.sessionId)) return null;
  checked.add(p.sessionId);
  // The panel is busy throughout, so the user cannot subscribe in between.
  p.busy = true;
  try {
    return await takeOver(key, p);
  } finally {
    p.busy = false;
  }
}

async function takeOver(key: string, p: PubSubPanel): Promise<string | null> {
  let found;
  try {
    found = await redisPubsubSubscriptions(p.sessionId);
  } catch (e: unknown) {
    checked.delete(p.sessionId);
    return describe(key, e);
  }
  const owned = new Set([...panels.values()].map((x) => x.subscriptionId).filter((id): id is string => id !== null));
  const orphans = found.filter((s) => !owned.has(s.subscriptionId));
  if (orphans.length === 0) return null;
  const names: Required<PubSubNames> = { channels: [], patterns: [], shardChannels: [] };
  for (const o of orphans) {
    for (const [list, from] of [
      [names.channels, o.channels],
      [names.patterns, o.patterns],
      [names.shardChannels, o.shardChannels],
    ] as const) {
      for (const n of from) if (!list.includes(n)) list.push(n);
    }
  }
  await Promise.all(orphans.map((o) => redisPubsubUnsubscribe(o.subscriptionId).catch(() => undefined)));
  if (names.channels.length + names.patterns.length + names.shardChannels.length === 0) return null;
  const error = await subscribeNames(key, p, names);
  // Only when every name is part of the panel again.
  const all = namesOf(p.names);
  const back = (['channels', 'patterns', 'shardChannels'] as const).every((kind) => names[kind].every((n) => all[kind].includes(n)));
  if (error === null && back) {
    p.errors = [...p.errors.slice(-4), "The tab's earlier subscriptions were opened again; messages published in between were missed."];
  }
  return error;
}

/** Stops one channel or pattern; the last one closes the subscription connection. */
async function unsubscribe(key: string, target: PubSubName): Promise<string | null> {
  const p = panels.get(key);
  if (!p) return null;
  const id = p.subscriptionId;
  const rest = p.names.filter((n) => n !== target);
  try {
    if (id && rest.length === 0) {
      await redisPubsubUnsubscribe(id);
      p.subscriptionId = null;
    } else if (id) {
      await redisPubsubChange(id, NO_NAMES, namesOf([target]));
    }
    p.names = rest;
    return null;
  } catch (e: unknown) {
    return describe(key, e);
  }
}

/** Pauses or resumes the view; messages keep arriving and are held (capped) while paused. */
function setPaused(key: string, paused: boolean): void {
  const p = panels.get(key);
  if (!p || p.paused === paused) return;
  p.paused = paused;
  if (!paused) {
    p.droppedByBackend += p.heldDropped;
    pushCapped(p.messages, p.held.splice(0));
    p.heldDropped = 0;
    p.version += 1;
  }
}

function clear(key: string): void {
  const p = panels.get(key);
  if (!p) return;
  p.messages.splice(0);
  p.held.splice(0);
  p.version += 1;
}

/** Ends the panel's subscription and forgets it (when its tab closes). */
async function close(key: string): Promise<void> {
  const p = panels.get(key);
  panels.delete(key);
  if (p?.subscriptionId) await redisPubsubUnsubscribe(p.subscriptionId).catch(() => undefined);
  if (panels.size === 0 && unlisten) {
    unlisten();
    unlisten = null;
    listening = null;
  }
}

/** Pub/Sub panels by tab, with one shared event listener. */
export function usePubSub() {
  return { panels, panel, reattach, subscribe, unsubscribe, setPaused, clear, close, MAX_VIEW_MESSAGES };
}
