import type { EditPlan, Planned } from './plans';
import { call } from './invoke';
import type { Engine } from './records';
import type { Value } from './sessions';

export type { EditPlan, Planned } from './plans';

// SQL live panels (crates/live-stats/src/sql). All run on the profile's admin session.

/** `Process` in crates/live-stats/src/sql/process.rs. */
export interface Process {
  /** Backend pid (PostgreSQL) or connection id (MySQL, MariaDB). */
  id: number;
  /** True for the admin connection the panel runs on. */
  own: boolean;
  user: string | null;
  database: string | null;
  application: string | null;
  clientAddress: string | null;
  clientHostname: string | null;
  clientPort: number | null;
  backendType: string | null;
  command: string | null;
  state: string | null;
  waitEventType: string | null;
  waitEvent: string | null;
  backendStart: string | null;
  xactStart: string | null;
  queryStart: string | null;
  durationMs: number | null;
  /** Statement text with credentials redacted. */
  query: string | null;
  blockedBy: number[];
  leaderId: number | null;
  /** Decimal text: PostgreSQL's query id is a 64-bit hash that a number would round (it must match `pg_stat_statements.queryid`). */
  queryId: string | null;
  progress: number | null;
  stage: number | null;
  maxStage: number | null;
  memoryBytes: number | null;
  rowsExamined: number | null;
}

export interface ProcessList {
  /** Connection id of the admin session that read the list. */
  ownId: number | null;
  /** The view the list came from. */
  source: string;
  processes: Process[];
}

export type KillTarget = 'query' | 'connection';

export interface KillOutcome {
  id: number;
  target: KillTarget;
  statement: string;
  /** False when the process no longer exists or cannot be signalled. */
  signalled: boolean;
  message: string | null;
}

export type VariableType = 'boolean' | 'integer' | 'real' | 'enum' | 'set' | 'string';
export type EditScope = 'session' | 'global' | 'persist' | 'persistOnly';

/** `ServerVariable` in crates/live-stats/src/sql/variables.rs. */
export interface ServerVariable {
  name: string;
  value: string | null;
  sessionValue: string | null;
  globalValue: string | null;
  defaultValue: string | null;
  resetValue: string | null;
  unit: string | null;
  category: string | null;
  description: string | null;
  varType: VariableType;
  typeInferred: boolean;
  context: string | null;
  /** Offer only these scopes. */
  settableScopes: EditScope[];
  dynamic: boolean | null;
  readOnly: boolean;
  restartRequired: boolean;
  pendingRestart: boolean;
  minValue: string | null;
  maxValue: string | null;
  enumValues: string[];
  source: string | null;
  sourceFile: string | null;
  setBy: string | null;
  setTime: string | null;
  persistedValue: string | null;
  /** The values hold a secret and are masked: never prefill an editor with them. */
  masked: boolean;
}

export type VariableChange = { action: 'set'; scope: EditScope; value: string } | { action: 'reset'; scope: EditScope };



export interface DatabaseSize {
  name: string;
  sizeBytes: number | null;
  connections: number | null;
}

export type ReplicationRole = 'primary' | 'replica' | 'galeraMember' | 'standalone' | 'unknown';

export interface ReplicaInfo {
  name: string | null;
  address: string | null;
  state: string | null;
  syncState: string | null;
  lagSeconds: number | null;
  lagBytes: number | null;
}

export interface UpstreamInfo {
  channel: string | null;
  sourceHost: string | null;
  sourcePort: number | null;
  state: string | null;
  ioRunning: string | null;
  sqlRunning: string | null;
  lagSeconds: number | null;
  lagBytes: number | null;
  lastReplayAgeSeconds: number | null;
  lagNote: string | null;
  receivePosition: string | null;
  replayPosition: string | null;
  primaryPosition: string | null;
  primaryLagBytes: number | null;
  lastError: string | null;
  gtidPosition: string | null;
}

export interface Replication {
  role: ReplicationRole;
  /** False when a privilege was missing; `message` says which. */
  visible: boolean;
  message: string | null;
  replicas: ReplicaInfo[];
  upstream: UpstreamInfo[];
}

export interface StatusValue {
  name: string;
  value: string;
}

/** `GaleraStatus` (MariaDB). */
export interface GaleraStatus {
  /** False: not a cluster member. */
  enabled: boolean;
  clusterSize: number | null;
  clusterStatus: string | null;
  ready: boolean | null;
  connected: boolean | null;
  localStateComment: string | null;
  flowControlPaused: number | null;
  clusterStateUuid: string | null;
  providerName: string | null;
  providerVersion: string | null;
  values: StatusValue[];
}

/** `ServerStatus` in crates/live-stats/src/sql/status.rs. */
export interface ServerStatus {
  engine: Engine;
  version: string;
  versionDetail: string | null;
  startedAt: string | null;
  uptimeSeconds: number | null;
  connections: number | null;
  maxConnections: number | null;
  maxUsedConnections: number | null;
  activeQueries: number | null;
  /** 0 to 1; `null` before any read. */
  cacheHitRatio: number | null;
  databases: DatabaseSize[];
  replication: Replication;
  galera: GaleraStatus | null;
}

export function processList(sessionId: string): Promise<ProcessList> {
  return call<ProcessList>('process_list', { sessionId });
}

export function killProcess(sessionId: string, id: number, target: KillTarget): Promise<KillOutcome> {
  return call<KillOutcome>('kill_process', { sessionId, id, target });
}

export function serverVariables(sessionId: string): Promise<ServerVariable[]> {
  return call<ServerVariable[]>('server_variables', { sessionId });
}

export function variableChangePlan(sessionId: string, name: string, change: VariableChange): Promise<Planned<EditPlan>> {
  return call<Planned<EditPlan>>('variable_change_plan', { sessionId, name, change });
}

export async function variableChangeApply(sessionId: string, planId: string, confirmed: boolean): Promise<void> {
  await call<unknown>('variable_change_apply', { sessionId, planId, confirmed });
}

export function serverStatus(sessionId: string): Promise<ServerStatus> {
  return call<ServerStatus>('server_status', { sessionId });
}

export function galeraStatus(sessionId: string): Promise<GaleraStatus> {
  return call<GaleraStatus>('galera_status', { sessionId });
}

// Redis live panels (crates/live-stats/src/redis). All run on the admin session.

export interface InfoField {
  key: string;
  value: string;
}

export interface MemorySection {
  usedMemory: number | null;
  usedMemoryRss: number | null;
  usedMemoryPeak: number | null;
  usedMemoryOverhead: number | null;
  usedMemoryDataset: number | null;
  usedMemoryScriptsVm: number | null;
  totalSystemMemory: number | null;
  /** 0 for no limit. */
  maxmemory: number | null;
  maxmemoryPolicy: string | null;
  memFragmentationRatio: number | null;
  memAllocator: string | null;
  other: InfoField[];
}

export interface ClientsSection {
  connectedClients: number | null;
  blockedClients: number | null;
  trackingClients: number | null;
  pubsubClients: number | null;
  watchingClients: number | null;
  maxclients: number | null;
  clusterConnections: number | null;
  other: InfoField[];
}

export interface ServerSection {
  redisVersion: string | null;
  valkeyVersion: string | null;
  serverName: string | null;
  mode: string | null;
  os: string | null;
  archBits: number | null;
  processId: number | null;
  runId: string | null;
  tcpPort: number | null;
  uptimeInSeconds: number | null;
  hz: number | null;
  executable: string | null;
  configFile: string | null;
  other: InfoField[];
}

export interface ReplicaLink {
  ip: string | null;
  port: number | null;
  state: string | null;
  offset: number | null;
  lag: number | null;
  other: InfoField[];
}

export interface ReplicationSection {
  /** `master` or `slave`. */
  role: string | null;
  connectedReplicas: number | null;
  replicas: ReplicaLink[];
  masterHost: string | null;
  masterPort: number | null;
  masterLinkStatus: string | null;
  masterLastIoSecondsAgo: number | null;
  replicaReplOffset: number | null;
  masterReplid: string | null;
  masterReplOffset: number | null;
  replBacklogSize: number | null;
  other: InfoField[];
}

export interface PersistenceSection {
  loading: boolean | null;
  rdbChangesSinceLastSave: number | null;
  rdbBgsaveInProgress: boolean | null;
  rdbLastSaveTime: number | null;
  rdbLastBgsaveStatus: string | null;
  aofEnabled: boolean | null;
  aofRewriteInProgress: boolean | null;
  aofLastBgrewriteStatus: string | null;
  aofLastWriteStatus: string | null;
  other: InfoField[];
}

export interface StatsSection {
  totalConnectionsReceived: number | null;
  totalCommandsProcessed: number | null;
  instantaneousOpsPerSec: number | null;
  totalNetInputBytes: number | null;
  totalNetOutputBytes: number | null;
  rejectedConnections: number | null;
  expiredKeys: number | null;
  evictedKeys: number | null;
  keyspaceHits: number | null;
  keyspaceMisses: number | null;
  /** `null` before the first lookup. */
  hitRatio: number | null;
  pubsubChannels: number | null;
  pubsubPatterns: number | null;
  pubsubshardChannels: number | null;
  latestForkUsec: number | null;
  totalErrorReplies: number | null;
  other: InfoField[];
}

export interface CpuSection {
  usedCpuSys: number | null;
  usedCpuUser: number | null;
  usedCpuSysChildren: number | null;
  usedCpuUserChildren: number | null;
  other: InfoField[];
}

export interface ModuleInfo {
  name: string;
  version: number | null;
  api: number | null;
  usedBy: string[];
  using: string[];
  options: string[];
  other: InfoField[];
}

export interface CommandStat {
  /** `container|sub` for subcommands. */
  command: string;
  calls: number;
  usec: number;
  usecPerCall: number | null;
  rejectedCalls: number | null;
  failedCalls: number | null;
}

export interface InfoSection {
  name: string;
  fields: InfoField[];
}

export interface KeyspaceDb {
  db: number;
  keys: number;
  expires: number;
  avgTtlMs: number;
  other: InfoField[];
}

/** `RedisInfo` in crates/live-stats/src/redis/info.rs. The cluster and sentinel sections are read by the topology panel through `redis_topology`. */
export interface RedisInfo {
  flavor: 'redis' | 'valkey';
  mode: 'standalone' | 'cluster' | 'sentinel';
  server: ServerSection;
  clients: ClientsSection;
  memory: MemorySection;
  persistence: PersistenceSection;
  stats: StatsSection;
  replication: ReplicationSection;
  cpu: CpuSection;
  keyspace: KeyspaceDb[];
  modules: ModuleInfo[];
  commandstats: CommandStat[];
  /** Every other section unchanged (TLS, threads, error stats, ...). */
  sections: InfoSection[];
}

export interface SlowLogEntry {
  id: number;
  /** Unix seconds. */
  time: number;
  durationUs: number;
  /** Command and arguments; secrets are `(redacted)`. */
  args: Value[];
  clientAddr: string | null;
  clientName: string | null;
  extra: Value[];
}

export interface SlowLog {
  /** Newest first. */
  entries: SlowLogEntry[];
  length: number;
  /** `null` without `CONFIG` permission. */
  thresholdUs: number | null;
  maxLen: number | null;
}

export interface LatencyEvent {
  event: string;
  /** Unix seconds of the latest spike. */
  time: number;
  latestMs: number;
  maxMs: number;
  sumMs: number | null;
  count: number | null;
}

export interface LatencyReport {
  /** 0: the monitor is off; `null` without `CONFIG` permission. */
  thresholdMs: number | null;
  events: LatencyEvent[];
}

export interface LatencySample {
  time: number;
  ms: number;
}

/** `ClientInfo` in crates/live-stats/src/redis/clients.rs. */
export interface ClientInfo {
  id: number;
  /** The admin connection itself. */
  isSelf: boolean;
  addr: string | null;
  laddr: string | null;
  name: string | null;
  user: string | null;
  age: number | null;
  idle: number | null;
  flags: string;
  flagNames: string[];
  db: number | null;
  sub: number | null;
  psub: number | null;
  ssub: number | null;
  multi: number | null;
  qbuf: number | null;
  omem: number | null;
  totMem: number | null;
  cmd: string | null;
  resp: number | null;
  libName: string | null;
  libVer: string | null;
  other: InfoField[];
}

/** `MemoryScanOptions`: send every field, or omit the options for the defaults. */
export interface MemoryScanOptions {
  pattern: string | null;
  delimiter: string;
  depth: number;
  maxKeys: number;
  timeBudgetMs: number;
  scanCount: number;
  samples: number;
  topKeys: number;
  maxPrefixes: number;
}

/** The backend defaults of `MemoryScanOptions` (crates/live-stats/src/redis/memory.rs). */
export const DEFAULT_MEMORY_SCAN: Readonly<MemoryScanOptions> = {
  pattern: null,
  delimiter: ':',
  depth: 1,
  maxKeys: 10_000,
  timeBudgetMs: 10_000,
  scanCount: 500,
  samples: 5,
  topKeys: 10,
  maxPrefixes: 100,
};

export interface PrefixUsage {
  /** Including the trailing delimiter; `null` for keys without it. */
  prefix: Value | null;
  keys: number;
  bytes: number;
  avgBytes: number;
  maxKey: Value;
  maxBytes: number;
  /** Scaled to the whole database; `null` when unknown (incomplete scan with a pattern). */
  estimatedBytes: number | null;
}

export interface OtherPrefixes {
  prefixes: number;
  keys: number;
  bytes: number;
  estimatedBytes: number | null;
}

export interface BigKey {
  key: Value;
  bytes: number;
  length: number | null;
}

export interface TypeUsage {
  keyType: string;
  keys: number;
  bytes: number;
  /** Biggest first. */
  biggest: BigKey[];
}

/** Why a memory analysis stopped; `stopped`: the user stopped it (the result is partial). */
export type ScanStop = 'complete' | 'keyBudget' | 'timeBudget' | 'stopped';

export interface MemoryAnalysis {
  scannedKeys: number;
  dbSize: number;
  complete: boolean;
  stoppedBy: ScanStop;
  elapsedMs: number;
  totalBytes: number;
  prefixes: PrefixUsage[];
  otherPrefixes: OtherPrefixes | null;
  types: TypeUsage[];
}

export function redisInfo(sessionId: string): Promise<RedisInfo> {
  return call<RedisInfo>('redis_info', { sessionId });
}

/** `DatabaseKeys` in crates/live-stats/src/redis/info.rs. */
export interface RedisDatabaseKeys {
  index: number;
  keys: number;
  expires: number;
  avgTtlMs: number;
}

/** `Databases` in crates/live-stats/src/redis/info.rs: the logical databases the server has. */
export interface RedisDatabases {
  mode: 'standalone' | 'cluster' | 'sentinel';
  /** `CONFIG GET databases` (1 in cluster mode); `null` when the server does not tell. */
  count: number | null;
  /** Every database `0..count`, or when the count is unknown every database with keys. */
  databases: RedisDatabaseKeys[];
}

export function redisDatabases(sessionId: string): Promise<RedisDatabases> {
  return call<RedisDatabases>('redis_databases', { sessionId });
}

export function redisSlowlog(sessionId: string, count?: number): Promise<SlowLog> {
  return call<SlowLog>('redis_slowlog', { sessionId, count: count ?? null });
}

export async function redisSlowlogReset(sessionId: string): Promise<void> {
  await call<unknown>('redis_slowlog_reset', { sessionId });
}

export function redisLatency(sessionId: string): Promise<LatencyReport> {
  return call<LatencyReport>('redis_latency', { sessionId });
}

export function redisLatencyHistory(sessionId: string, event: string): Promise<LatencySample[]> {
  return call<LatencySample[]>('redis_latency_history', { sessionId, event });
}

/** Resets the given events (all when empty); resolves with the number reset. */
export function redisLatencyReset(sessionId: string, events: string[] = []): Promise<number> {
  return call<number>('redis_latency_reset', { sessionId, events });
}

export function redisClients(sessionId: string): Promise<ClientInfo[]> {
  return call<ClientInfo[]>('redis_clients', { sessionId });
}

export async function redisClientKill(sessionId: string, id: number): Promise<void> {
  await call<unknown>('redis_client_kill', { sessionId, id });
}

/**
 * Keeps the admin session busy for up to `timeBudgetMs`; the user's session stays free. `runId` (any unique
 * text, chosen by the caller) lets {@link redisMemoryAnalysisStop} stop it.
 */
export function redisMemoryAnalysis(sessionId: string, options: MemoryScanOptions | null, runId: string | null = null): Promise<MemoryAnalysis> {
  return call<MemoryAnalysis>('redis_memory_analysis', { sessionId, options, runId });
}

/**
 * Stops the analysis `runId`: it returns at its next step with what it measured (`stoppedBy: 'stopped'`,
 * `complete: false`). Unknown ids are fine; a stop sent before the run started stops it at once.
 */
export async function redisMemoryAnalysisStop(sessionId: string, runId: string): Promise<void> {
  await call<unknown>('redis_memory_analysis_stop', { sessionId, runId });
}

// Redis config, functions and topology (app/src-tauri/src/m2/live_redis.rs).

/** `ConfigEntry` in crates/live-stats/src/redis/config.rs. */
export interface ConfigEntry {
  /** Lower case. */
  name: string;
  /** Always `null` for a sensitive parameter. */
  value: string | null;
  /** The value is a secret and is masked. */
  sensitive: boolean;
  /** For a sensitive parameter: whether a value is set. */
  hasValue: boolean;
  /** Changing it is risky. */
  dangerous: boolean;
  /** Settable only at startup. */
  immutable: boolean;
  /** Needs `enable-protected-configs`. */
  protected: boolean;
}

export type ConfigRequest = { kind: 'set'; changes: { name: string; value: string }[] } | { kind: 'rewrite' };

export type FlushMode = 'default' | 'async' | 'sync';

export type FunctionRequest =
  | { kind: 'load'; code: string; replace: boolean }
  | { kind: 'delete'; library: string }
  | { kind: 'scriptFlush'; mode: FlushMode };

/** `RedisPlanView`: the commands with secrets masked; the real commands stay on the server. */
export interface RedisPlanView {
  display: string[];
  /** Applying it always needs confirmation. */
  destructive: boolean;
  sensitive: boolean;
  warnings: string[];
}

export interface FunctionInfo {
  name: string;
  description: string | null;
  flags: string[];
}

export interface FunctionLibrary {
  name: string;
  engine: string;
  functions: FunctionInfo[];
  /** Only when listed with code. */
  code: string | null;
}

export interface SlotRange {
  start: number;
  /** Inclusive. */
  end: number;
}

export interface SlotMigration {
  slot: number;
  nodeId: string;
}

export interface ClusterNode {
  id: string;
  /** Empty for `noaddr`. */
  ip: string;
  port: number;
  clusterPort: number | null;
  hostname: string | null;
  shardId: string | null;
  aux: InfoField[];
  endpoint: string | null;
  role: 'primary' | 'replica' | 'unknown';
  myself: boolean;
  failing: boolean;
  flags: string[];
  primaryId: string | null;
  pingSent: number | null;
  pongReceived: number | null;
  configEpoch: number | null;
  linkState: 'connected' | 'disconnected' | null;
  slots: SlotRange[];
  migrating: SlotMigration[];
  importing: SlotMigration[];
  health: string | null;
  replicationOffset: number | null;
}

export interface ClusterState {
  state: string | null;
  slotsAssigned: number | null;
  slotsOk: number | null;
  slotsPfail: number | null;
  slotsFail: number | null;
  knownNodes: number | null;
  size: number | null;
  currentEpoch: number | null;
  myEpoch: number | null;
  other: InfoField[];
}

export interface ClusterTopology {
  source: 'nodes' | 'shards' | 'slots';
  state: ClusterState;
  myId: string | null;
  /** Primaries first (by first slot), each followed by its replicas. */
  nodes: ClusterNode[];
  slotMap: { range: SlotRange; nodeId: string }[];
  unassigned: SlotRange[];
}

export interface SentinelPeer {
  name: string;
  ip: string | null;
  port: number | null;
  runId: string | null;
  flags: string[];
  down: boolean;
  lastOkPingReplyMs: number | null;
  masterLinkStatus: string | null;
  replOffset: number | null;
  fields: InfoField[];
}

export interface SentinelMaster {
  master: SentinelPeer;
  quorum: number | null;
  numReplicas: number | null;
  numOtherSentinels: number | null;
  downAfterMs: number | null;
  failoverTimeoutMs: number | null;
  parallelSyncs: number | null;
  configEpoch: number | null;
  replicas: SentinelPeer[];
  sentinels: SentinelPeer[];
}

export interface RedisTopology {
  cluster: ClusterTopology | null;
  sentinel: { masters: SentinelMaster[] } | null;
}

export function redisConfig(sessionId: string, pattern?: string): Promise<ConfigEntry[]> {
  return call<ConfigEntry[]>('redis_config', { sessionId, pattern: pattern ?? null });
}

export function redisConfigPlan(sessionId: string, request: ConfigRequest): Promise<Planned<RedisPlanView>> {
  return call<Planned<RedisPlanView>>('redis_config_plan', { sessionId, request });
}

export function redisFunctionPlan(sessionId: string, request: FunctionRequest): Promise<Planned<RedisPlanView>> {
  return call<Planned<RedisPlanView>>('redis_function_plan', { sessionId, request });
}

/** Runs a stored config or function plan; one reply per command. */
export function redisPlanApply(sessionId: string, planId: string, confirmed: boolean): Promise<Value[]> {
  return call<Value[]>('redis_plan_apply', { sessionId, planId, confirmed });
}

/** Redis 6.2 rejects: it has no functions. */
export function redisFunctions(sessionId: string, libraryPattern: string | null, withCode: boolean): Promise<FunctionLibrary[]> {
  return call<FunctionLibrary[]>('redis_functions', { sessionId, libraryPattern, withCode });
}

export function redisTopology(sessionId: string): Promise<RedisTopology> {
  return call<RedisTopology>('redis_topology', { sessionId });
}
