import { redisCommandHelp } from '../api/redis';
import type { CommandHelp } from '../api/redis';

/** A plain command or subcommand word (`CONFIG`, `GET`, `JSON.GET`, `XGROUP`). */
const WORD = /^[A-Za-z][A-Za-z0-9_.|-]*$/;

/**
 * The command a console line runs, for help: its first word, and the
 * second when it looks like a subcommand (`CONFIG GET`); the backend falls
 * back to the command itself when the second word is an argument. `null`
 * for an empty line or a comment.
 */
export function commandOfLine(line: string): string | null {
  const text = line.trim();
  if (text === '' || text.startsWith('#') || text.startsWith('//')) return null;
  const words = text.split(/\s+/);
  const first = words[0] ?? '';
  if (!WORD.test(first)) return null;
  const second = words[1];
  return second !== undefined && WORD.test(second) ? `${first} ${second}`.toUpperCase() : first.toUpperCase();
}

/** Help answers per session and command, so a lookup runs once per session (the server's docs do not change). */
const cache = new Map<string, Map<string, Promise<CommandHelp>>>();

/** Help for `name` on `sessionId`, cached; a failed lookup is not cached. */
export function commandHelp(sessionId: string, name: string): Promise<CommandHelp> {
  let perSession = cache.get(sessionId);
  if (!perSession) {
    perSession = new Map();
    cache.set(sessionId, perSession);
  }
  const key = name.toUpperCase();
  const known = perSession.get(key);
  if (known) return known;
  const pending = redisCommandHelp(sessionId, key);
  perSession.set(key, pending);
  pending.catch(() => perSession.delete(key));
  return pending;
}

/** Forgets the help of a closed session. */
export function forgetCommandHelp(sessionId: string): void {
  cache.delete(sessionId);
}

/** The arity in words (`COMMAND INFO`: negative means at least). */
export function arityText(arity: number | null): string | null {
  if (arity === null) return null;
  const words = Math.abs(arity);
  return arity < 0 ? `at least ${words} words, the command included` : `exactly ${words} words, the command included`;
}
