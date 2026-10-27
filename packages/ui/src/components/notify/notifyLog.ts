import { MAX_CHANNEL_BYTES, MAX_PAYLOAD_BYTES } from '../../api/notify';
import type { NotifyMessage, NotifyState } from '../../api/notify';

/** Notifications a panel keeps in view; older ones are dropped, never stored. */
export const MAX_VIEW_NOTIFICATIONS = 2000;

/** A row of the list: a notification, or a marker where some were lost. */
export type LogEntry =
  | (NotifyMessage & { type: 'message'; id: number })
  | { type: 'gap'; id: number; at: number; text: string };

const encoder = new TextEncoder();

/** Size of a text in UTF-8 bytes, as the server counts it. */
export function byteLength(text: string): number {
  return encoder.encode(text).length;
}

/** Why a channel name cannot be used, or `null` when it can. */
export function channelProblem(name: string): string | null {
  if (name === '') return 'Enter a channel.';
  if (name.includes('\0')) return 'The channel name contains a NUL character.';
  const bytes = byteLength(name);
  if (bytes > MAX_CHANNEL_BYTES) return `The channel name is ${bytes} bytes long; PostgreSQL allows at most ${MAX_CHANNEL_BYTES}.`;
  return null;
}

/** Why a payload cannot be sent, or `null` when it can. */
export function payloadProblem(payload: string): string | null {
  if (payload.includes('\0')) return 'The payload contains a NUL character.';
  const bytes = byteLength(payload);
  if (bytes > MAX_PAYLOAD_BYTES) return `The payload is ${bytes} bytes long; PostgreSQL allows at most ${MAX_PAYLOAD_BYTES}.`;
  return null;
}

/**
 * True when an unquoted name in SQL would be another channel: PostgreSQL folds
 * unquoted identifiers to lower case, so `NOTIFY Orders` sends to `orders`.
 */
export function needsQuotesInSql(name: string): boolean {
  return !/^[a-z_][a-z0-9_$]*$/.test(name);
}

/** The channel as a quoted identifier, as the backend sends it. */
export function quoteChannel(name: string): string {
  return `"${name.replaceAll('"', '""')}"`;
}

/** The SQL a user would type to send the same notification elsewhere. */
export function notifySql(channel: string, payload: string): string {
  const literal = (text: string): string => `'${text.replaceAll("'", "''")}'`;
  return payload === '' ? `NOTIFY ${quoteChannel(channel)}` : `SELECT pg_notify(${literal(channel)}, ${literal(payload)})`;
}

/** Appends to `list` in place and drops the oldest beyond `max`. Returns how many were dropped. */
export function pushCapped<T>(list: T[], incoming: readonly T[], max = MAX_VIEW_NOTIFICATIONS): number {
  for (const item of incoming) list.push(item);
  const over = list.length - max;
  if (over > 0) list.splice(0, over);
  return Math.max(0, over);
}

/** True when an entry matches the filter (channel or payload, case-insensitive). Markers always show. */
export function matchesFilter(entry: LogEntry, filter: string, channel: string | null): boolean {
  if (entry.type === 'gap') return true;
  if (channel !== null && entry.channel !== channel) return false;
  const f = filter.trim().toLowerCase();
  if (f === '') return true;
  return entry.channel.toLowerCase().includes(f) || entry.payload.toLowerCase().includes(f);
}

/** `HH:MM:SS.mmm` in the user's time zone. */
export function messageTime(ms: number): string {
  const d = new Date(ms);
  const pad = (n: number, w = 2): string => String(n).padStart(w, '0');
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}.${pad(d.getMilliseconds(), 3)}`;
}

/** One line of the connection state, for the panel header. */
export function statusText(state: NotifyState | null): string {
  if (!state || state.status === 'stopped') {
    if (state?.error) return `Stopped: ${state.error.message}`;
    return 'Not listening';
  }
  if (state.status === 'connected') {
    const n = state.channels.length;
    return `Listening on ${n} channel${n === 1 ? '' : 's'} (backend ${state.backendPid ?? '?'})`;
  }
  const parts: string[] = [];
  if (state.attempt > 0) parts.push(`${state.attempt} failed attempt${state.attempt === 1 ? '' : 's'}`);
  if (state.retryInMs !== null) parts.push(`next try in ${formatWait(state.retryInMs)}`);
  return parts.length > 0 ? `Reconnecting (${parts.join(', ')})` : 'Reconnecting';
}

/** A wait in ms as `500 ms` or `4 s`. */
export function formatWait(ms: number): string {
  return ms < 1000 ? `${ms} ms` : `${Math.round(ms / 100) / 10} s`;
}

/** True when `next` is newer than the state shown (`null`: nothing shown yet). */
export function isNewer(shown: NotifyState | null, next: NotifyState): boolean {
  return shown === null || next.version > shown.version;
}

/**
 * Markers to add when a newer state arrives: the listener's `gap` flag turned on
 * (it reconnected, so notifications sent while it was down are lost; also seen
 * by a panel that opens later), and a stop for a reason. `previous` is the state shown before.
 */
export function stateMarkers(previous: NotifyState | null, next: NotifyState): string[] {
  const out: string[] = [];
  const sameListener = previous !== null && previous.listenerId === next.listenerId;
  if (next.gap && !(sameListener && previous.gap)) {
    out.push('The listener reconnected. Notifications sent while it was down are lost.');
  }
  if (previous && previous.status !== 'stopped' && next.status === 'stopped' && next.error) {
    out.push(`The listener stopped: ${next.error.message}`);
  }
  return out;
}

/** Messages that arrived per channel, for the channel list. */
export function countByChannel(entries: readonly LogEntry[]): Map<string, number> {
  const counts = new Map<string, number>();
  for (const e of entries) if (e.type === 'message') counts.set(e.channel, (counts.get(e.channel) ?? 0) + 1);
  return counts;
}

/** The entries as JSON lines-like array text, for export. */
export function exportEntries(entries: readonly LogEntry[]): string {
  const rows = entries
    .filter((e): e is Extract<LogEntry, { type: 'message' }> => e.type === 'message')
    .map((e) => ({ receivedAt: new Date(e.receivedAt).toISOString(), channel: e.channel, payload: e.payload, pid: e.pid, own: e.own }));
  return `${JSON.stringify(rows, null, 2)}\n`;
}
