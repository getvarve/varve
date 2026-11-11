import { StreamLanguage, StringStream } from '@codemirror/language';
import type { StreamParser } from '@codemirror/language';

/** Commands the console highlights. Unknown first words are still shown as command names, but not as keywords. */
const COMMANDS = new Set(
  (
    'APPEND AUTH BITCOUNT BITFIELD BITOP BITPOS BLMOVE BLPOP BRPOP BZPOPMAX BZPOPMIN CLIENT CLUSTER COMMAND CONFIG COPY DBSIZE ' +
    'DECR DECRBY DEL DISCARD DUMP ECHO EVAL EVALSHA EXEC EXISTS EXPIRE EXPIREAT EXPIRETIME FCALL FLUSHALL FLUSHDB FUNCTION ' +
    'GEOADD GEODIST GEOHASH GEOPOS GEOSEARCH GET GETDEL GETEX GETRANGE GETSET HDEL HELLO HEXISTS HGET HGETALL HINCRBY ' +
    'HINCRBYFLOAT HKEYS HLEN HMGET HMSET HRANDFIELD HSCAN HSET HSETNX HSTRLEN HVALS INCR INCRBY INCRBYFLOAT INFO KEYS LASTSAVE ' +
    'LATENCY LCS LINDEX LINSERT LLEN LMOVE LMPOP LPOP LPOS LPUSH LPUSHX LRANGE LREM LSET LTRIM MEMORY MGET MIGRATE MODULE ' +
    'MONITOR MOVE MSET MSETNX MULTI OBJECT PERSIST PEXPIRE PEXPIREAT PEXPIRETIME PFADD PFCOUNT PFMERGE PING PSETEX PSUBSCRIBE ' +
    'PTTL PUBLISH PUBSUB PUNSUBSCRIBE QUIT RANDOMKEY RENAME RENAMENX RESTORE ROLE RPOP RPOPLPUSH RPUSH RPUSHX SADD SAVE SCAN ' +
    'SCARD SCRIPT SDIFF SDIFFSTORE SELECT SET SETEX SETNX SETRANGE SINTER SINTERCARD SINTERSTORE SISMEMBER SLOWLOG SMEMBERS ' +
    'SMISMEMBER SMOVE SORT SPOP SRANDMEMBER SREM SSCAN STRLEN SUBSCRIBE SUNION SUNIONSTORE SWAPDB TIME TOUCH TTL TYPE UNLINK ' +
    'UNSUBSCRIBE UNWATCH WAIT WATCH XACK XADD XAUTOCLAIM XCLAIM XDEL XGROUP XINFO XLEN XPENDING XRANGE XREAD XREADGROUP ' +
    'XREVRANGE XTRIM ZADD ZCARD ZCOUNT ZDIFF ZINCRBY ZINTER ZINTERCARD ZLEXCOUNT ZMPOP ZMSCORE ZPOPMAX ZPOPMIN ZRANDMEMBER ' +
    'ZRANGE ZRANGEBYLEX ZRANGEBYSCORE ZRANGESTORE ZRANK ZREM ZREMRANGEBYLEX ZREMRANGEBYRANK ZREMRANGEBYSCORE ZREVRANGE ' +
    'ZREVRANK ZSCAN ZSCORE ZUNION ZUNIONSTORE JSON.GET JSON.SET JSON.DEL JSON.TYPE JSON.MGET FT.SEARCH FT.INFO TS.RANGE TS.ADD'
  ).split(' '),
);

/** Commands whose second word is a subcommand (`CLIENT LIST`, `CONFIG GET`). */
const CONTAINERS = new Set(['ACL', 'CLIENT', 'CLUSTER', 'COMMAND', 'CONFIG', 'FUNCTION', 'LATENCY', 'MEMORY', 'MODULE', 'OBJECT', 'PUBSUB', 'SCRIPT', 'SLOWLOG', 'XGROUP', 'XINFO']);

interface State {
  /** 0: the next word is the command, 1: the next word is a subcommand, 2: arguments. */
  position: 0 | 1 | 2;
}

const parser: StreamParser<State> = {
  name: 'redis',
  startState: () => ({ position: 0 }),
  token(stream, state) {
    if (stream.sol()) state.position = 0;
    if (stream.eatSpace()) return null;
    const ch = stream.peek();
    if (state.position === 0 && ch === '#') {
      stream.skipToEnd();
      return 'comment';
    }
    if (ch === '"' || ch === "'") {
      stream.next();
      let escaped = false;
      while (!stream.eol()) {
        const c = stream.next();
        if (escaped) escaped = false;
        else if (c === '\\' && ch === '"') escaped = true;
        else if (c === ch) break;
      }
      state.position = 2;
      return 'string';
    }
    stream.eatWhile(/[^\s"']/);
    const word = stream.current();
    if (state.position === 0) {
      const name = word.toUpperCase();
      state.position = CONTAINERS.has(name) ? 1 : 2;
      return COMMANDS.has(name) || CONTAINERS.has(name) ? 'keyword' : 'variableName';
    }
    if (state.position === 1) {
      state.position = 2;
      return 'keyword';
    }
    if (/^[+-]?(\d+(\.\d*)?|\.\d+)(e[+-]?\d+)?$/i.test(word) || /^[+-]?inf$/i.test(word)) return 'number';
    return null;
  },
};

/** Redis console highlighting: command names, strings, numbers and `#` comments. */
export const redisLanguage = StreamLanguage.define(parser);

/** The tokens of one console line, for tests. */
export function redisTokens(line: string): { text: string; type: string | null }[] {
  const stream = new StringStream(line, 4, 2);
  const state = parser.startState ? parser.startState(2) : { position: 0 as const };
  const out: { text: string; type: string | null }[] = [];
  while (!stream.eol()) {
    stream.start = stream.pos;
    const type = parser.token(stream, state);
    if (stream.pos === stream.start) stream.next();
    const text = stream.current();
    if (text.trim() !== '') out.push({ text, type });
  }
  return out;
}

/** The text of a token without its quotes (`"a b"` becomes `a b`). */
function unquote(text: string): string {
  if (text.length >= 2 && (text.startsWith('"') || text.startsWith("'")) && text.endsWith(text.charAt(0))) {
    return text.slice(1, -1);
  }
  return text;
}

/**
 * The `MULTI` block a console session has open between runs: `MULTI` in one
 * run and `EXEC` or `DISCARD` in a later one is one block, and a `SELECT`
 * queued in it switches the database only when that `EXEC` runs.
 */
export interface MultiState {
  /** A `MULTI` block is open. */
  open: boolean;
  /** The database a `SELECT` queued in the open block switches to on `EXEC`; `null` without one. */
  queuedDb: number | null;
}

/** No block open: a new session, or after `EXEC` or `DISCARD`. */
export const NO_MULTI: MultiState = { open: false, queuedDb: null };

/** The command name of a console line (upper case) and whether it has arguments; `null` for no command. */
function commandOf(line: string): { name: string; arg: { text: string } | undefined; extra: boolean } | null {
  const tokens = redisTokens(line).filter((t) => t.type !== 'comment');
  const [command, arg, extra] = tokens;
  if (!command || command.type !== 'keyword') return null;
  return { name: command.text.toUpperCase(), arg, extra: extra !== undefined };
}

/**
 * What `script` does to the session's database and `MULTI` block, read with
 * the same splitting rules as the highlighter. `db` is the last `SELECT n`
 * that took effect, or `null` when none did. A `SELECT` inside `MULTI` is
 * only queued: it counts once `EXEC` runs the block, not after `DISCARD`,
 * and not while the block is still open when the script ends (it is then
 * carried in `multi`, for the run that ends the block). `before` is the
 * block left open by earlier runs on the same session.
 */
export function selectEffect(script: string, before: MultiState = NO_MULTI): { db: number | null; multi: MultiState } {
  let db: number | null = null;
  let inMulti = before.open;
  /** The database a queued `SELECT` of the open `MULTI` block would switch to. */
  let queued: number | null = before.open ? before.queuedDb : null;
  for (const line of script.split(/\r?\n/)) {
    const command = commandOf(line);
    if (!command) continue;
    const { name, arg, extra } = command;
    if (!arg && name === 'MULTI') {
      // MULTI inside MULTI is refused by the server and keeps the open block as it is.
      if (!inMulti) queued = null;
      inMulti = true;
      continue;
    }
    if (!arg && (name === 'EXEC' || name === 'DISCARD')) {
      if (name === 'EXEC' && inMulti && queued !== null) db = queued;
      inMulti = false;
      queued = null;
      continue;
    }
    if (name !== 'SELECT' || !arg || extra) continue;
    const value = unquote(arg.text);
    if (!/^\d+$/.test(value) || Number(value) > 65535) continue;
    if (inMulti) queued = Number(value);
    else db = Number(value);
  }
  return { db, multi: inMulti ? { open: true, queuedDb: queued } : NO_MULTI };
}

/**
 * The database the session is on after `script` ran, or `null` when the
 * script selects no database (see `selectEffect`). `before` is the `MULTI`
 * block earlier runs left open; without it the script is taken to start
 * outside a block.
 */
export function lastSelectedDb(script: string, before: MultiState = NO_MULTI): number | null {
  return selectEffect(script, before).db;
}

/**
 * What a console run left behind: the database it switched the session to
 * (`null` when it did not change) and the `MULTI` block it left open. A run
 * that succeeded counts every line. A run that failed while executing (the
 * backend then names the failing line in `detail`) still ran the commands
 * before the failing one, so those count; a failing `EXEC` also ends its
 * block (the server discards an aborted transaction). Parse errors and
 * checks that fail before anything runs change nothing.
 */
export function runEffect(
  text: string,
  error: { detail: string | null; position: number | null } | null,
  before: MultiState = NO_MULTI,
): { db: number | null; multi: MultiState } {
  if (!error) return selectEffect(text, before);
  if (!error.detail?.startsWith('Line ') || error.position === null || error.position < 1) return { db: null, multi: before };
  const chars = [...text];
  const ran = selectEffect(chars.slice(0, error.position - 1).join(''), before);
  const failed = commandOf(chars.slice(error.position - 1).join('').split(/\r?\n/)[0] ?? '');
  if (failed && !failed.arg && failed.name === 'EXEC') return { db: ran.db, multi: NO_MULTI };
  return ran;
}

/**
 * The database a console run left the session on, or `null` when it did not
 * change (see `runEffect`). `before` is the `MULTI` block earlier runs on
 * the session left open.
 */
export function selectedDbAfterRun(
  text: string,
  error: { detail: string | null; position: number | null } | null,
  before: MultiState = NO_MULTI,
): number | null {
  return runEffect(text, error, before).db;
}
