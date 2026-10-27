import type { PubSubMessage } from '../api/redis';
import { plainText, prettyJson } from './values';

/** Messages a Pub/Sub panel keeps in view; older ones are dropped, never stored. */
export const MAX_VIEW_MESSAGES = 500;

/** A received message with a stable id for rendering and selection. */
export interface ReceivedMessage extends PubSubMessage {
  id: number;
  entry: 'message';
}

/** A marker in the message list: the connection was lost and messages in between were missed. */
export interface GapMarker {
  id: number;
  entry: 'gap';
  reason: string;
  lostAt: number;
  resumedAt: number;
}

/** One row of the message list. */
export type ViewEntry = ReceivedMessage | GapMarker;

/**
 * Appends `incoming` to `list` and drops the oldest beyond `max`, in place.
 * Returns how many were dropped.
 */
export function pushCapped<T>(list: T[], incoming: readonly T[], max = MAX_VIEW_MESSAGES): number {
  for (const item of incoming) list.push(item);
  const over = list.length - max;
  if (over > 0) list.splice(0, over);
  return Math.max(0, over);
}

/** The payload as text for the table and the filter: text as is, bytes as hex. */
export function payloadText(message: Pick<PubSubMessage, 'payload'>): string {
  return plainText(message.payload) ?? '';
}

/** The payload for the detail pane: pretty JSON when asked and when it parses, else as is. */
export function payloadDetail(message: Pick<PubSubMessage, 'payload' | 'truncated'>, pretty: boolean): string {
  const text = plainText(message.payload) ?? '';
  if (!pretty || message.truncated || message.payload.t !== 'text') return text;
  return prettyJson(text) ?? text;
}

/** True when a row matches the filter text (channel, pattern or payload, case-insensitive); gaps always show. */
export function matchesFilter(message: PubSubMessage | GapMarker, filter: string): boolean {
  const f = filter.trim().toLowerCase();
  if (f === '' || isGap(message)) return true;
  return (
    message.channel.toLowerCase().includes(f) ||
    (message.pattern ?? '').toLowerCase().includes(f) ||
    payloadText(message).toLowerCase().includes(f)
  );
}

/** `HH:MM:SS.mmm` in the user's time zone. */
export function messageTime(ms: number): string {
  const d = new Date(ms);
  const pad = (n: number, w = 2): string => String(n).padStart(w, '0');
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}.${pad(d.getMilliseconds(), 3)}`;
}

/**
 * Messages per second over the last `windowMs`, from the receive times of
 * recent messages (`times` sorted ascending).
 */
export function messageRate(times: readonly number[], now: number, windowMs = 5000): number {
  let count = 0;
  for (let i = times.length - 1; i >= 0; i -= 1) {
    if ((times[i] ?? 0) < now - windowMs) break;
    count += 1;
  }
  return Math.round((count * 1000) / windowMs);
}

/** True for a glob pattern (`*`, `?` or `[`), which is subscribed with PSUBSCRIBE. */
export function isGlob(name: string): boolean {
  return /[*?[]/.test(name);
}

/** True for a gap marker row. */
export function isGap(entry: PubSubMessage | GapMarker): entry is GapMarker {
  return 'entry' in entry && entry.entry === 'gap';
}

/** The JSON a panel exports: the messages in view (payloads as text or hex) and the gaps between them. */
export function exportMessages(entries: readonly (PubSubMessage | GapMarker)[]): string {
  return JSON.stringify(entries.map((m) => (isGap(m) ? gapJson(m) : messageJson(m))), null, 2);
}

function gapJson(g: GapMarker): Record<string, unknown> {
  return { gap: true, reason: g.reason, lostAt: new Date(g.lostAt).toISOString(), resumedAt: new Date(g.resumedAt).toISOString() };
}

function messageJson(m: PubSubMessage): Record<string, unknown> {
  return {
    receivedAt: new Date(m.receivedAt).toISOString(),
    channel: m.channel,
    pattern: m.pattern,
    payload: payloadText(m),
    encoding: m.payload.t === 'bytes' ? 'hex' : 'text',
    size: m.size,
    truncated: m.truncated,
  };
}

/**
 * Splits a lost-connection reason into its sentence and a suggested
 * `CONFIG SET ...` command (the output buffer limit hint), if it has one.
 */
export function reasonParts(reason: string): { text: string; command: string | null } {
  const match = /CONFIG SET client-output-buffer-limit "[^"]*"/.exec(reason);
  if (!match) return { text: reason, command: null };
  // The sentence without the command and the "for example" that led to it.
  const before = reason.slice(0, match.index).replace(/[,;:]?\s*for example\s*$/i, '').trim();
  const after = reason.slice(match.index + match[0].length).trim();
  const text = `${before.replace(/[.,;:]$/, '')}.${after === '' || after === '.' ? '' : ` ${after}`}`;
  return { text, command: match[0] };
}

/** Wall clock time span of a gap, for its marker. */
export function gapText(gap: Pick<GapMarker, 'lostAt' | 'resumedAt'>): string {
  const seconds = Math.max(0, Math.round((gap.resumedAt - gap.lostAt) / 1000));
  return `Connection lost ${messageTime(gap.lostAt).slice(0, 8)} to ${messageTime(gap.resumedAt).slice(0, 8)} (${seconds} s): messages published then were missed.`;
}
