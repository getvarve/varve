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
 * The database the session is on after `script` ran: the last `SELECT n`
 * line, read with the same splitting rules as the highlighter, or `null`
 * when the script selects no database.
 */
export function lastSelectedDb(script: string): number | null {
  let db: number | null = null;
  for (const line of script.split(/\r?\n/)) {
    const tokens = redisTokens(line).filter((t) => t.type !== 'comment');
    const [command, arg, extra] = tokens;
    if (!command || command.type !== 'keyword' || command.text.toUpperCase() !== 'SELECT' || !arg || extra) continue;
    const value = unquote(arg.text);
    if (/^\d+$/.test(value) && Number(value) <= 65535) db = Number(value);
  }
  return db;
}

/**
 * The database a console run left the session on, or `null` when it did not
 * change. A run that succeeded counts every SELECT. A run that failed while
 * executing (the backend then names the failing line in `detail`) still ran
 * the commands before the failing one, so SELECTs before `position` count.
 * Parse errors and checks that fail before anything runs change nothing.
 */
export function selectedDbAfterRun(
  text: string,
  error: { detail: string | null; position: number | null } | null,
): number | null {
  if (!error) return lastSelectedDb(text);
  if (!error.detail?.startsWith('Line ') || error.position === null || error.position <= 1) return null;
  return lastSelectedDb([...text].slice(0, error.position - 1).join(''));
}
