import { z } from 'zod';
import type { KeyEdit } from '../api/redis';
import { jsonProblem, parseScore, textValue, ttlMsFromSeconds } from './redisEdit';

function custom(ctx: z.RefinementCtx, path: string, message: string): void {
  ctx.addIssue({ code: z.ZodIssueCode.custom, path: [path], message });
}

/** Renaming a key: a new, different name; `replace` overwrites a key of that name. */
export function keyRenameSchema(current: string) {
  return z.object({ newKey: z.string(), replace: z.boolean() }).superRefine((v, ctx) => {
    if (v.newKey === '') custom(ctx, 'newKey', 'Enter the new key name.');
    else if (v.newKey === current) custom(ctx, 'newKey', 'The new name is the same as the current one.');
  });
}
export type KeyRenameValues = z.input<ReturnType<typeof keyRenameSchema>>;

/** Setting or removing a key's TTL. */
export const keyTtlSchema = z
  .object({ mode: z.enum(['expire', 'persist']), seconds: z.string() })
  .superRefine((v, ctx) => {
    if (v.mode === 'expire' && ttlMsFromSeconds(v.seconds) === null) custom(ctx, 'seconds', 'Enter a whole number of seconds, at least 1.');
  });
export type KeyTtlValues = z.input<typeof keyTtlSchema>;

/** Types a new key can be created as. */
export const NEW_KEY_TYPES = ['string', 'hash', 'list', 'set', 'zset', 'stream', 'json'] as const;
export type NewKeyType = (typeof NEW_KEY_TYPES)[number];

/**
 * A new key: its name, type and first element (a key cannot exist empty),
 * plus an optional TTL in seconds.
 */
export const newKeySchema = z
  .object({
    key: z.string(),
    type: z.enum(NEW_KEY_TYPES),
    /** String value, JSON document, list element, set or sorted set member, or hash / stream field. */
    value: z.string(),
    /** Hash and stream: the value of `field`. Sorted set: the score. */
    second: z.string(),
    field: z.string(),
    ttlSeconds: z.string(),
  })
  .superRefine((v, ctx) => {
    if (v.key === '') custom(ctx, 'key', 'Enter a key name.');
    if ((v.type === 'hash' || v.type === 'stream') && v.field === '') custom(ctx, 'field', 'Enter the first field name.');
    if (v.type === 'zset' && parseScore(v.second) === null) custom(ctx, 'second', 'Enter a score: a number, inf or -inf.');
    if (v.type === 'json') {
      const problem = jsonProblem(v.value);
      if (problem !== null) custom(ctx, 'value', `Not valid JSON: ${problem}`);
    }
    if (v.ttlSeconds.trim() !== '' && ttlMsFromSeconds(v.ttlSeconds) === null) {
      custom(ctx, 'ttlSeconds', 'Leave empty for no TTL, or enter a whole number of seconds.');
    }
  });
export type NewKeyValues = z.input<typeof newKeySchema>;

/** The edits that create a key from the new key form (the form must be valid). */
export function newKeyEdits(v: NewKeyValues): KeyEdit[] {
  const edits: KeyEdit[] = [];
  const value = textValue(v.value);
  switch (v.type) {
    case 'string':
      edits.push({ op: 'stringSet', value, original: null });
      break;
    case 'json':
      edits.push({ op: 'jsonSet', document: v.value, original: null });
      break;
    case 'hash':
      edits.push({ op: 'hashSet', field: textValue(v.field), value: textValue(v.second), original: null });
      break;
    case 'list':
      edits.push({ op: 'listPush', end: 'tail', values: [value] });
      break;
    case 'set':
      edits.push({ op: 'setAdd', members: [value] });
      break;
    case 'zset': {
      const score = parseScore(v.second);
      if (score === null) throw new Error('The score is not a number; validate the form first.');
      edits.push({ op: 'zsetAdd', member: value, score });
      break;
    }
    case 'stream':
      edits.push({ op: 'streamAdd', id: null, fields: [{ field: textValue(v.field), value: textValue(v.second) }] });
      break;
  }
  const ttl = v.ttlSeconds.trim() === '' ? null : ttlMsFromSeconds(v.ttlSeconds);
  if (ttl !== null) edits.push({ op: 'expire', ttlMs: ttl });
  return edits;
}
