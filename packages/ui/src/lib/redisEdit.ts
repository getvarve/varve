import { moduleKindOf } from '../api/redis';
import type { DigestMissing, FieldValue, KeyEdit, KeyType, RedisEditOutcome, RedisEditPlan, ZsetBound } from '../api/redis';
import type { Value } from '../api/sessions';
import { isPlaceholder, plainText } from './values';

/** Edits one plan may hold (`RedisEditRequest`). */
export const MAX_EDITS = 10_000;
/** Bytes of new values one plan may carry (originals are not counted: the backend keeps only their digest). */
export const MAX_EDIT_BYTES = 8 * 1024 * 1024;

/** A text value as the backend takes it. */
export function textValue(text: string): Value {
  return { t: 'text', v: text };
}

/**
 * True when a read value can be edited in place: text only. Byte values
 * (shown as hex) and placeholders of values too large to read can be removed
 * but not edited here.
 */
export function editableText(value: Value): boolean {
  return value.t === 'text' && !isPlaceholder(value);
}

/** True when a read value can be sent back as `original` (everything but a placeholder). */
export function canReference(value: Value): boolean {
  return !isPlaceholder(value);
}

const FLOAT = /^[+-]?(\d+(\.\d*)?|\.\d+)(e[+-]?\d+)?$/i;

/**
 * A sorted set score typed by the user, as the `Value` the backend takes:
 * an int for whole numbers in the safe range, a float otherwise, `inf` and
 * `-inf` as Redis spells them. `null` when the text is not a score (Redis
 * refuses `nan`).
 */
export function parseScore(text: string): Value | null {
  const t = text.trim();
  if (/^[+]?inf$/i.test(t)) return { t: 'other', v: 'inf' };
  if (/^-inf$/i.test(t)) return { t: 'other', v: '-inf' };
  if (!FLOAT.test(t)) return null;
  const n = Number(t);
  if (!Number.isFinite(n)) return null;
  if (/^[+-]?\d+$/.test(t) && Number.isSafeInteger(n)) return { t: 'int', v: n };
  return { t: 'float', v: n };
}

/** The text of a score for an input field. */
export function scoreText(score: Value): string {
  return plainText(score) ?? '';
}

/** Explicit stream ids: `<ms>-<seq>` or `<ms>` (the server fills in the sequence). */
export function isStreamId(text: string): boolean {
  return /^\d{1,20}(-\d{1,20})?$/.test(text.trim());
}

/** Whole seconds from a TTL field as milliseconds text, or `null` when it is not a positive whole number. */
export function ttlMsFromSeconds(text: string): string | null {
  const t = text.trim();
  if (!/^\d{1,15}$/.test(t)) return null;
  const ms = BigInt(t) * 1000n;
  return ms >= 1n ? ms.toString() : null;
}

/** Parses JSON text; returns the error message when it is not valid JSON. */
export function jsonProblem(text: string): string | null {
  try {
    JSON.parse(text);
    return null;
  } catch (e: unknown) {
    return e instanceof Error ? e.message : 'Not valid JSON.';
  }
}

/**
 * JSON text without insignificant whitespace, keeping numbers, key order and
 * escapes exactly as written (JSON.parse would round big numbers).
 */
export function minifyJson(text: string): string {
  let out = '';
  let inString = false;
  let escaped = false;
  for (const ch of text) {
    if (inString) {
      out += ch;
      if (escaped) escaped = false;
      else if (ch === '\\') escaped = true;
      else if (ch === '"') inString = false;
    } else if (ch === '"') {
      inString = true;
      out += ch;
    } else if (ch !== ' ' && ch !== '\n' && ch !== '\r' && ch !== '\t') out += ch;
  }
  return out;
}

/** Stable text of a value, for identities and comparisons. */
function valueKey(value: Value): string {
  return JSON.stringify(value);
}

/**
 * The element an edit touches, so one plan touches each element once (a
 * second edit of the same element replaces the first). Pushes and stream
 * appends have no element yet: `null`, they never replace each other.
 */
export function editTarget(edit: KeyEdit): string | null {
  switch (edit.op) {
    case 'stringSet':
    case 'jsonSet':
      return 'value';
    case 'hashSet':
    case 'hashDelete':
      return `field:${valueKey(edit.field)}`;
    case 'listSet':
    case 'listRemove':
      return `index:${edit.index}`;
    case 'setAdd':
      return edit.members.length === 1 && edit.members[0] ? `member:${valueKey(edit.members[0])}` : null;
    case 'setRemove':
      return `member:${valueKey(edit.member)}`;
    case 'zsetAdd':
    case 'zsetScore':
    case 'zsetRemove':
      return `member:${valueKey(edit.member)}`;
    case 'streamDelete':
      return `id:${edit.id}`;
    case 'expire':
    case 'persist':
      return 'ttl';
    case 'streamTrim':
      return 'trim';
    case 'tsAdd':
      return edit.timestamp === null || edit.timestamp === '*' ? null : `sample:${edit.timestamp}`;
    case 'tsDeleteRange':
      return `range:${edit.from}:${edit.to}`;
    case 'tsAlter':
      return 'settings';
    case 'cuckooDelete':
      return `item:${valueKey(edit.item)}`;
    case 'listPush':
    case 'streamAdd':
    case 'rename':
    case 'delete':
    case 'bloomAdd':
    case 'cuckooAdd':
    case 'cmsIncrBy':
    case 'topkAdd':
    case 'tdigestAdd':
      return null;
  }
}

/** Target of a hash field, list index, member or stream id, as `editTarget` names it. */
export const targetOf = {
  field: (field: Value) => `field:${valueKey(field)}`,
  index: (index: string) => `index:${index}`,
  member: (member: Value) => `member:${valueKey(member)}`,
  id: (id: string) => `id:${id}`,
};

/** Bytes of new values an edit adds to a plan (UTF-8 for text, hex / 2 for bytes); originals do not count. */
export function editBytes(edit: KeyEdit): number {
  const size = (v: Value | null | undefined): number => {
    if (!v) return 0;
    if (v.t === 'bytes') return v.v.length / 2;
    const text = plainText(v);
    return text === null ? 0 : new TextEncoder().encode(text).length;
  };
  const fields = (list: FieldValue[]): number => list.reduce((n, f) => n + size(f.field) + size(f.value), 0);
  switch (edit.op) {
    case 'stringSet':
      return size(edit.value);
    case 'jsonSet':
      return new TextEncoder().encode(edit.document).length;
    case 'hashSet':
      return size(edit.field) + size(edit.value);
    case 'hashDelete':
      return size(edit.field);
    case 'listPush':
      return edit.values.reduce((n, v) => n + size(v), 0);
    case 'listSet':
      return size(edit.value);
    case 'listRemove':
      return 0;
    case 'setAdd':
      return edit.members.reduce((n, v) => n + size(v), 0);
    case 'setRemove':
      return size(edit.member);
    case 'zsetAdd':
      return size(edit.member);
    case 'zsetScore':
    case 'zsetRemove':
      return size(edit.member);
    case 'streamAdd':
      return fields(edit.fields);
    case 'bloomAdd':
    case 'cuckooAdd':
    case 'topkAdd':
      return edit.items.reduce((n, v) => n + size(v), 0);
    case 'cuckooDelete':
      return size(edit.item);
    case 'cmsIncrBy':
      return edit.items.reduce((n, i) => n + size(i.item), 0);
    case 'tsAlter':
      return (edit.labels ?? []).reduce((n, l) => n + l.name.length + l.value.length, 0);
    default:
      return 0;
  }
}

/**
 * Pending edits of one key, in the order they were made. Staging an edit of
 * an element that already has one replaces it, so each element is edited at
 * most once per plan (a backend rule).
 */
export class EditDraft {
  private items: KeyEdit[] = [];

  get edits(): readonly KeyEdit[] {
    return this.items;
  }

  get size(): number {
    return this.items.length;
  }

  /** Adds an edit, replacing an earlier one of the same element. */
  stage(edit: KeyEdit): void {
    const target = editTarget(edit);
    if (target !== null) {
      const at = this.items.findIndex((e) => editTarget(e) === target);
      if (at >= 0) {
        this.items.splice(at, 1, edit);
        return;
      }
    }
    this.items.push(edit);
  }

  /** The pending edit of an element, if any. */
  pending(target: string): KeyEdit | null {
    return this.items.find((e) => editTarget(e) === target) ?? null;
  }

  /** Drops the pending edit of an element. */
  unstage(target: string): void {
    this.items = this.items.filter((e) => editTarget(e) !== target);
  }

  /** Drops one edit by position (for pushes and appends, which have no element). */
  removeAt(index: number): void {
    this.items.splice(index, 1);
  }

  clear(): void {
    this.items = [];
  }

  /** Why the draft cannot be planned as it is, or `null`. */
  problem(): string | null {
    if (this.items.length === 0) return 'Nothing to save.';
    if (this.items.length > MAX_EDITS) return `At most ${MAX_EDITS.toLocaleString('en-US')} changes can be saved at once.`;
    const bytes = this.items.reduce((n, e) => n + editBytes(e), 0);
    if (bytes > MAX_EDIT_BYTES) return 'These changes carry more than 8 MiB of new values. Save them in smaller parts.';
    return null;
  }
}

/** Value edits a key type accepts (`expire`, `persist`, `rename` and `delete` fit any type). */
export function opsForType(type: KeyType): readonly KeyEdit['op'][] {
  switch (moduleKindOf(type)) {
    case 'timeSeries':
      return ['tsAdd', 'tsDeleteRange', 'tsAlter'];
    case 'bloom':
      return ['bloomAdd'];
    case 'cuckoo':
      return ['cuckooAdd', 'cuckooDelete'];
    case 'countMin':
      return ['cmsIncrBy'];
    case 'topK':
      return ['topkAdd'];
    case 'tDigest':
      return ['tdigestAdd'];
    case null:
      break;
  }
  const name = typeof type === 'object' ? 'other' : type;
  switch (name) {
    case 'string':
      return ['stringSet'];
    case 'json':
      return ['jsonSet'];
    case 'hash':
      return ['hashSet', 'hashDelete'];
    case 'list':
      return ['listPush', 'listSet', 'listRemove'];
    case 'set':
      return ['setAdd', 'setRemove'];
    case 'zset':
      return ['zsetAdd', 'zsetScore', 'zsetRemove'];
    case 'stream':
      return ['streamAdd', 'streamDelete', 'streamTrim'];
    default:
      return [];
  }
}

/** A short line per pending edit, for the save bar. */
export function describeEdit(edit: KeyEdit): string {
  const text = (v: Value): string => {
    const t = plainText(v) ?? '';
    return t.length > 40 ? `${t.slice(0, 40)}…` : t;
  };
  switch (edit.op) {
    case 'stringSet':
      return 'Set the value';
    case 'jsonSet':
      return 'Set the JSON document';
    case 'hashSet':
      return edit.original === null ? `Add field ${text(edit.field)}` : `Change field ${text(edit.field)}`;
    case 'hashDelete':
      return `Delete field ${text(edit.field)}`;
    case 'listPush':
      return `Push ${edit.values.length} to the ${edit.end === 'head' ? 'head (LPUSH)' : 'tail (RPUSH)'}`;
    case 'listSet':
      return `Set index ${edit.index}`;
    case 'listRemove':
      return `Remove index ${edit.index}`;
    case 'setAdd':
      return edit.members.length === 1 && edit.members[0] ? `Add ${text(edit.members[0])}` : `Add ${edit.members.length} members`;
    case 'setRemove':
      return `Remove ${text(edit.member)}`;
    case 'zsetAdd':
      return `Add ${text(edit.member)} with score ${scoreText(edit.score)}`;
    case 'zsetScore':
      return `Score of ${text(edit.member)} to ${scoreText(edit.score)}`;
    case 'zsetRemove':
      return `Remove ${text(edit.member)}`;
    case 'streamAdd':
      return `Add an entry (${edit.id ?? '*'})`;
    case 'streamDelete':
      return `Delete entry ${edit.id}`;
    case 'streamTrim':
      return `Trim with ${edit.strategy === 'maxLen' ? 'MAXLEN' : 'MINID'} ${edit.approximate ? '~ ' : ''}${edit.threshold}`;
    case 'expire':
      return `Expire in ${formatSeconds(edit.ttlMs)}`;
    case 'persist':
      return 'Remove the TTL';
    case 'rename':
      return `Rename to ${edit.newKey}`;
    case 'delete':
      return 'Delete the key';
    case 'tsAdd':
      return `Add sample ${scoreText(edit.value)} at ${edit.timestamp === null || edit.timestamp === '*' ? 'the server time' : edit.timestamp}`;
    case 'tsDeleteRange':
      return `Delete samples from ${edit.from} to ${edit.to}`;
    case 'tsAlter':
      return 'Change the series settings';
    case 'bloomAdd':
    case 'topkAdd':
      return edit.items.length === 1 && edit.items[0] ? `Add ${text(edit.items[0])}` : `Add ${edit.items.length} items`;
    case 'cuckooAdd':
      return `${edit.nx ? 'Add if missing' : 'Add'} ${edit.items.length === 1 && edit.items[0] ? text(edit.items[0]) : `${edit.items.length} items`}`;
    case 'cuckooDelete':
      return `Delete ${text(edit.item)}`;
    case 'cmsIncrBy':
      return edit.items.length === 1 && edit.items[0] ? `Count ${text(edit.items[0].item)} +${edit.items[0].increment}` : `Count ${edit.items.length} items`;
    case 'tdigestAdd':
      return `Add ${edit.values.length} observation${edit.values.length === 1 ? '' : 's'}`;
  }
}

function formatSeconds(ms: string): string {
  const s = /^\d+$/.test(ms) ? BigInt(ms) / 1000n : 0n;
  return `${s.toString()} s`;
}

/** The headline of a plan preview. */
export function planHeadline(plan: Pick<RedisEditPlan, 'key' | 'db' | 'commands'>): string {
  const n = plan.commands.length;
  return `${n} command${n === 1 ? '' : 's'} on ${plan.key} in db${plan.db}`;
}

/** What the editor says after an apply; `reread` tells it to read the key again. */
export interface OutcomeView {
  tone: 'success' | 'warning' | 'danger';
  title: string;
  message: string;
  reread: boolean;
}

const CONFLICT_TITLES: Record<Extract<RedisEditOutcome, { status: 'conflict' }>['reason'], string> = {
  typeChanged: 'The key changed type or no longer exists',
  versionChanged: 'The key or its TTL changed since it was read',
  valueChanged: 'A value changed since it was read',
  targetExists: 'The new name is taken',
  concurrentChange: 'Someone changed the key at the same moment',
};

/** Turns an apply outcome into the editor's message. */
export function outcomeView(outcome: RedisEditOutcome, edits: readonly KeyEdit[]): OutcomeView {
  switch (outcome.status) {
    case 'applied':
      return {
        tone: 'success',
        title: 'Saved',
        message: edits.length === 0 ? 'The change was applied.' : `${edits.length} change${edits.length === 1 ? '' : 's'} applied.`,
        reread: true,
      };
    case 'conflict': {
      const edit = outcome.edit === null ? undefined : edits[outcome.edit];
      const which = edit ? ` (${describeEdit(edit)})` : '';
      return {
        tone: 'warning',
        title: CONFLICT_TITLES[outcome.reason],
        message: `${outcome.message}${which} Nothing was changed. The key was read again; make your changes on the current value.`,
        reread: true,
      };
    }
    case 'failed':
      return {
        tone: 'danger',
        title: `Command ${outcome.command + 1} failed`,
        message: `${outcome.error.message} Redis does not roll back, so the other commands of the transaction ran. The key was read again.`,
        reread: true,
      };
  }
}

/**
 * One end of a sorted set range as typed, in Redis syntax: empty, `-inf`,
 * `+inf`, `-` or `+` for open; a leading `(` for exclusive. Lex bounds also
 * take a leading `[` for inclusive. `null` when a score is not a number.
 */
export function parseBound(text: string, by: 'score' | 'lex'): ZsetBound | null {
  const t = by === 'score' ? text.trim() : text;
  if (t === '' || t === '-' || t === '+' || (by === 'score' && /^[+-]?inf$/i.test(t))) return { kind: 'unbounded' };
  const exclusive = t.startsWith('(');
  const body = exclusive || (by === 'lex' && t.startsWith('[')) ? t.slice(1) : t;
  if (by === 'lex') return { kind: exclusive ? 'exclusive' : 'inclusive', value: textValue(body) };
  const score = parseScore(body);
  return score ? { kind: exclusive ? 'exclusive' : 'inclusive', value: score } : null;
}

/** The type of the key a create plan makes, from its first value edit; `unknown` when it has none. */
export function createdKeyType(edits: readonly KeyEdit[]): KeyType {
  for (const edit of edits) {
    switch (edit.op) {
      case 'stringSet':
        return 'string';
      case 'jsonSet':
        return 'json';
      case 'hashSet':
        return 'hash';
      case 'listPush':
        return 'list';
      case 'setAdd':
        return 'set';
      case 'zsetAdd':
        return 'zset';
      case 'streamAdd':
        return 'stream';
      default:
        break;
    }
  }
  return 'unknown';
}

/**
 * Why whole-key changes of a key cannot be checked for concurrent changes, or `null` when they can (also for
 * `deferred` and `stalled`: the content is read in the background; a plan made without it says so and asks).
 */
export function digestNote(missing: DigestMissing | null): string | null {
  switch (missing) {
    case 'tooLarge':
      return 'This key is too large to read in full in time, so rename, delete and trim cannot check it for changes by others; a change that keeps its length would go unnoticed. You are asked to confirm such changes.';
    case 'notPermitted':
      return 'Your ACL user may not read this key in full, so rename, delete and trim cannot check it for changes by others; a change that keeps its length would go unnoticed. You are asked to confirm such changes.';
    default:
      return null;
  }
}

/** Confirmation statements that name commands (short upper-case words), not warning sentences. */
export function commandStatements(statements: readonly string[]): string[] {
  return statements.filter((s) => /^[A-Z][A-Z0-9_.|]*(?: [A-Z][A-Z0-9_]*)?$/.test(s));
}

/** Above this a string is loaded whole for editing only after the user confirms the size. */
export const STRING_CONFIRM_BYTES = 16n * 1024n * 1024n;
/** Above this a string is not loaded into the editor at all (the webview would freeze). */
export const STRING_EDIT_MAX_BYTES = 64n * 1024n * 1024n;

/**
 * Whether the whole string of `total` bytes may be loaded for editing: at
 * once, after a confirmation, or not at all. An unknown size asks first.
 */
export function wholeStringPolicy(total: string | null): 'load' | 'confirm' | 'refuse' {
  if (total === null || !/^\d+$/.test(total)) return 'confirm';
  const bytes = BigInt(total);
  if (bytes > STRING_EDIT_MAX_BYTES) return 'refuse';
  if (bytes > STRING_CONFIRM_BYTES) return 'confirm';
  return 'load';
}

/** Raw bytes from the lowercase hex of a `bytes` value. */
export function hexBytes(hex: string): Uint8Array {
  const out = new Uint8Array(Math.floor(hex.length / 2));
  for (let i = 0; i < out.length; i += 1) out[i] = Number.parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out;
}
