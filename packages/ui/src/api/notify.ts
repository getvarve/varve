import { call } from './invoke';
import type { DriverError } from './errors';

/**
 * PostgreSQL LISTEN / NOTIFY of a tab (app/src-tauri/src/m2/notify.rs,
 * scratchpad M2-UI-API.md "PostgreSQL LISTEN / NOTIFY (B2)"). Each tab has at
 * most one listener: a dedicated connection that reconnects by itself.
 */

/** Tauri event with a `NotifyBatch`. */
export const NOTIFY_MESSAGES_EVENT = 'notify-messages';

/** Tauri event with a `NotifyState`. */
export const NOTIFY_STATUS_EVENT = 'notify-status';

/** Longest channel name in bytes (PostgreSQL truncates longer identifiers). */
export const MAX_CHANNEL_BYTES = 63;

/** Longest payload in bytes PostgreSQL accepts. */
export const MAX_PAYLOAD_BYTES = 7999;

/** `NotifyStatus`. */
export type NotifyStatus = 'connected' | 'reconnecting' | 'stopped';

/** `NotifyState`: the tab's listener. */
export interface NotifyState {
  sessionId: string;
  /** The listener this state is of; `null` when the tab has none. */
  listenerId: number | null;
  /** Orders states (events and command replies): apply only a higher version than the one shown. */
  version: number;
  status: NotifyStatus;
  /** Listened channels (also while reconnecting: listened to again once back), sorted. */
  channels: string[];
  backendPid: number | null;
  /** Failed reconnect attempts since the connection was lost (0 right after the loss). */
  attempt: number;
  retryInMs: number | null;
  /** Why it is reconnecting or why it stopped. */
  error: DriverError | null;
  /** Notifications dropped since the listener started (the view fell behind). */
  dropped: number;
  /** The listener reconnected since it started: notifications sent while it was down were lost. */
  gap: boolean;
}

/** `NotifyMessage`: one received notification. */
export interface NotifyMessage {
  channel: string;
  payload: string;
  /** Process id of the sending backend. */
  pid: number;
  /** Sent from this panel. */
  own: boolean;
  /** Not valid UTF-8 (a SQL_ASCII database): shown decoded lossily, invalid bytes as U+FFFD. */
  lossy: boolean;
  /** Unix ms. */
  receivedAt: number;
}

/** `NotifyBatch`, the payload of `notify-messages`. Acknowledge `seq` once shown. */
export interface NotifyBatch {
  sessionId: string;
  /** The listener that sent it; acknowledge with it. */
  listenerId: number;
  seq: number;
  messages: NotifyMessage[];
  /** Dropped since the previous batch because the view fell behind. */
  dropped: number;
}

/** `NotifySent`. */
export interface NotifySent {
  channel: string;
  payloadBytes: number;
  durationMs: number;
}

export function notifyState(sessionId: string): Promise<NotifyState> {
  return call('notify_state', { sessionId });
}

export function notifyListen(sessionId: string, channel: string): Promise<NotifyState> {
  return call('notify_listen', { sessionId, channel });
}

export function notifyUnlisten(sessionId: string, channel: string): Promise<NotifyState> {
  return call('notify_unlisten', { sessionId, channel });
}

/** Rejects with `confirmationRequired` on production until `confirmed`, `readOnly` on read-only sessions. */
export function notifySend(sessionId: string, channel: string, payload: string, confirmed: boolean): Promise<NotifySent> {
  return call('notify_send', { sessionId, channel, payload, confirmed });
}

/** Acknowledges batches up to `seq` of listener `listenerId` (acks of another listener are ignored). */
export function notifyAck(sessionId: string, listenerId: number, seq: number): Promise<void> {
  return call('notify_ack', { sessionId, listenerId, seq });
}

export function notifyStop(sessionId: string): Promise<void> {
  return call('notify_stop', { sessionId });
}
