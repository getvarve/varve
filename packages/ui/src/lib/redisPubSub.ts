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

/** Messages received in one wall clock second (`second` is `ms / 1000`, rounded down). */
export interface RateBucket {
  second: number;
  count: number;
}

/** The oldest second that still counts towards the rate at `now` (it may be partly older than the window). */
function firstCountedSecond(now: number, windowMs: number): number {
  return Math.floor((now - windowMs) / 1000);
}

/**
 * Counts `count` messages received at `now` into per-second `buckets` (in
 * place, oldest first) and drops the seconds the rate window no longer needs.
 * One counter per second instead of one time per message, so the rate stays
 * right at any message rate.
 */
export function countReceived(buckets: RateBucket[], now: number, count: number, windowMs = 5000): void {
  const second = Math.floor(now / 1000);
  const last = buckets.at(-1);
  if (last && last.second === second) last.count += count;
  else buckets.push({ second, count });
  const first = firstCountedSecond(now, windowMs);
  let drop = 0;
  while (drop < buckets.length && (buckets[drop]?.second ?? 0) < first) drop += 1;
  if (drop > 0) buckets.splice(0, drop);
}

/**
 * Messages per second over about the last `windowMs`, from the per-second
 * `buckets` of `countReceived`: the whole seconds that overlap the window,
 * divided by the time they span up to `now`.
 */
export function messageRate(buckets: readonly RateBucket[], now: number, windowMs = 5000): number {
  const first = firstCountedSecond(now, windowMs);
  let count = 0;
  for (const bucket of buckets) if (bucket.second >= first) count += bucket.count;
  const spanMs = now - first * 1000;
  return spanMs > 0 ? Math.round((count * 1000) / spanMs) : 0;
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

/** What following the end needs of the scrolling message list (an element, or a stand-in in tests). */
export interface ScrollBox {
  scrollTop: number;
  readonly scrollHeight: number;
  readonly clientHeight: number;
}

/** How close to the end (px) still counts as "at the end". */
export const FOLLOW_SLACK = 24;

function nearEnd(el: ScrollBox): boolean {
  return el.scrollHeight - el.scrollTop - el.clientHeight <= FOLLOW_SLACK;
}

/**
 * Keeps a live message list at its end while batches arrive, and counts the
 * messages that arrived while it was scrolled up. The list's tab can be
 * hidden (`display: none`): its sizes and scroll offset then read 0 and the
 * offset is lost, so nothing is measured while hidden. The last state seen
 * while shown is kept and put back by `shown` when the tab is on screen again.
 */
export class EndFollower {
  /** Messages that arrived while the list was scrolled up. */
  unseen = 0;
  /** The list is at its end, so new batches keep it there. */
  following = true;
  /** The scroll offset last seen while the list was on screen. */
  private savedTop = 0;

  /** The list was scrolled (by the user, or by a jump) while `visible`. */
  scrolled(el: ScrollBox, visible: boolean): void {
    if (!visible) return;
    this.savedTop = el.scrollTop;
    this.following = nearEnd(el);
    if (this.following) this.unseen = 0;
  }

  /** Before a batch is rendered: notes whether the list is at its end. A hidden list keeps what it was. */
  beforeBatch(el: ScrollBox | null, visible: boolean): void {
    if (el === null) this.following = true;
    else if (visible) this.following = nearEnd(el);
  }

  /**
   * After a batch of `arrived` messages is rendered (`empty`: the list has no
   * messages now). A following list goes to its end; otherwise the batch is
   * counted. Returns true when the caller should hold the rows in view where
   * they were (scrolled up and on screen).
   */
  afterBatch(el: ScrollBox | null, visible: boolean, arrived: number, empty: boolean): boolean {
    if (el === null || empty) {
      this.unseen = 0;
      return false;
    }
    if (this.following) {
      this.unseen = 0;
      if (visible) this.toEnd(el);
      return false;
    }
    this.unseen += arrived;
    return visible;
  }

  /** The list is on screen again: back to its end when it was following, else to where it was. */
  shown(el: ScrollBox): void {
    if (this.following) this.jumpToEnd(el);
    else el.scrollTop = this.savedTop;
    // The browser clamps the offset to the list's current height.
    this.savedTop = el.scrollTop;
  }

  /** Goes to the end and follows from there ("N new messages"). */
  jumpToEnd(el: ScrollBox | null): void {
    if (el !== null) this.toEnd(el);
    this.following = true;
    this.unseen = 0;
  }

  /** Another message list (a new subscription panel): nothing unseen yet. */
  restart(): void {
    this.unseen = 0;
  }

  private toEnd(el: ScrollBox): void {
    el.scrollTop = el.scrollHeight;
    this.savedTop = el.scrollTop;
  }
}
