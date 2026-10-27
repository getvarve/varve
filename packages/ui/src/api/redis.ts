import type { Planned } from './plans';
import { call } from './invoke';
import type { DriverError } from './errors';
import type { Value } from './sessions';

export type { Planned } from './plans';

/**
 * `KeyType` in crates/driver-redis/src/browser.rs. Module types arrive as
 * `{ other: name }`; `unknown` means the scan did not read the type (key info
 * reads it when the key is opened).
 */
export type KeyType = 'string' | 'list' | 'set' | 'zset' | 'hash' | 'stream' | 'json' | 'unknown' | { other: string };

/** One entry of the key browser's type filter: the value the tab keeps, and the name `TYPE` reports. */
export interface KeyTypeFilter {
  value: string;
  label: string;
}

/** Core key types the scan filter always offers, with the name `TYPE` reports. */
export const KEY_TYPE_FILTERS: readonly KeyTypeFilter[] = [
  { value: 'string', label: 'string' },
  { value: 'hash', label: 'hash' },
  { value: 'list', label: 'list' },
  { value: 'set', label: 'set' },
  { value: 'zset', label: 'zset' },
  { value: 'stream', label: 'stream' },
];

const CORE_FILTERS = ['string', 'hash', 'list', 'set', 'zset', 'stream', 'json'] as const;

/**
 * The module key types the scan filter offers on a server: the `TYPE`
 * names of its available module features (`ModuleReport.keyTypes`), with
 * what they are. JSON keeps its `json` value. Only these are offered,
 * because Valkey refuses `SCAN ... TYPE` with a type name it does not know.
 */
export function moduleTypeFilters(keyTypes: readonly string[]): KeyTypeFilter[] {
  return keyTypes.map((name) => {
    if (name === 'ReJSON-RL') return { value: 'json', label: 'ReJSON-RL (JSON)' };
    const kind = moduleKindOf({ other: name });
    return { value: name, label: kind ? `${name} (${MODULE_KIND_LABELS[kind]})` : name };
  });
}

/** The key type a filter value scans for (`redis_scan`, bulk TTL); `null` for all types. */
export function keyTypeOfFilter(value: string): Exclude<KeyType, 'unknown'> | null {
  if (value === '') return null;
  return (CORE_FILTERS as readonly string[]).includes(value) ? (value as (typeof CORE_FILTERS)[number]) : { other: value };
}

export interface KeyEntry {
  /** Key bytes as lowercase hex. */
  name: string;
  /** Lossless display text; pass this to the other commands. */
  display: string;
  keyType: KeyType;
  /** Time to live in milliseconds as decimal text; `null` when the key does not expire. */
  ttlMs: string | null;
  /**
   * True for a key whose name was too large to read; it cannot be opened.
   * Optional until every backend sends it (the UI then infers it from an
   * empty name with the oversized marker).
   */
  placeholder?: boolean;
}

/** Unsigned 64-bit numbers travel as decimal strings: JavaScript numbers cannot hold them exactly. */
export type U64Text = string;

export interface ScanPage {
  /** Cursor for the next step; `"0"` when the scan is complete. */
  cursor: U64Text;
  keys: KeyEntry[];
}

export interface KeyInfo {
  keyType: KeyType;
  /** Time to live in milliseconds as decimal text; `null` when the key does not expire. */
  ttlMs: string | null;
  memoryBytes: U64Text | null;
  encoding: string | null;
  length: U64Text | null;
}

export interface ValuePage {
  cursor: string;
  count: number;
}

export interface FieldValue {
  field: Value;
  value: Value;
}

export interface ListItem {
  index: U64Text;
  value: Value;
}

export interface ScoredMember {
  member: Value;
  /** A float, or `{ t: 'other', v }` with Redis's text `inf`, `-inf` or `nan` (crates/driver-redis/src/reply.rs). */
  score: Value;
}

export interface StreamEntry {
  id: string;
  fields: FieldValue[];
}

export type ValueData =
  | { kind: 'string'; value: Value; offset: U64Text; truncated: boolean }
  | { kind: 'hash'; entries: FieldValue[] }
  | { kind: 'list'; items: ListItem[] }
  | { kind: 'set'; members: Value[] }
  | { kind: 'zset'; members: ScoredMember[] }
  | { kind: 'stream'; entries: StreamEntry[] }
  | { kind: 'json'; document: Value }
  /** A time series: `TS.INFO` and samples oldest first; `nextCursor` is the next timestamp. */
  | { kind: 'timeSeries'; info: TsInfo; samples: TsSample[] }
  /** A Bloom or Cuckoo filter, Count-Min sketch, Top-K or t-digest: its info fields; `top` only for a Top-K. */
  | { kind: 'probabilistic'; moduleKind: ModuleKind; fields: InfoField[]; top: TopKItem[] };

// Modules (crates/driver-redis/src/modules.rs, app/src-tauri/src/m2/redis_modules.rs).

/** A module key type the browser reads (`TSDB-TYPE`, `MBbloom--`, `MBbloomCF`, `CMSk-TYPE`, `TopK-TYPE`, `TDIS-TYPE`). */
export type ModuleKind = 'timeSeries' | 'bloom' | 'cuckoo' | 'countMin' | 'topK' | 'tDigest';

/** The module kind of a key type, or null for core types and unknown modules. */
export function moduleKindOf(type: KeyType): ModuleKind | null {
  if (typeof type !== 'object') return null;
  switch (type.other) {
    case 'TSDB-TYPE':
      return 'timeSeries';
    case 'MBbloom--':
    case 'bloomfltr':
      return 'bloom';
    case 'MBbloomCF':
      return 'cuckoo';
    case 'CMSk-TYPE':
      return 'countMin';
    case 'TopK-TYPE':
      return 'topK';
    case 'TDIS-TYPE':
      return 'tDigest';
    default:
      return null;
  }
}

export const MODULE_KIND_LABELS: Readonly<Record<ModuleKind, string>> = {
  timeSeries: 'Time series',
  bloom: 'Bloom filter',
  cuckoo: 'Cuckoo filter',
  countMin: 'Count-Min sketch',
  topK: 'Top-K',
  tDigest: 't-digest',
};

/** A field of an info reply. */
export interface InfoField {
  name: string;
  value: Value;
}

export interface TsLabel {
  name: string;
  value: string;
}

export interface TsRule {
  destKey: string;
  bucketMs: U64Text;
  aggregation: string;
  alignMs: string;
}

/** What `TS.ALTER` changes, as read; send it back unchanged as `original`. */
export interface TsMeta {
  /** `"0"` keeps samples forever. */
  retentionMs: U64Text;
  labels: TsLabel[];
  /** `block`, `first`, `last`, `min`, `max` or `sum`; null for the server default. */
  duplicatePolicy: string | null;
}

export interface TsInfo {
  totalSamples: U64Text;
  /** Null while the series is empty. Timestamps are milliseconds as decimal text. */
  firstTimestamp: string | null;
  lastTimestamp: string | null;
  meta: TsMeta;
  sourceKey: string | null;
  rules: TsRule[];
  /** Every scalar field of `TS.INFO`. */
  fields: InfoField[];
}

export interface TsSample {
  /** Milliseconds as decimal text. */
  timestamp: string;
  /** A float, or `{ t: 'other', v: 'nan' }`. */
  value: Value;
}

/** `TsRangeQuery`: `from`/`to` are inclusive milliseconds (null: open end); `cursor` is the previous page's `next`. */
export interface TsRangeQuery {
  from: string | null;
  to: string | null;
  reverse: boolean;
  count: number;
  cursor: string | null;
}

export interface TsRangePage {
  samples: TsSample[];
  next: string | null;
}

export interface TopKItem {
  item: Value;
  count: U64Text;
}

/** A read-only query of a probabilistic key, tagged by `op` (`redis_module_query`). */
export type ModuleQuery =
  | { op: 'bloomExists'; items: Value[] }
  | { op: 'cuckooExists'; items: Value[] }
  | { op: 'cuckooCount'; items: Value[] }
  | { op: 'cmsQuery'; items: Value[] }
  | { op: 'topkQuery'; items: Value[] }
  | { op: 'tdigestQuantile'; quantiles: Value[] }
  | { op: 'tdigestCdf'; values: Value[] }
  | { op: 'tdigestRank'; values: Value[] }
  | { op: 'tdigestRevRank'; values: Value[] }
  | { op: 'tdigestByRank'; ranks: Value[] }
  | { op: 'tdigestTrimmedMean'; low: Value; high: Value };

/** One input and the answer: a bool for exists queries, a number otherwise. */
export interface QueryRow {
  input: Value;
  output: Value;
}

export interface QueryResult {
  command: string;
  rows: QueryRow[];
}

export type ModuleFeature = 'json' | 'search' | 'timeSeries' | 'bloom' | 'cuckoo' | 'countMin' | 'topK' | 'tDigest';

export interface FeatureSupport {
  feature: ModuleFeature;
  available: boolean;
  /** Why it is missing (or cannot be told); null when available. */
  reason: string | null;
}

export interface LoadedModule {
  name: string;
  version: string | null;
}

/** `ModuleReport`: one entry per feature; `modules` is `MODULE LIST`, null when the user may not run it. */
export interface ModuleReport {
  features: FeatureSupport[];
  modules: LoadedModule[] | null;
  /** `TYPE` names of the module keys the available features store: the module types the scan filter offers. */
  keyTypes: string[];
}

/**
 * True when the server has a module feature (JSON, Search, time series,
 * probabilistic types) or a loaded module other than Valkey's built-in
 * `lua`: the object tree then shows "Modules and Search". Also true when
 * the report cannot tell (the user may run neither COMMAND INFO nor MODULE
 * LIST: no module list, and every feature says so, see
 * crates/driver-redis/src/modules.rs `modules`), so the panel stays and says
 * what it cannot tell.
 */
export function hasModules(report: ModuleReport): boolean {
  const cannotTell = report.modules === null && report.features.length > 0 && report.features.every((f) => !f.available && (f.reason ?? '').startsWith('Varve cannot tell'));
  return cannotTell || report.features.some((f) => f.available) || (report.modules ?? []).some((m) => m.name.toLowerCase() !== 'lua');
}

export interface SearchAttribute {
  identifier: string;
  attribute: string;
  fieldType: string;
  /** The options as `FT.CREATE` takes them, in its order. */
  options: string[];
  /** `FT.INFO` settings Varve cannot give back to `FT.CREATE`. */
  unknown: string[];
}

/** The module that answered `FT.INFO`: RediSearch (Redis 8, Redis Stack) or valkey-search. */
export type SearchEngine = 'rediSearch' | 'valkeySearch';

/**
 * `SearchIndex` (`FT.INFO`). `ddl` is the definition rebuilt as one `FT.CREATE` line in the console's quoting
 * (copy and run it as is); `ddlDisplay` the same over several lines, for reading only. Both are null when the
 * reply holds settings that would not recreate the index: `notes` says why and `raw` has the reply.
 */
export interface SearchIndex {
  name: string;
  engine: SearchEngine;
  keyType: string | null;
  prefixes: string[];
  filter: string | null;
  language: string | null;
  languageField: string | null;
  /** `INDEXALL ENABLE`: every key is indexed, also one without the attributes. */
  indexesAll: boolean;
  defaultScore: string | null;
  scoreField: string | null;
  payloadField: string | null;
  indexOptions: string[];
  stopwords: string[] | null;
  attributes: SearchAttribute[];
  numDocs: U64Text | null;
  indexingFailures: U64Text | null;
  percentIndexed: number | null;
  stats: InfoField[];
  ddl: string | null;
  ddlDisplay: string | null;
  notes: string[];
  raw: string;
}

export function redisModules(sessionId: string): Promise<ModuleReport> {
  return call<ModuleReport>('redis_modules', { sessionId });
}

/** `FT._LIST`; rejects with `unsupported` (code `SEARCH_UNAVAILABLE`) on servers without Search. */
export function redisSearchIndexes(sessionId: string): Promise<string[]> {
  return call<string[]>('redis_search_indexes', { sessionId });
}

export function redisSearchIndex(sessionId: string, name: string): Promise<SearchIndex | null> {
  return call<SearchIndex | null>('redis_search_index', { sessionId, name });
}

/** A page of samples (`TS.RANGE` / `TS.REVRANGE`); null when the key does not exist. */
export function redisTsRange(sessionId: string, key: string, query: TsRangeQuery): Promise<TsRangePage | null> {
  return call<TsRangePage | null>('redis_ts_range', { sessionId, key, query });
}

/** A read-only query of a probabilistic key (read-only sessions too); null when the key does not exist. */
export function redisModuleQuery(sessionId: string, key: string, query: ModuleQuery): Promise<QueryResult | null> {
  return call<QueryResult | null>('redis_module_query', { sessionId, key, query });
}

// Console help (crates/driver-redis/src/docs.rs).

/** `CommandHelp`: `COMMAND DOCS` text, or (`fromDocs: false`) the arity and flags of `COMMAND INFO` with a `note`. */
export interface CommandHelp {
  name: string;
  known: boolean;
  fromDocs: boolean;
  summary: string | null;
  syntax: string | null;
  since: string | null;
  group: string | null;
  complexity: string | null;
  deprecatedSince: string | null;
  replacedBy: string | null;
  subcommands: string[];
  arity: number | null;
  flags: string[];
  note: string | null;
}

/** Help for a command (`GET`, `CONFIG SET`) on the tab's session. */
export function redisCommandHelp(sessionId: string, name: string): Promise<CommandHelp> {
  return call<CommandHelp>('redis_command_help', { sessionId, name });
}

// Bulk TTL (crates/driver-redis/src/bulk.rs, app/src-tauri/src/m2/redis_bulk.rs).

/** The key browser's filter. */
export interface BulkFilter {
  pattern: string;
  keyType: Exclude<KeyType, 'unknown'> | null;
}

export type BulkTtlAction = { kind: 'expire'; ttlMs: U64Text } | { kind: 'persist' };

/**
 * A preview: `keys` is a lower bound unless `complete`; `db` is the database counted. `cluster`: the server
 * runs in cluster mode, so the count and the change only reach the keys of the node this connection is on.
 */
export interface BulkCount {
  keys: U64Text;
  complete: boolean;
  sample: string[];
  db: number;
  cluster: boolean;
}

export interface BulkTtlRequest {
  filter: BulkFilter;
  action: BulkTtlAction;
  /** The preview's database; the start is refused when the tab is on another one now. */
  db: number;
}

export interface BulkCounts {
  steps: U64Text;
  matched: U64Text;
  changed: U64Text;
  skipped: U64Text;
}

export type BulkTtlState = 'running' | 'complete' | 'stopped' | 'failed';

export interface BulkTtlStatus {
  jobId: string;
  state: BulkTtlState;
  counts: BulkCounts;
  error: DriverError | null;
  elapsedMs: number;
  db: number;
  action: BulkTtlAction;
  filter: BulkFilter;
}

/** Counts the keys the filter matches (refused on read-only sessions). */
export function redisBulkTtlPreview(sessionId: string, filter: BulkFilter): Promise<BulkCount> {
  return call<BulkCount>('redis_bulk_ttl_preview', { sessionId, filter });
}

/** A started bulk job; pass `jobId` to the status and stop calls. */
export interface BulkTtlJob {
  jobId: string;
}

/** Starts the change in the background; rejects with `confirmationRequired` until `confirmed` on production. */
export function redisBulkTtlStart(sessionId: string, request: BulkTtlRequest, confirmed: boolean): Promise<BulkTtlJob> {
  return call<BulkTtlJob>('redis_bulk_ttl_start', { sessionId, request, confirmed });
}

/** The tab's bulk jobs, newest first: a reopened dialog finds a job that still runs. */
export function redisBulkTtlJobs(sessionId: string): Promise<BulkTtlStatus[]> {
  return call<BulkTtlStatus[]>('redis_bulk_ttl_jobs', { sessionId });
}

export function redisBulkTtlStatus(sessionId: string, jobId: string): Promise<BulkTtlStatus> {
  return call<BulkTtlStatus>('redis_bulk_ttl_status', { sessionId, jobId });
}

/** Stops the job before its next step; keys changed so far stay changed. */
export function redisBulkTtlStop(sessionId: string, jobId: string): Promise<BulkTtlStatus> {
  return call<BulkTtlStatus>('redis_bulk_ttl_stop', { sessionId, jobId });
}

export interface ValueChunk {
  keyType: KeyType;
  total: U64Text | null;
  nextCursor: string | null;
  data: ValueData;
}

export interface DbKeyspace {
  db: number;
  keys: U64Text;
  expires: U64Text;
}

/** The name `TYPE` reports for a key type. */
export function keyTypeName(type: KeyType): string {
  if (typeof type === 'object') return type.other;
  return type === 'json' ? 'ReJSON-RL' : type;
}

export function redisScan(
  sessionId: string,
  cursor: U64Text,
  pattern: string,
  keyType: Exclude<KeyType, 'unknown'> | null,
  count: number,
): Promise<ScanPage> {
  return call<ScanPage>('redis_scan', { sessionId, cursor, pattern, keyType, count });
}

export function redisKeyInfo(sessionId: string, key: string): Promise<KeyInfo | null> {
  return call<KeyInfo | null>('redis_key_info', { sessionId, key });
}

export function redisReadValue(sessionId: string, key: string, page: ValuePage): Promise<ValueChunk | null> {
  return call<ValueChunk | null>('redis_read_value', { sessionId, key, page });
}

/** Switches the logical database of a Redis session. */
export async function redisSelectDb(sessionId: string, db: number): Promise<void> {
  await call<unknown>('redis_select_db', { sessionId, db });
}

export function redisKeyspace(sessionId: string): Promise<DbKeyspace[]> {
  return call<DbKeyspace[]>('redis_keyspace', { sessionId });
}

/** `ChannelSubscribers` in crates/live-stats/src/redis/pubsub.rs. */
export interface ChannelSubscribers {
  /** Channel name: text, or bytes when not UTF-8. */
  channel: Value;
  subscribers: number;
}

/** `PubSubSnapshot` in crates/live-stats/src/redis/pubsub.rs: Pub/Sub state at one moment. */
export interface PubSubSnapshot {
  /** Channels with at least one subscriber, most subscribers first. */
  channels: ChannelSubscribers[];
  /** True when more than `maxChannels` channels matched. */
  truncated: boolean;
  /** Pattern subscriptions on the server (`PUBSUB NUMPAT`). */
  patternSubscriptions: number;
  /** Shard channels (Redis 7.0 and newer); `null` on older servers. */
  shardChannels: ChannelSubscribers[] | null;
}

/** Active channels and subscriber counts (`PUBSUB CHANNELS` + `NUMSUB`), read on the admin session. */
export function redisPubsub(sessionId: string, pattern: string | null = null, maxChannels: number | null = null): Promise<PubSubSnapshot> {
  return call<PubSubSnapshot>('redis_pubsub', { sessionId, pattern, maxChannels });
}

// Value editors (B1: crates/driver-redis/src/edit.rs, app/src-tauri/src/m2/redis_edit.rs).


/**
 * Why a key version has no digest: reading the content took too long, the ACL user may not read it all,
 * (`deferred`) the key is larger than what is read when it is opened (or a JSON or module key), so the
 * backend reads it right after in the background (`KeyVersion.digestRead`), or (`stalled`) that read got no
 * answer in time. A whole-key plan without a digest is unchecked and always asks first.
 */
export type DigestMissing = 'tooLarge' | 'notPermitted' | 'deferred' | 'stalled';

/**
 * `KeyVersion`: what an edit checks the key against. `digest` covers the whole
 * content (read in pieces); it is null when `digestMissing` says why. Whole-key
 * changes then cannot see a change that keeps the length (`RedisEditPlan.unchecked`).
 */
export interface KeyVersion {
  keyType: KeyType;
  length: U64Text | null;
  digest: string | null;
  digestMissing: DigestMissing | null;
  /** When the key expires (unix ms, server clock); null without a TTL. Checked by `expire` and `persist`. */
  expireAtMs: U64Text | null;
  /**
   * The background digest read of a `deferred` version: send the version back unchanged with a plan, and
   * cancel the read with `redisKeyDigestCancel` when the viewer closes or reads the key again.
   */
  digestRead?: string | null;
  /** The logical database the key was read on; a plan on a tab now on another database is refused. */
  db?: number | null;
}

/** `KeyExpect`: the key must not exist (a new key), or must still be what the UI read. */
export type KeyExpect =
  | { state: 'absent' }
  | { state: 'present'; keyType: KeyType; version: KeyVersion | null };

/** `KeyEdit`, tagged by `op`. Values travel exactly as `redis_read_value` returned them. */
export type KeyEdit =
  | { op: 'stringSet'; value: Value; original: Value | null }
  | { op: 'jsonSet'; document: string; original: string | null }
  | { op: 'hashSet'; field: Value; value: Value; original: Value | null }
  | { op: 'hashDelete'; field: Value; original: Value }
  | { op: 'listPush'; end: 'head' | 'tail'; values: Value[] }
  | { op: 'listSet'; index: U64Text; value: Value; original: Value }
  | { op: 'listRemove'; index: U64Text; original: Value }
  | { op: 'setAdd'; members: Value[] }
  | { op: 'setRemove'; member: Value }
  | { op: 'zsetAdd'; member: Value; score: Value }
  | { op: 'zsetScore'; member: Value; score: Value; original: Value }
  | { op: 'zsetRemove'; member: Value; original: Value }
  | { op: 'streamAdd'; id: string | null; fields: FieldValue[] }
  | { op: 'streamDelete'; id: string }
  | { op: 'streamTrim'; strategy: 'maxLen' | 'minId'; threshold: string; approximate: boolean }
  | { op: 'expire'; ttlMs: U64Text }
  | { op: 'persist' }
  | { op: 'rename'; newKey: string; replace: boolean }
  | { op: 'delete' }
  /** `TS.ADD`; `timestamp` null or `*` uses the server clock. With a timestamp the series must have no sample there. */
  | { op: 'tsAdd'; timestamp: string | null; value: Value }
  /** `TS.DEL` from `from` to `to` (ms, or `-` / `+`): a whole-key change, needs `expect.version`. */
  | { op: 'tsDeleteRange'; from: string; to: string }
  /** `TS.ALTER`, checked against `original` inside the same script; must be the only edit of its plan. */
  | { op: 'tsAlter'; retentionMs: U64Text | null; labels: TsLabel[] | null; duplicatePolicy: string | null; original: TsMeta }
  | { op: 'bloomAdd'; items: Value[] }
  | { op: 'cuckooAdd'; items: Value[]; nx: boolean }
  | { op: 'cuckooDelete'; item: Value }
  | { op: 'cmsIncrBy'; items: { item: Value; increment: U64Text }[] }
  | { op: 'topkAdd'; items: Value[] }
  | { op: 'tdigestAdd'; values: Value[] };

export type KeyEditOp = KeyEdit['op'];

/** `RedisEditRequest`: 1 to 10000 edits of one key. `key` is the lossless display text. */
export interface RedisEditRequest {
  key: string;
  expect: KeyExpect;
  edits: KeyEdit[];
}

/** `RedisEditPlan`: what an apply will run, shown before it runs. */
export interface RedisEditPlan {
  key: string;
  /** The logical database the plan applies to. */
  db: number;
  /** The command lines, long values shortened. */
  commands: string[];
  /** What is checked before anything runs, in words. */
  checks: string[];
  risk: 'read' | 'write' | 'destructive';
  /** True when the plan deletes or overwrites data that is not an edited value. */
  destructive: boolean;
  /**
   * True when a whole-key change cannot be checked for concurrent changes (the
   * version has no digest): `warnings` says why, and applying always asks.
   */
  unchecked: boolean;
  warnings: string[];
}

export type EditConflictReason = 'typeChanged' | 'versionChanged' | 'valueChanged' | 'targetExists' | 'concurrentChange';

/** `RedisEditOutcome`, tagged by `status`. */
export type RedisEditOutcome =
  | { status: 'applied'; replies: Value[] }
  | { status: 'conflict'; edit: number | null; reason: EditConflictReason; message: string }
  | { status: 'failed'; command: number; error: DriverError };

/** Type, length and digest of a key, to send back as `expect.version`; `null` when the key does not exist. */
export function redisKeyVersion(sessionId: string, key: string): Promise<KeyVersion | null> {
  return call<KeyVersion | null>('redis_key_version', { sessionId, key });
}

/** Stops the background digest read of a version (`KeyVersion.digestRead`); unknown ids are fine. */
export async function redisKeyDigestCancel(sessionId: string, readId: string): Promise<void> {
  await call<unknown>('redis_key_digest_cancel', { sessionId, readId });
}

export function redisEditPlan(sessionId: string, request: RedisEditRequest): Promise<Planned<RedisEditPlan>> {
  return call<Planned<RedisEditPlan>>('redis_edit_plan', { sessionId, request });
}

/** Runs a stored plan; rejects with `confirmationRequired` for plans that need a yes first. */
export function redisEditApply(sessionId: string, planId: string, confirmed: boolean): Promise<RedisEditOutcome> {
  return call<RedisEditOutcome>('redis_edit_apply', { sessionId, planId, confirmed });
}

// Pub/Sub (B1: crates/driver-redis/src/pubsub.rs, app/src-tauri/src/m2/pubsub.rs).

/** The Tauri event every Pub/Sub subscription reports on. */
export const PUBSUB_EVENT = 'redis-pubsub';

/** Channel names, glob patterns and sharded channels (Redis 7.0+, `SSUBSCRIBE`), as display text. */
export interface PubSubNames {
  channels: string[];
  patterns: string[];
  shardChannels?: string[];
}

/** What a subscribed name is. */
export type PubSubNameKind = 'channel' | 'pattern' | 'shardChannel';

export interface PubSubSubscription {
  subscriptionId: string;
  event: typeof PUBSUB_EVENT;
}

/** A subscription as the backend lists it: only names the server confirmed. */
export interface PubSubSubscriptionInfo {
  subscriptionId: string;
  channels: string[];
  patterns: string[];
  shardChannels: string[];
  state: 'subscribed' | 'reconnecting';
  delivered: number;
  dropped: number;
}

/** One received message. `size` is the full payload size; `payload` is shortened when `truncated`. */
export interface PubSubMessage {
  channel: string;
  pattern: string | null;
  payload: Value;
  size: U64Text;
  truncated: boolean;
  /** Unix milliseconds. */
  receivedAt: number;
}

/** `PubSubEvent`, tagged by `kind`. */
export type PubSubEvent =
  | { kind: 'batch'; subscriptionId: string; seq: number; messages: PubSubMessage[]; dropped: number }
  /**
   * The server confirmed a (un)subscribe; `count` is how many names this
   * connection is subscribed to now. `nameKind` says whether `name` is a
   * channel or a pattern.
   */
  | { kind: 'subscribed' | 'unsubscribed'; subscriptionId: string; nameKind: PubSubNameKind; name: string; count: number }
  /**
   * The server refused something; when it refused a name, `nameKind` and `name` say which, and that name is
   * no longer part of the subscription.
   */
  | {
      kind: 'error';
      subscriptionId: string;
      message: string;
      code: string | null;
      nameKind?: PubSubNameKind | null;
      name?: string | null;
    }
  /** The connection was lost; Varve opens it again (attempts 1 to 5, starting 1, 3, 7, 15 and 31 s after the loss). */
  | { kind: 'reconnecting'; subscriptionId: string; attempt: number; reason: string }
  /**
   * The subscription is back and the server answered for every name (refused ones came as `error` events);
   * messages published between `lostAt` and `resumedAt` (unix ms) were missed.
   */
  | { kind: 'gap'; subscriptionId: string; reason: string; lostAt: number; resumedAt: number }
  | { kind: 'closed'; subscriptionId: string; reason: string };

/**
 * Opens a subscription on its own connection. `resubscribe` (default true)
 * opens a lost connection again (`reconnecting`, then `gap`).
 */
export function redisPubsubSubscribe(
  sessionId: string,
  names: PubSubNames,
  options: { maxPayloadBytes?: number | null; resubscribe?: boolean } = {},
): Promise<PubSubSubscription> {
  return call<PubSubSubscription>('redis_pubsub_subscribe', {
    sessionId,
    channels: names.channels,
    patterns: names.patterns,
    shardChannels: names.shardChannels ?? [],
    maxPayloadBytes: options.maxPayloadBytes ?? null,
    resubscribe: options.resubscribe ?? true,
  });
}

export async function redisPubsubChange(subscriptionId: string, add: PubSubNames, remove: PubSubNames): Promise<void> {
  await call<unknown>('redis_pubsub_change', { subscriptionId, add, remove });
}

/** Acknowledges every batch up to `seq`, so the backend sends more. */
export async function redisPubsubAck(subscriptionId: string, seq: number): Promise<void> {
  await call<unknown>('redis_pubsub_ack', { subscriptionId, seq });
}

export async function redisPubsubUnsubscribe(subscriptionId: string): Promise<void> {
  await call<unknown>('redis_pubsub_unsubscribe', { subscriptionId });
}

export function redisPubsubSubscriptions(sessionId: string): Promise<PubSubSubscriptionInfo[]> {
  return call<PubSubSubscriptionInfo[]>('redis_pubsub_subscriptions', { sessionId });
}

/** `PUBLISH` on the tab's session; returns how many clients received it. */
export function redisPublish(sessionId: string, channel: string, message: Value, confirmed: boolean): Promise<number> {
  return call<number>('redis_publish', { sessionId, channel, message, confirmed });
}

// Sorted set ranges (B1).

/** One end of a sorted set range: a score `Value` for `score`, a member `Value` for `lex`. */
export type ZsetBound = { kind: 'unbounded' } | { kind: 'inclusive'; value: Value } | { kind: 'exclusive'; value: Value };

/** `ZsetRange`: `min` is always the low end, also with `reverse`. */
export interface ZsetRange {
  by: 'score' | 'lex';
  min: ZsetBound;
  max: ZsetBound;
  reverse: boolean;
  offset: U64Text;
  count: number;
}

/** `ZsetRangePage`: `nextOffset` continues the same range (null: done). Lex pages have null scores. */
export interface ZsetRangePage {
  members: ScoredMember[];
  nextOffset: U64Text | null;
}

/** `ZRANGE ... BYSCORE|BYLEX [REV] LIMIT`; `null` when the key does not exist. */
export function redisZsetRange(sessionId: string, key: string, range: ZsetRange): Promise<ZsetRangePage | null> {
  return call<ZsetRangePage | null>('redis_zset_range', { sessionId, key, range });
}

// Stream consumer groups (B1).

/** `StreamInfo` (`XINFO STREAM`); numbers are decimal strings. */
export interface StreamInfo {
  length: U64Text;
  lastGeneratedId: string;
  groups: U64Text;
  firstEntryId: string | null;
  lastEntryId: string | null;
  maxDeletedEntryId: string | null;
  entriesAdded: U64Text | null;
  recordedFirstEntryId: string | null;
}

/** `StreamGroup` (`XINFO GROUPS`); `entriesRead` and `lag` need Redis 7.0. */
export interface StreamGroup {
  name: string;
  consumers: U64Text;
  pending: U64Text;
  lastDeliveredId: string;
  entriesRead: U64Text | null;
  lag: U64Text | null;
}

/** `StreamConsumer` (`XINFO CONSUMERS`); `inactiveMs` needs Redis 7.2. */
export interface StreamConsumer {
  name: string;
  pending: U64Text;
  idleMs: U64Text;
  inactiveMs: U64Text | null;
}

/** `PendingSummary` (`XPENDING key group`). */
export interface PendingSummary {
  count: U64Text;
  minId: string | null;
  maxId: string | null;
  consumers: { name: string; pending: U64Text }[];
}

/** `PendingQuery`: `start` null for the beginning, else the previous page's `nextStart`. */
export interface PendingQuery {
  start: string | null;
  count: number;
  consumer: string | null;
  minIdleMs: U64Text | null;
}

export interface PendingEntry {
  id: string;
  consumer: string;
  idleMs: U64Text;
  deliveries: U64Text;
}

export interface PendingPage {
  entries: PendingEntry[];
  nextStart: string | null;
}

/** `StreamGroupChange`, tagged by `op`; planned with `redis_stream_group_plan`, applied with `redis_edit_apply`. */
export type StreamGroupChange =
  | { op: 'groupCreate'; group: string; id: string; mkStream: boolean; entriesRead: U64Text | null }
  | { op: 'groupDestroy'; group: string; expectedPending: U64Text }
  | { op: 'groupSetId'; group: string; id: string; entriesRead: U64Text | null }
  | { op: 'groupDelConsumer'; group: string; consumer: string; expectedPending: U64Text }
  | { op: 'ack'; group: string; ids: string[] }
  | { op: 'claim'; group: string; consumer: string; minIdleMs: U64Text; ids: string[]; justId: boolean }
  | { op: 'autoClaim'; group: string; consumer: string; minIdleMs: U64Text; start: string; count: number; justId: boolean };

export function redisStreamInfo(sessionId: string, key: string): Promise<StreamInfo | null> {
  return call<StreamInfo | null>('redis_stream_info', { sessionId, key });
}

export function redisStreamGroups(sessionId: string, key: string): Promise<StreamGroup[]> {
  return call<StreamGroup[]>('redis_stream_groups', { sessionId, key });
}

export function redisStreamConsumers(sessionId: string, key: string, group: string): Promise<StreamConsumer[]> {
  return call<StreamConsumer[]>('redis_stream_consumers', { sessionId, key, group });
}

export function redisStreamPendingSummary(sessionId: string, key: string, group: string): Promise<PendingSummary> {
  return call<PendingSummary>('redis_stream_pending_summary', { sessionId, key, group });
}

export function redisStreamPending(sessionId: string, key: string, group: string, page: PendingQuery): Promise<PendingPage> {
  return call<PendingPage>('redis_stream_pending', { sessionId, key, group, page });
}

export function redisStreamGroupPlan(sessionId: string, key: string, change: StreamGroupChange): Promise<Planned<RedisEditPlan>> {
  return call<Planned<RedisEditPlan>>('redis_stream_group_plan', { sessionId, key, change });
}
