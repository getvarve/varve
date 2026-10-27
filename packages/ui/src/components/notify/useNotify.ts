import { listen } from '@tauri-apps/api/event';
import type { UnlistenFn } from '@tauri-apps/api/event';
import { nextTick, reactive } from 'vue';
import {
  NOTIFY_MESSAGES_EVENT,
  NOTIFY_STATUS_EVENT,
  notifyAck,
  notifyListen,
  notifySend,
  notifyState,
  notifyStop,
  notifyUnlisten,
} from '../../api/notify';
import type { NotifyBatch, NotifySent, NotifyState } from '../../api/notify';
import { presentError } from '../../api/errors';
import { useDialogs } from '../../composables/useDialogs';
import { useTabs } from '../../composables/useTabs';
import { runWithConfirmation } from '../../lib/destructive';
import { channelProblem, payloadProblem } from './notifyLog';
import { applyBatch, applyState, clearPanel, newPanel, setPaused } from './notifyPanel';
import type { NotifyPanel } from './notifyPanel';

const panels = reactive(new Map<string, NotifyPanel>());
let unlistens: UnlistenFn[] = [];
let listening: Promise<void> | null = null;

function panelOfSession(sessionId: string): NotifyPanel | null {
  for (const panel of panels.values()) if (panel.sessionId === sessionId) return panel;
  return null;
}

function onBatch(batch: NotifyBatch): void {
  const panel = panelOfSession(batch.sessionId);
  if (panel) applyBatch(panel, batch);
  // Acknowledge after the batch was rendered, so the backend never runs ahead
  // of the view. A batch of a panel that is gone is acknowledged too.
  const { sessionId, listenerId, seq } = batch;
  void nextTick().then(() => notifyAck(sessionId, listenerId, seq).catch(() => undefined));
}

function onState(state: NotifyState): void {
  const panel = panelOfSession(state.sessionId);
  if (panel) applyState(panel, state);
}

function ensureListening(): Promise<void> {
  listening ??= Promise.all([
    listen<NotifyBatch>(NOTIFY_MESSAGES_EVENT, (e) => onBatch(e.payload)),
    listen<NotifyState>(NOTIFY_STATUS_EVENT, (e) => onState(e.payload)),
  ]).then((fns) => {
    unlistens = fns;
  });
  return listening;
}

/** The message of a failed call; a lost session or reconnect is reported to the tab `key`. */
function describe(key: string, e: unknown): string {
  const view = presentError(useTabs().reportTabError(key, e));
  return view.hint ? `${view.message} ${view.hint}` : view.message;
}

/** The panel of `key` (a tab id), created on first use for `sessionId` (a reconnect gives a new session). */
function panel(key: string, sessionId: string): NotifyPanel {
  const existing = panels.get(key);
  if (existing && existing.sessionId === sessionId) return existing;
  // The listener of an old session ended with it.
  panels.set(key, newPanel(sessionId));
  return panels.get(key) as NotifyPanel;
}

/** Reads the listener's state (after a reload of the view, or on open). */
async function refresh(key: string): Promise<string | null> {
  const p = panels.get(key);
  if (!p) return null;
  try {
    await ensureListening();
    applyState(p, await notifyState(p.sessionId));
    return null;
  } catch (e: unknown) {
    return describe(key, e);
  }
}

/** Listens to a channel, starting the tab's listener on first use. Returns an error message or `null`. */
async function listenTo(key: string, channel: string): Promise<string | null> {
  const p = panels.get(key);
  if (!p) return null;
  const problem = channelProblem(channel);
  if (problem) return problem;
  p.busy = true;
  try {
    await ensureListening();
    applyState(p, await notifyListen(p.sessionId, channel));
    return null;
  } catch (e: unknown) {
    return describe(key, e);
  } finally {
    p.busy = false;
  }
}

/** Stops listening to a channel. */
async function unlistenFrom(key: string, channel: string): Promise<string | null> {
  const p = panels.get(key);
  if (!p) return null;
  try {
    applyState(p, await notifyUnlisten(p.sessionId, channel));
    return null;
  } catch (e: unknown) {
    return describe(key, e);
  }
}

/** The outcome of a send: what was sent, nothing (declined), or an error message. */
export type SendOutcome = { sent: NotifySent } | { declined: true } | { error: string };

/** Sends a notification on the listener's connection; asks first where the connection needs it. */
async function send(key: string, channel: string, payload: string, connectionName: string): Promise<SendOutcome> {
  const p = panels.get(key);
  if (!p) return { declined: true };
  const problem = channelProblem(channel) ?? payloadProblem(payload);
  if (problem) return { error: problem };
  const sessionId = p.sessionId;
  try {
    await ensureListening();
    const sent = await runWithConfirmation(
      (confirmed) => notifySend(sessionId, channel, payload, confirmed),
      (request) =>
        useDialogs().askDestructive({
          connectionName,
          environment: request.environment,
          statements: request.statements,
        }),
    );
    if (sent === null) return { declined: true };
    // The listener may have just started: show its state.
    void refresh(key);
    return { sent };
  } catch (e: unknown) {
    return { error: describe(key, e) };
  }
}

/** Stops the tab's listener (the panel stays, with its log). */
async function stop(key: string): Promise<string | null> {
  const p = panels.get(key);
  if (!p) return null;
  try {
    await notifyStop(p.sessionId);
    applyState(p, await notifyState(p.sessionId));
    return null;
  } catch (e: unknown) {
    return describe(key, e);
  }
}

function pause(key: string, paused: boolean): void {
  const p = panels.get(key);
  if (p) setPaused(p, paused);
}

function clear(key: string): void {
  const p = panels.get(key);
  if (p) clearPanel(p);
}

/** Ends the panel's listener and forgets it (when its tab closes). */
async function close(key: string): Promise<void> {
  const p = panels.get(key);
  panels.delete(key);
  if (p) await notifyStop(p.sessionId).catch(() => undefined);
  if (panels.size === 0 && listening) {
    const done = listening;
    listening = null;
    await done.catch(() => undefined);
    for (const fn of unlistens) fn();
    unlistens = [];
  }
}

/** Notification panels by tab, with one shared pair of event listeners. */
export function useNotify() {
  return { panels, panel, refresh, listen: listenTo, unlisten: unlistenFrom, send, stop, pause, clear, close };
}
