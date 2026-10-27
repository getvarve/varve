import type { CmdError } from '../api/errors';
import type { Value } from '../api/sessions';
import { plainText } from './values';

function textToHex(text: string): string {
  return [...new TextEncoder().encode(text)].map((b) => b.toString(16).padStart(2, '0')).join('');
}

function asHex(value: Value): string {
  if (value.t === 'bytes') return value.v;
  if (value.t === 'text') return textToHex(value.v);
  return textToHex(plainText(value) ?? '');
}

/**
 * Joins the chunks of a long Redis string. Chunks are byte slices: text
 * while every chunk so far is valid UTF-8, otherwise bytes. Once any chunk
 * is bytes the whole value is shown as bytes, so text and hex never mix.
 */
export function joinStringChunks(previous: Value | null, next: Value): Value {
  if (previous === null) return next;
  if (previous.t === 'text' && next.t === 'text') return { t: 'text', v: previous.v + next.v };
  return { t: 'bytes', v: asHex(previous) + asHex(next) };
}

/** Bytes in one string chunk: UTF-8 length of text, half the hex length of bytes. */
export function chunkBytes(value: Value): bigint {
  if (value.t === 'bytes') return BigInt(value.v.length / 2);
  if (value.t === 'text') return BigInt(new TextEncoder().encode(value.v).length);
  return 0n;
}

/** Byte position after a chunk: its offset from the backend (decimal text) plus its size. */
export function chunkEnd(offset: string, value: Value): bigint {
  return (/^\d+$/.test(offset) ? BigInt(offset) : 0n) + chunkBytes(value);
}

/** Entries a value view keeps at most; beyond this the console reads the rest. */
export const MAX_VIEW_ENTRIES = 10_000;

/**
 * True when a value page cursor is dead: page-state cursors are single use
 * and expire (after 10 minutes, or when a session holds more than 16). A dead
 * cursor is never retried; the read starts again from the beginning.
 */
export function isDeadCursor(error: CmdError): boolean {
  if (error.kind !== 'session' || error.error.kind !== 'driver') return false;
  const e = error.error.error;
  return e.code === 'CURSOR_EXPIRED' || e.message.startsWith('Invalid page cursor');
}
