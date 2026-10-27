/**
 * Object details, DDL and the engine-specific object editors (M2).
 *
 * Mirrors `crates/catalog/src/model.rs`, `crates/catalog/src/postgres/**`,
 * `crates/catalog/src/mysql/**`, `app/src-tauri/src/m2/{editors,ddl,plans}.rs`.
 * Optional result fields are always present and `null` when empty.
 *
 * Integers: 64-bit values that can pass 2^53 (sequence values and limits,
 * identity options) travel as exact decimal text both ways. Other integers
 * are JSON numbers; editors never send one back that they did not get from
 * the user (see `lib/ddlPlan.ts`).
 */
import { catalogDescribe as describe } from './catalog';
import type { ObjectRef } from './catalog';
import { call } from './invoke';
import type { ExecOutput } from './sessions';
import type { MyDesignerAlterOp, MyDesignerChange, PgDesignerChange } from './grid';
import type { EditPlan, Planned } from './plans';

// ---------------------------------------------------------------------------
// Catalog model (crates/catalog/src/model.rs)
// ---------------------------------------------------------------------------

// `ObjectKind`, `ObjectRef`, `Container` and `ObjectSummary` live in `./catalog` (U1); re-exported for the editors.
export type { Container, ObjectKind, ObjectRef, ObjectSummary } from './catalog';

export type { EditPlan, Planned } from './plans';

// ---------------------------------------------------------------------------
// PostgreSQL details (crates/catalog/src/postgres/{relations,programs}/)
// ---------------------------------------------------------------------------

export interface PgQualifiedName {
  schema: string;
  name: string;
}

export interface PgOption {
  name: string;
  value: string;
}

export interface PgMeta {
  owner: string | null;
  comment: string | null;
  extension: string | null;
  system: boolean;
}

export interface PgColumn {
  schema: string;
  table: string;
  name: string;
  position: number;
  dataType: string;
  notNull: boolean;
  default: string | null;
  comment: string | null;
}

export interface PgIndexKey {
  definition: string;
  expression: boolean;
  descending: boolean;
  nullsFirst: boolean;
}

export interface PgIndex {
  schema: string;
  table: string;
  name: string;
  method: string;
  unique: boolean;
  primary: boolean;
  valid: boolean;
  keys: PgIndexKey[];
  includeColumns: string[];
  predicate: string | null;
  sizeBytes: number | null;
  definition: string;
  comment: string | null;
}

export interface PgTrigger {
  schema: string;
  table: string;
  name: string;
  /** `before`, `after` or `insteadOf`. */
  timing: string;
  /** `insert`, `update`, `delete`, `truncate`. */
  events: string[];
  updateColumns: string[];
  /** `row` or `statement`. */
  level: string;
  condition: string | null;
  /** The trigger function as `regprocedure` text (`public.f()`); its arguments are in `definition`. */
  function: string;
  /** `origin`, `disabled`, `replica` or `always`. */
  enabled: string;
  constraintTrigger: boolean;
  deferrable: boolean;
  deferred: boolean;
  oldTable: string | null;
  newTable: string | null;
  inherited: boolean;
  definition: string;
  comment: string | null;
}

export interface PgPartitionEntry {
  schema: string;
  name: string;
  bound: string;
  isDefault: boolean;
  partitioned: boolean;
  foreign: boolean;
  detachPending: boolean;
}

export interface PgPartitioning {
  strategy: string;
  key: string;
  partitions: PgPartitionEntry[];
  defaultPartition: PgQualifiedName | null;
}

export interface PgPartitionOf {
  parent: PgQualifiedName;
  bound: string;
  isDefault: boolean;
  detachPending: boolean;
}

/** The parts of `PgTable` the object editors read. */
export interface PgTable {
  schema: string;
  name: string;
  owner: string;
  partitioning: PgPartitioning | null;
  partitionOf: PgPartitionOf | null;
  rowEstimate: number | null;
  totalBytes: number | null;
  sizeNote: string | null;
  comment: string | null;
  system: boolean;
  /** Row level security is enabled. */
  rlsEnabled: boolean;
  /** Row level security also applies to the owner (`FORCE`). */
  rlsForced: boolean;
  columns: PgColumn[];
  triggers: PgTrigger[];
}

export interface PgView {
  schema: string;
  name: string;
  owner: string;
  definition: string;
  /** `local` or `cascaded`. */
  checkOption: string | null;
  securityBarrier: boolean;
  securityInvoker: boolean;
  options: PgOption[];
  comment: string | null;
  system: boolean;
  columns: PgColumn[];
  triggers: PgTrigger[];
}

export interface PgMaterializedView {
  schema: string;
  name: string;
  owner: string;
  definition: string;
  populated: boolean;
  accessMethod: string | null;
  tablespace: string | null;
  storageParameters: PgOption[];
  rowEstimate: number | null;
  totalBytes: number | null;
  sizeNote: string | null;
  lastRefreshNote: string;
  /** True when `REFRESH ... CONCURRENTLY` is possible (a usable unique index, populated). */
  refreshConcurrently: boolean;
  comment: string | null;
  system: boolean;
  columns: PgColumn[];
  indexes: PgIndex[];
}

export interface PgOwnedBy {
  schema: string;
  table: string;
  column: string;
}

/** `PgSequence`. 64-bit values arrive as exact decimal text. */
export interface PgSequence {
  schema: string;
  name: string;
  owner: string;
  dataType: string;
  start: string;
  increment: string;
  min: string;
  max: string;
  cache: string;
  cycle: boolean;
  /**
   * `last_value` of the sequence: with `isCalled` the last value handed out, without it the value the
   * next `nextval` returns (new, after RESTART or setval(.., false)); `null` in lists or without privilege.
   */
  lastValue: string | null;
  isCalled: boolean | null;
  unlogged: boolean;
  ownedBy: PgOwnedBy | null;
  identity: string | null;
  comment: string | null;
  system: boolean;
}

export type PgArgumentMode = 'in' | 'out' | 'inOut' | 'variadic' | 'table';
export type PgVolatility = 'immutable' | 'stable' | 'volatile';
export type PgParallel = 'safe' | 'restricted' | 'unsafe';

export interface PgArgument {
  name: string | null;
  mode: PgArgumentMode;
  dataType: string;
  default: string | null;
}

export interface PgFunction {
  schema: string;
  name: string;
  signature: string;
  arguments: PgArgument[];
  result: string | null;
  returnType: string | null;
  returnsSet: boolean;
  window: boolean;
  language: string;
  volatility: PgVolatility;
  strict: boolean;
  securityDefiner: boolean;
  leakproof: boolean;
  parallel: PgParallel;
  cost: number;
  rows: number | null;
  /** `name=value` entries of `proconfig`. */
  config: string[];
  source: string | null;
  library: string | null;
  sqlStandardBody: boolean;
  definition: string;
  meta: PgMeta;
}

export type PgTriggerState = 'enabled' | 'disabled' | 'replica' | 'always';

export interface PgEventTrigger {
  name: string;
  event: string;
  tags: string[];
  function: string;
  state: PgTriggerState;
  meta: PgMeta;
}

export interface PgEnumType {
  schema: string;
  name: string;
  labels: string[];
  meta: PgMeta;
}

export interface PgAttribute {
  name: string;
  dataType: string;
  collation: string | null;
  comment: string | null;
}

export interface PgCompositeType {
  schema: string;
  name: string;
  attributes: PgAttribute[];
  meta: PgMeta;
}

export interface PgDomainConstraint {
  name: string;
  definition: string;
  validated: boolean;
  notNull: boolean;
  comment: string | null;
}

export interface PgDomain {
  schema: string;
  name: string;
  baseType: string;
  collation: string | null;
  default: string | null;
  notNull: boolean;
  constraints: PgDomainConstraint[];
  meta: PgMeta;
}

export interface PgRangeType {
  schema: string;
  name: string;
  subtype: string;
  subtypeOpclass: string;
  collation: string | null;
  canonical: string | null;
  subtypeDiff: string | null;
  multirange: string | null;
  meta: PgMeta;
}

export interface PgExtension {
  name: string;
  schema: string;
  version: string;
  relocatable: boolean;
  defaultVersion: string | null;
  availableUpdates: string[];
  updateAvailable: boolean;
  description: string | null;
  requires: string[];
  configTables: string[];
  members: string[];
  meta: PgMeta;
}

/**
 * An extension the server can install (`catalog_available_extensions`,
 * crates/catalog/src/postgres/programs/extensions.rs `PgAvailableExtension`):
 * the install picker's rows.
 */
export interface PgAvailableExtension {
  name: string;
  /** What a plain `CREATE EXTENSION` installs; `null` when the control file names none. */
  defaultVersion: string | null;
  /** Every installable version, oldest first. */
  versions: string[];
  /** Installed version in the session's database; `null` when not installed there. */
  installedVersion: string | null;
  installedSchema: string | null;
  /** Description from the control file. */
  comment: string | null;
  relocatable: boolean;
  /** The schema the control file fixes; a schema choice is refused then. */
  fixedSchema: string | null;
  /** Only a superuser can install it (unless `trusted`). */
  superuser: boolean;
  trusted: boolean;
  /** Extensions the default version requires; CASCADE installs the missing ones. */
  requires: string[];
  /** Control file data of every installable version, in the order of `versions`. */
  versionDetails: PgExtensionVersion[];
}

/** One installable version of an extension (`pg_available_extension_versions`). */
export interface PgExtensionVersion {
  version: string;
  superuser: boolean;
  trusted: boolean;
  relocatable: boolean;
  fixedSchema: string | null;
  requires: string[];
}

/** A stored setting (`ALTER DATABASE ... SET`), for every role (`role` null) or one role in the database. */
export interface PgSetting {
  role: string | null;
  name: string;
  /** As stored by the server (list parameters keep their element quoting). */
  value: string;
}

export interface PgDatabase {
  name: string;
  owner: string;
  encoding: string;
  /** `LC_COLLATE`. */
  collation: string;
  /** `LC_CTYPE`. */
  ctype: string;
  /** `libc`, `icu` (15+) or `builtin` (17+). */
  localeProvider: string;
  /** ICU locale (15+) or builtin locale (17+). */
  locale: string | null;
  icuRules: string | null;
  collationVersion: string | null;
  isTemplate: boolean;
  allowConnections: boolean;
  /** `-1` for no limit; `-2` marks a database an interrupted drop left invalid (15+). */
  connectionLimit: number;
  tablespace: string;
  sizeBytes: number | null;
  settings: PgSetting[];
  comment: string | null;
  system: boolean;
}

export interface PgSchema {
  name: string;
  owner: string;
  extension: string | null;
  comment: string | null;
  system: boolean;
}

export interface PgRule {
  schema: string;
  /** The table or view. */
  table: string;
  name: string;
  /** `select`, `insert`, `update` or `delete`. */
  event: string;
  instead: boolean;
  /** `origin` (enabled), `disabled`, `replica` or `always`. */
  enabled: string;
  /** `pg_get_ruledef`, without a trailing semicolon. */
  definition: string;
  comment: string | null;
}

export interface PgPolicy {
  schema: string;
  table: string;
  name: string;
  /** `all`, `select`, `insert`, `update` or `delete`. */
  command: string;
  /** False for `RESTRICTIVE`. */
  permissive: boolean;
  /** Role names; `public` for `PUBLIC`. */
  roles: string[];
  using: string | null;
  withCheck: string | null;
  comment: string | null;
}

/** `PgObject`: `PgRelationObject` and `PgProgramObject`, both tagged by `kind`. Kinds the editors do not read are kept opaque. */
export type PgObject =
  | ({ kind: 'table' | 'partitionedTable' | 'partition' | 'foreignTable' } & PgTable)
  | ({ kind: 'view' } & PgView)
  | ({ kind: 'materializedView' } & PgMaterializedView)
  | ({ kind: 'sequence' } & PgSequence)
  | ({ kind: 'trigger' } & PgTrigger)
  | ({ kind: 'function' | 'procedure' } & PgFunction)
  | ({ kind: 'eventTrigger' } & PgEventTrigger)
  | ({ kind: 'enumType' } & PgEnumType)
  | ({ kind: 'compositeType' } & PgCompositeType)
  | ({ kind: 'domain' } & PgDomain)
  | ({ kind: 'rangeType' } & PgRangeType)
  | ({ kind: 'extension' } & PgExtension)
  | ({ kind: 'database' } & PgDatabase)
  | ({ kind: 'schema' } & PgSchema)
  | ({ kind: 'rule' } & PgRule)
  | ({ kind: 'policy' } & PgPolicy)
  | {
      kind:
        | 'column'
        | 'index'
        | 'constraint'
        | 'tablespace'
        | 'aggregate'
        | 'publication'
        | 'subscription'
        | 'foreignDataWrapper'
        | 'foreignServer'
        | 'collation'
        | 'cast'
        | 'operator';
    };

// ---------------------------------------------------------------------------
// MySQL / MariaDB details (crates/catalog/src/mysql/types.rs)
// ---------------------------------------------------------------------------

export type MyDefault = { type: 'null' } | { type: 'literal'; value: string } | { type: 'expression'; value: string };

export interface MyGenerated {
  generatedKind: 'virtual' | 'stored' | 'rowStart' | 'rowEnd';
  expression: string | null;
}

export interface MyColumn {
  table: string;
  name: string;
  position: number;
  dataType: string;
  columnType: string;
  nullable: boolean;
  default: MyDefault | null;
  autoIncrement: boolean;
  generated: MyGenerated | null;
  invisible: boolean;
  comment: string | null;
  key: string | null;
}

export interface MySystemVersioning {
  rowStart: string | null;
  rowEnd: string | null;
}

export interface MyPartition {
  table: string;
  name: string;
  partition: string | null;
  position: number;
  method: string | null;
  expression: string | null;
  description: string | null;
  values: string | null;
  rows: number | null;
  dataLength: number | null;
  indexLength: number | null;
  comment: string | null;
  tablespace: string | null;
  subpartitions: MyPartition[];
}

export interface MyPartitioning {
  method: string;
  linear: boolean;
  expression: string | null;
  subpartitionMethod: string | null;
  subpartitionExpression: string | null;
  partitions: MyPartition[];
}

/** The parts of `MyTable` the object editors read. */
export interface MyTable {
  database: string;
  name: string;
  tableType: string;
  engine: string | null;
  rowEstimate: number | null;
  dataLength: number | null;
  indexLength: number | null;
  comment: string | null;
  systemVersioning: MySystemVersioning | null;
  columns: MyColumn[];
  partitioning: MyPartitioning | null;
  triggers: string[];
  system: boolean;
}

export interface MyView {
  database: string;
  name: string;
  definition: string | null;
  algorithm: string | null;
  security: string | null;
  definer: string | null;
  checkOption: string | null;
  updatable: boolean | null;
  charsetClient: string | null;
  collationConnection: string | null;
  columns: MyColumn[];
  system: boolean;
}

export interface MyParameter {
  position: number;
  mode: string | null;
  name: string | null;
  dataType: string;
}

export interface MyRoutine {
  database: string;
  name: string;
  parameters: MyParameter[];
  returns: string | null;
  deterministic: boolean;
  dataAccess: string | null;
  security: string | null;
  definer: string | null;
  sqlMode: string | null;
  comment: string | null;
  body: string | null;
  created: string | null;
  altered: string | null;
  charsetClient: string | null;
  collationConnection: string | null;
  databaseCollation: string | null;
}

export interface MyTrigger {
  database: string;
  table: string;
  name: string;
  timing: string;
  event: string;
  actionOrder: number;
  follows: string | null;
  precedes: string | null;
  definer: string | null;
  sqlMode: string | null;
  body: string | null;
  created: string | null;
}

export type MySchedule =
  | { type: 'at'; at: string | null }
  | {
      type: 'every';
      intervalValue: string | null;
      intervalField: string | null;
      starts: string | null;
      ends: string | null;
    };

export interface MyEvent {
  database: string;
  name: string;
  definer: string | null;
  timeZone: string | null;
  schedule: MySchedule;
  status: 'enabled' | 'disabled' | 'slavesideDisabled';
  preserve: boolean;
  sqlMode: string | null;
  comment: string | null;
  body: string | null;
  created: string | null;
  altered: string | null;
  lastExecuted: string | null;
  originator: number | null;
  scheduler: 'on' | 'off' | 'disabled';
}

export interface MySequence {
  database: string;
  name: string;
  dataType: string | null;
  /** Exact decimal text (sequence values can exceed JavaScript numbers). */
  start: string;
  increment: string;
  minimum: string;
  maximum: string;
  cache: string;
  cycle: boolean;
  nextValue: string;
  cycleCount: string;
  lastValue: string | null;
  comment: string | null;
  engine: string | null;
}

/** `MyObject`, tagged by `kind`. Kinds the editors do not read are kept opaque. */
export interface MyDatabase {
  name: string;
  charset: string | null;
  collation: string | null;
  /** Default table encryption (MySQL 8.0.16+). */
  encryption: boolean | null;
  /** MySQL 8.0.22+. */
  readOnly: boolean | null;
  /** MariaDB 10.5+. */
  comment: string | null;
  /** Data and index size of its tables (server estimate); null in lists. */
  sizeBytes: number | null;
  tableCount: number | null;
  system: boolean;
}

export type MyObject =
  | ({ kind: 'database' } & MyDatabase)
  | ({ kind: 'table' } & MyTable)
  | ({ kind: 'view' } & MyView)
  | ({ kind: 'procedure' | 'function' } & MyRoutine)
  | ({ kind: 'trigger' } & MyTrigger)
  | ({ kind: 'event' } & MyEvent)
  | ({ kind: 'sequence' } & MySequence)
  | ({ kind: 'partition' } & MyPartition)
  | { kind: 'column' | 'index' | 'constraint' | 'package' | 'packageBody' };

/** `ObjectDetails`. */
export type ObjectDetails = { engine: 'postgres'; object: PgObject } | { engine: 'mysql'; object: MyObject };

// ---------------------------------------------------------------------------
// PostgreSQL changes (crates/catalog/src/postgres/edit/)
// ---------------------------------------------------------------------------

export interface QualifiedName {
  /** `null` resolves through the session's `search_path`. */
  schema: string | null;
  name: string;
}

export type RoleSpec =
  | { kind: 'name'; name: string }
  | { kind: 'public' }
  | { kind: 'currentUser' }
  | { kind: 'sessionUser' }
  | { kind: 'currentRole' };

export type DropBehavior = 'restrict' | 'cascade';

export interface StorageParam {
  name: string;
  value: string;
}

export interface ConfigSetting {
  name: string;
  values: string[];
}

export type FiringState = 'enabled' | 'disabled' | 'replica' | 'always';

export type PgCheckOption = 'local' | 'cascaded';

export interface PgCreateView {
  name: QualifiedName;
  orReplace?: boolean;
  columns?: string[];
  query: string;
  checkOption?: PgCheckOption | null;
  securityBarrier?: boolean | null;
  securityInvoker?: boolean | null;
  comment?: string | null;
}

export type PgViewOperation =
  | { op: 'rename'; newName: string }
  | { op: 'setSchema'; schema: string }
  | { op: 'renameColumn'; name: string; newName: string }
  | { op: 'setColumnDefault'; column: string; expression: string }
  | { op: 'dropColumnDefault'; column: string }
  | { op: 'setOptions'; checkOption?: PgCheckOption | null; securityBarrier?: boolean | null; securityInvoker?: boolean | null }
  | { op: 'resetOptions'; options: ('checkOption' | 'securityBarrier' | 'securityInvoker')[] }
  | { op: 'owner'; owner: RoleSpec };

interface DropFields {
  ifExists?: boolean;
  behavior?: DropBehavior;
}

export type PgViewChange =
  | ({ action: 'create' } & PgCreateView)
  | { action: 'alter'; name: QualifiedName; operations: PgViewOperation[] }
  | ({ action: 'drop'; name: QualifiedName } & DropFields);

export interface PgCreateMaterializedView {
  name: QualifiedName;
  ifNotExists?: boolean;
  columns?: string[];
  query: string;
  withData?: boolean;
  storageParams?: StorageParam[];
  tablespace?: string | null;
  comment?: string | null;
}

export type PgMaterializedViewOperation =
  | { op: 'rename'; newName: string }
  | { op: 'setSchema'; schema: string }
  | { op: 'renameColumn'; name: string; newName: string }
  | { op: 'setTablespace'; tablespace: string }
  | { op: 'setStorageParams'; params: StorageParam[] }
  | { op: 'resetStorageParams'; names: string[] }
  | { op: 'owner'; owner: RoleSpec };

export type PgMaterializedViewChange =
  | ({ action: 'create' } & PgCreateMaterializedView)
  | { action: 'refresh'; name: QualifiedName; concurrently?: boolean; hasUniqueIndex?: boolean; withData?: boolean }
  | { action: 'alter'; name: QualifiedName; operations: PgMaterializedViewOperation[] }
  | ({ action: 'drop'; name: QualifiedName } & DropFields);

export type PgSequenceType = 'smallint' | 'integer' | 'bigint';
/** `SequenceLimit`; the value is exact decimal text. */
export type PgSequenceLimit = { kind: 'default' } | { kind: 'value'; value: string };

/** `SequenceOptions`; 64-bit values travel as exact decimal text. */
export interface PgSequenceOptions {
  dataType?: PgSequenceType | null;
  increment?: string | null;
  minValue?: PgSequenceLimit | null;
  maxValue?: PgSequenceLimit | null;
  start?: string | null;
  cache?: string | null;
  cycle?: boolean | null;
}

export interface PgColumnRef {
  table: QualifiedName;
  column: string;
}

export type PgSequenceOperation =
  | { op: 'options'; options: PgSequenceOptions }
  | { op: 'restart'; with?: string | null }
  | { op: 'setValue'; value: string; isCalled?: boolean }
  | { op: 'ownedBy'; column?: PgColumnRef | null }
  | { op: 'rename'; newName: string }
  | { op: 'setSchema'; schema: string }
  | { op: 'logged'; logged: boolean }
  | { op: 'owner'; owner: RoleSpec };

export type PgSequenceChange =
  | {
      action: 'create';
      name: QualifiedName;
      ifNotExists?: boolean;
      unlogged?: boolean;
      options?: PgSequenceOptions;
      ownedBy?: PgColumnRef | null;
      comment?: string | null;
    }
  | { action: 'alter'; name: QualifiedName; operations: PgSequenceOperation[] }
  | ({ action: 'drop'; name: QualifiedName } & DropFields);

export type PgRoutineKind = 'function' | 'procedure';
export type PgArgMode = 'in' | 'out' | 'inOut' | 'variadic';

export interface PgRoutineArg {
  mode?: PgArgMode;
  name?: string | null;
  dataType: string;
  default?: string | null;
}

export type PgReturns =
  | { kind: 'type'; dataType: string; setof?: boolean }
  | { kind: 'table'; columns: { name: string; dataType: string }[] };

export type PgRoutineBody =
  | { kind: 'definition'; source: string }
  | { kind: 'object'; file: string; symbol: string }
  | { kind: 'return'; expression: string }
  | { kind: 'atomic'; statements: string[] };

export interface PgCreateRoutine {
  routine: PgRoutineKind;
  name: QualifiedName;
  orReplace?: boolean;
  arguments?: PgRoutineArg[];
  returns?: PgReturns | null;
  language: string;
  body: PgRoutineBody;
  volatility?: PgVolatility | null;
  strict?: boolean | null;
  securityDefiner?: boolean | null;
  leakproof?: boolean | null;
  parallel?: PgParallel | null;
  cost?: number | null;
  rows?: number | null;
  window?: boolean;
  settings?: ConfigSetting[];
  comment?: string | null;
}

export type PgRoutineOperation =
  | { op: 'rename'; newName: string }
  | { op: 'setSchema'; schema: string }
  | { op: 'owner'; owner: RoleSpec }
  | { op: 'security'; definer: boolean }
  | { op: 'volatility'; volatility: PgVolatility }
  | { op: 'strict'; strict: boolean }
  | { op: 'leakproof'; leakproof: boolean }
  | { op: 'parallel'; parallel: PgParallel }
  | { op: 'cost'; cost: number }
  | { op: 'rows'; rows: number }
  | { op: 'set'; setting: ConfigSetting }
  | { op: 'reset'; name: string };

export type PgRoutineChange =
  | ({ action: 'create' } & PgCreateRoutine)
  | { action: 'alter'; routine: PgRoutineKind; name: QualifiedName; signature: string; operations: PgRoutineOperation[] }
  | ({ action: 'drop'; routine: PgRoutineKind; name: QualifiedName; signature: string } & DropFields);

export type PgTriggerTiming = 'before' | 'after' | 'insteadOf';
export type PgTriggerEvent = { kind: 'insert' } | { kind: 'update'; columns?: string[] } | { kind: 'delete' } | { kind: 'truncate' };

export interface PgCreateTrigger {
  name: string;
  table: QualifiedName;
  orReplace?: boolean;
  constraint?: boolean;
  timing: PgTriggerTiming;
  events: PgTriggerEvent[];
  level: 'row' | 'statement';
  oldTable?: string | null;
  newTable?: string | null;
  condition?: string | null;
  function: QualifiedName;
  arguments?: string[];
  deferrable?: boolean;
  initiallyDeferred?: boolean;
  comment?: string | null;
}

export type PgTriggerChange =
  | ({ action: 'create' } & PgCreateTrigger)
  | { action: 'setState'; name: string; table: QualifiedName; state: FiringState }
  | { action: 'rename'; name: string; table: QualifiedName; newName: string }
  | ({ action: 'drop'; name: string; table: QualifiedName } & DropFields);

export type PgEventTriggerEvent = 'ddlCommandStart' | 'ddlCommandEnd' | 'tableRewrite' | 'sqlDrop' | 'login';

export type PgEventTriggerChange =
  | { action: 'create'; name: string; event: PgEventTriggerEvent; tags?: string[]; function: QualifiedName; comment?: string | null }
  | { action: 'setState'; name: string; state: FiringState }
  | { action: 'rename'; name: string; newName: string }
  | { action: 'owner'; name: string; owner: RoleSpec }
  | ({ action: 'drop'; name: string } & DropFields);

export interface PgCompositeAttribute {
  name: string;
  dataType: string;
  collation?: string | null;
}

export type PgCompositeOperation =
  | { op: 'addAttribute'; attribute: PgCompositeAttribute; cascade?: boolean }
  | { op: 'dropAttribute'; name: string; ifExists?: boolean; cascade?: boolean }
  | { op: 'renameAttribute'; name: string; newName: string; cascade?: boolean }
  | { op: 'alterAttributeType'; name: string; dataType: string; collation?: string | null; cascade?: boolean };

export type PgEnumPosition = { kind: 'before'; label: string } | { kind: 'after'; label: string };

export interface PgCreateRangeType {
  name: QualifiedName;
  subtype: string;
  subtypeOpclass?: QualifiedName | null;
  collation?: string | null;
  canonical?: QualifiedName | null;
  subtypeDiff?: QualifiedName | null;
  multirangeTypeName?: QualifiedName | null;
  comment?: string | null;
}

export type PgTypeChange =
  | { action: 'createEnum'; name: QualifiedName; labels: string[]; comment?: string | null }
  | { action: 'addEnumValue'; name: QualifiedName; value: string; ifNotExists?: boolean; position?: PgEnumPosition | null }
  | { action: 'renameEnumValue'; name: QualifiedName; value: string; newValue: string }
  | { action: 'createComposite'; name: QualifiedName; attributes: PgCompositeAttribute[]; comment?: string | null }
  | { action: 'alterComposite'; name: QualifiedName; operations: PgCompositeOperation[] }
  | ({ action: 'createRange' } & PgCreateRangeType)
  | { action: 'rename'; name: QualifiedName; newName: string }
  | { action: 'setSchema'; name: QualifiedName; schema: string }
  | { action: 'owner'; name: QualifiedName; owner: RoleSpec }
  | ({ action: 'drop'; name: QualifiedName } & DropFields);

export interface PgDomainCheck {
  name?: string | null;
  expression: string;
}

export type PgDomainOperation =
  | { op: 'setDefault'; expression: string }
  | { op: 'dropDefault' }
  | { op: 'setNotNull' }
  | { op: 'dropNotNull' }
  | { op: 'addCheck'; check: PgDomainCheck; notValid?: boolean }
  | ({ op: 'dropConstraint'; name: string } & DropFields)
  | { op: 'renameConstraint'; name: string; newName: string }
  | { op: 'validateConstraint'; name: string }
  /** `COMMENT ON CONSTRAINT ... ON DOMAIN`; `null` removes the comment. */
  | { op: 'commentConstraint'; name: string; comment: string | null }
  | { op: 'rename'; newName: string }
  | { op: 'setSchema'; schema: string }
  | { op: 'owner'; owner: RoleSpec };

export type PgDomainChange =
  | {
      action: 'create';
      name: QualifiedName;
      dataType: string;
      collation?: string | null;
      default?: string | null;
      notNull?: boolean;
      checks?: PgDomainCheck[];
      comment?: string | null;
    }
  | { action: 'alter'; name: QualifiedName; operations: PgDomainOperation[] }
  | ({ action: 'drop'; name: QualifiedName } & DropFields);

export type PgExtensionChange =
  | { action: 'create'; name: string; ifNotExists?: boolean; schema?: string | null; version?: string | null; cascade?: boolean; comment?: string | null }
  | { action: 'update'; name: string; version?: string | null }
  | { action: 'setSchema'; name: string; schema: string }
  | ({ action: 'drop'; name: string } & DropFields);

/** `LocaleProvider` (crates/catalog/src/postgres/edit/database.rs). */
export type PgLocaleProvider = 'libc' | 'icu' | 'builtin';

/** `CreateDatabase`. Owner is a role name only (`CREATE DATABASE` takes no `CURRENT_USER`). */
export interface PgCreateDatabase {
  name: string;
  owner?: RoleSpec | null;
  template?: string | null;
  encoding?: string | null;
  /** 15+. */
  strategy?: 'walLog' | 'fileCopy' | null;
  locale?: string | null;
  lcCollate?: string | null;
  lcCtype?: string | null;
  /** 15+; `builtin` 17+. */
  localeProvider?: PgLocaleProvider | null;
  /** 15+. */
  icuLocale?: string | null;
  /** 16+. */
  icuRules?: string | null;
  /** 17+. */
  builtinLocale?: string | null;
  tablespace?: string | null;
  allowConnections?: boolean | null;
  /** `-1` is unlimited. */
  connectionLimit?: number | null;
  isTemplate?: boolean | null;
  comment?: string | null;
}

/** `DatabaseOperation`, applied in order. */
export type PgDatabaseOperation =
  | { op: 'rename'; newName: string }
  | { op: 'owner'; owner: RoleSpec }
  | { op: 'connectionLimit'; limit: number }
  | { op: 'allowConnections'; allow: boolean }
  | { op: 'isTemplate'; isTemplate: boolean }
  | { op: 'set'; setting: ConfigSetting }
  | { op: 'reset'; name: string }
  | { op: 'resetAll' }
  /** Must be the only operation of its change (cannot run in a transaction). */
  | { op: 'tablespace'; tablespace: string };

export type PgDatabaseChange =
  | ({ action: 'create' } & PgCreateDatabase)
  | { action: 'alter'; name: string; operations: PgDatabaseOperation[] }
  /** `force` (13+) terminates every other session connected to the database first. */
  | { action: 'drop'; name: string; ifExists?: boolean; force?: boolean };

export type PgSchemaOperation = { op: 'rename'; newName: string } | { op: 'owner'; owner: RoleSpec };

export type PgSchemaChange =
  | { action: 'create'; name: string; owner?: RoleSpec | null; ifNotExists?: boolean; comment?: string | null }
  | { action: 'alter'; name: string; operations: PgSchemaOperation[] }
  | ({ action: 'drop'; name: string } & DropFields);

export type PgRuleEvent = 'select' | 'insert' | 'update' | 'delete';

/** `CreateRule`: `actions` empty means `DO ... NOTHING`; each action is one statement without `;`. */
export interface PgCreateRule {
  name: string;
  table: QualifiedName;
  orReplace?: boolean;
  event: PgRuleEvent;
  condition?: string | null;
  instead?: boolean;
  actions?: string[];
  comment?: string | null;
}

export type PgRuleChange =
  | ({ action: 'create' } & PgCreateRule)
  | { action: 'rename'; name: string; table: QualifiedName; newName: string }
  | { action: 'setState'; name: string; table: QualifiedName; state: FiringState }
  | ({ action: 'drop'; name: string; table: QualifiedName } & DropFields);

export type PgPolicyCommand = 'all' | 'select' | 'insert' | 'update' | 'delete';

/** `CreatePolicy`: `roles` empty means `PUBLIC`. */
export interface PgCreatePolicy {
  name: string;
  table: QualifiedName;
  restrictive?: boolean;
  command?: PgPolicyCommand;
  roles?: RoleSpec[];
  using?: string | null;
  withCheck?: string | null;
  comment?: string | null;
}

export type PgPolicyChange =
  | ({ action: 'create' } & PgCreatePolicy)
  | {
      action: 'alter';
      name: string;
      table: QualifiedName;
      roles?: RoleSpec[] | null;
      using?: string | null;
      withCheck?: string | null;
      /** Applied after the other changes. */
      newName?: string | null;
    }
  | { action: 'drop'; name: string; table: QualifiedName; ifExists?: boolean }
  /** Drop and create in one transaction (a failing create leaves the old policy): command or permissiveness changes, or an expression is removed. */
  | { action: 'replace'; name: string; policy: PgCreatePolicy };

/** Row level security switches of a table (`TableOperation` in table.rs). */
export type PgRlsOperation = { op: 'rowLevelSecurity'; enabled: boolean } | { op: 'forceRowLevelSecurity'; forced: boolean };

/** `PgChange`, tagged by `object`; the inner change is flattened next to it. */
export type PgChange =
  | ({ object: 'view' } & PgViewChange)
  | ({ object: 'materializedView' } & PgMaterializedViewChange)
  | ({ object: 'sequence' } & PgSequenceChange)
  | ({ object: 'routine' } & PgRoutineChange)
  | ({ object: 'trigger' } & PgTriggerChange)
  | ({ object: 'eventTrigger' } & PgEventTriggerChange)
  | ({ object: 'type' } & PgTypeChange)
  | ({ object: 'domain' } & PgDomainChange)
  | ({ object: 'extension' } & PgExtensionChange)
  | ({ object: 'database' } & PgDatabaseChange)
  | ({ object: 'schema' } & PgSchemaChange)
  | ({ object: 'rule' } & PgRuleChange)
  | ({ object: 'policy' } & PgPolicyChange)
  /** Row level security on a table (the policy editor); other table changes come from the table designer. */
  | { object: 'table'; action: 'alter'; name: QualifiedName; operations: PgRlsOperation[] }
  | { object: 'comment'; target: ObjectRef; comment?: string | null }
  | { object: 'owner'; target: ObjectRef; owner: RoleSpec }
  /** Tables and indexes, as the table designer builds them (api/grid.ts). */
  | PgDesignerChange;

// ---------------------------------------------------------------------------
// MySQL / MariaDB changes (crates/catalog/src/mysql/edit/)
// ---------------------------------------------------------------------------

export type MyDefiner = { kind: 'currentUser' } | { kind: 'account'; user: string; host: string };
export type MySqlSecurity = 'definer' | 'invoker';

/** `TypeName` in mysql/edit/types.rs. */
export type MyTypeName =
  | 'tinyint'
  | 'smallint'
  | 'mediumint'
  | 'int'
  | 'bigint'
  | 'decimal'
  | 'float'
  | 'double'
  | 'bit'
  | 'boolean'
  | 'char'
  | 'varchar'
  | 'binary'
  | 'varbinary'
  | 'tinytext'
  | 'text'
  | 'mediumtext'
  | 'longtext'
  | 'tinyblob'
  | 'blob'
  | 'mediumblob'
  | 'longblob'
  | 'enum'
  | 'set'
  | 'date'
  | 'time'
  | 'datetime'
  | 'timestamp'
  | 'year'
  | 'json'
  | 'geometry'
  | 'point'
  | 'linestring'
  | 'polygon'
  | 'multipoint'
  | 'multilinestring'
  | 'multipolygon'
  | 'geometrycollection'
  | 'inet4'
  | 'inet6'
  | 'uuid'
  | 'vector';

export interface MyDataType {
  name: MyTypeName;
  length?: number | null;
  scale?: number | null;
  values?: string[];
  unsigned?: boolean;
  zerofill?: boolean;
  charset?: string | null;
  collation?: string | null;
}

export interface MyCreateView {
  database: string;
  name: string;
  replace?: boolean;
  algorithm?: 'undefined' | 'merge' | 'temptable' | null;
  definer?: MyDefiner | null;
  security?: MySqlSecurity | null;
  columns?: string[];
  query: string;
  checkOption?: 'cascaded' | 'local' | null;
}

export interface MyRoutineParameter {
  mode?: 'in' | 'out' | 'inout' | null;
  name: string;
  dataType: MyDataType;
}

export type MyDataAccess = 'containsSql' | 'noSql' | 'readsSqlData' | 'modifiesSqlData';

export interface MyCreateRoutine {
  database: string;
  name: string;
  routine: 'procedure' | 'function';
  replace?: boolean;
  definer?: MyDefiner | null;
  parameters?: MyRoutineParameter[];
  returns?: MyDataType | null;
  characteristics?: {
    comment?: string | null;
    deterministic?: boolean | null;
    dataAccess?: MyDataAccess | null;
    security?: MySqlSecurity | null;
  };
  body: string;
}

/** `ALTER PROCEDURE|FUNCTION`: changes these characteristics in place, keeping the routine's grants. Fields left out are not changed. */
export interface MyAlterRoutine {
  database: string;
  name: string;
  routine: 'procedure' | 'function';
  /** An empty string removes the comment. */
  comment?: string | null;
  dataAccess?: MyDataAccess | null;
  security?: MySqlSecurity | null;
}

export interface MyCreateTrigger {
  database: string;
  table: string;
  name: string;
  replace?: boolean;
  definer?: MyDefiner | null;
  timing: 'before' | 'after';
  events: ('insert' | 'update' | 'delete')[];
  order?: { kind: 'follows' | 'precedes'; trigger: string } | null;
  body: string;
}

export type MyIntervalUnit =
  | 'year'
  | 'quarter'
  | 'month'
  | 'week'
  | 'day'
  | 'hour'
  | 'minute'
  | 'second'
  | 'yearMonth'
  | 'dayHour'
  | 'dayMinute'
  | 'daySecond'
  | 'hourMinute'
  | 'hourSecond'
  | 'minuteSecond';

export interface MyInterval {
  quantity: string;
  unit: MyIntervalUnit;
}

export type MyTimeBase = { kind: 'currentTimestamp' } | { kind: 'timestamp'; value: string };

export interface MyEventTime {
  base: MyTimeBase;
  plus?: MyInterval[];
}

export type MyEventSchedule =
  | { kind: 'at'; at: MyEventTime }
  | { kind: 'every'; interval: MyInterval; starts?: MyEventTime | null; ends?: MyEventTime | null };

export type MyEventStatus = 'enable' | 'disable' | 'disableOnReplica';

export interface MyCreateEvent {
  database: string;
  name: string;
  ifNotExists?: boolean;
  definer?: MyDefiner | null;
  schedule: MyEventSchedule;
  onCompletionPreserve?: boolean | null;
  status?: MyEventStatus | null;
  comment?: string | null;
  body: string;
}

export interface MyAlterEvent {
  database: string;
  name: string;
  definer?: MyDefiner | null;
  schedule?: MyEventSchedule | null;
  onCompletionPreserve?: boolean | null;
  renameTo?: string | null;
  status?: MyEventStatus | null;
  comment?: string | null;
  body?: string | null;
}

export interface MySequenceType {
  name: 'tinyint' | 'smallint' | 'mediumint' | 'int' | 'bigint';
  unsigned?: boolean;
}

/** `SequenceLimit` (MariaDB); `SequenceNumber` values travel as exact decimal text. */
export type MySequenceLimit = { kind: 'unbounded' } | { kind: 'value'; value: string };
export type MySequenceRestart = { kind: 'start' } | { kind: 'with'; value: string };

export interface MySequenceFields {
  dataType?: MySequenceType | null;
  start?: string | null;
  increment?: string | null;
  minValue?: MySequenceLimit | null;
  maxValue?: MySequenceLimit | null;
  cache?: string | null;
  cycle?: boolean | null;
}

/** A row start or end column for `addSystemVersioning` (a `ColumnDef` with `rowPeriod`). */
export interface MyPeriodColumn {
  name: string;
  dataType: MyDataType;
  nullable?: boolean;
  invisible?: boolean;
  rowPeriod: 'start' | 'end';
}

/** The `AlterOp`s the object editors use (system versioning, partition maintenance). */
export type MyAlterOp =
  | { op: 'addSystemVersioning'; columns?: [MyPeriodColumn, MyPeriodColumn] | null }
  | { op: 'dropSystemVersioning'; columns?: { start: string; end: string } | null }
  | { op: 'removePartitioning' }
  | { op: 'addPartitionCount'; count: number }
  | { op: 'dropPartitions'; names: string[] }
  | { op: 'truncatePartitions'; names?: string[] }
  | { op: 'coalescePartitions'; count: number };

/** `MyChange`, tagged by `change` (the ones the object editors build). */
/** `CreateDatabase` / `AlterDatabase` (crates/catalog/src/mysql/edit/database.rs). Fields left out are not changed. */
export interface MyDatabaseOptions {
  charset?: string | null;
  collation?: string | null;
  /** MySQL 8.0.16+. */
  encryption?: boolean | null;
  /** MariaDB 10.5+. */
  comment?: string | null;
}

export type MyChange =
  | ({ change: 'createDatabase'; name: string; ifNotExists?: boolean } & MyDatabaseOptions)
  /** `readOnly`: MySQL 8.0.22+. */
  | ({ change: 'alterDatabase'; name: string; readOnly?: boolean | null } & MyDatabaseOptions)
  | { change: 'dropDatabase'; name: string; ifExists?: boolean }
  | { change: 'dropTable'; database: string; name: string; ifExists?: boolean }
  | ({ change: 'createView' } & MyCreateView)
  | { change: 'dropView'; database: string; name: string; ifExists?: boolean }
  | ({ change: 'createRoutine' } & MyCreateRoutine)
  | { change: 'dropRoutine'; database: string; name: string; routine: 'procedure' | 'function'; ifExists?: boolean }
  | ({ change: 'alterRoutine' } & MyAlterRoutine)
  | ({ change: 'createTrigger' } & MyCreateTrigger)
  | { change: 'dropTrigger'; database: string; name: string; ifExists?: boolean }
  | ({ change: 'createEvent' } & MyCreateEvent)
  | ({ change: 'alterEvent' } & MyAlterEvent)
  | { change: 'dropEvent'; database: string; name: string; ifExists?: boolean }
  | ({ change: 'createSequence'; database: string; name: string; replace?: boolean; ifNotExists?: boolean; comment?: string | null } & MySequenceFields)
  | ({ change: 'alterSequence'; database: string; name: string; restart?: MySequenceRestart | null } & MySequenceFields)
  | { change: 'dropSequence'; database: string; name: string; ifExists?: boolean }
  | { change: 'setSequenceValue'; database: string; name: string; value: string; isUsed?: boolean; round?: string | null }
  | {
      change: 'alterTable';
      database: string;
      name: string;
      /** Editor operations (versioning, partition maintenance) and the table designer's. */
      operations: (MyAlterOp | MyDesignerAlterOp)[];
      algorithm?: 'default' | 'instant' | 'inplace' | 'nocopy' | 'copy' | null;
      lock?: 'default' | 'none' | 'shared' | 'exclusive' | null;
      systemVersioned?: boolean;
    }
  /** `createTable`, as the table designer builds it (api/grid.ts). */
  | MyDesignerChange;

/** `EngineChange` in m2/editors.rs (MySQL and MariaDB both use `mysql`). */
export type EngineChange = { engine: 'postgres'; change: PgChange } | { engine: 'mysql'; change: MyChange };

/** `EditApplied` in m2/editors.rs. `statements` is the number of statements that ran. */
export interface EditApplied {
  statements: number;
  transactional: boolean;
  outputs: ExecOutput[];
  durationMs: number;
}

/** `DdlSession` in crates/catalog/src/mysql/session.rs: the `-- varve-session:` header. */
export interface DdlSession {
  characterSetClient: string | null;
  characterSetResults: string | null;
  collationConnection: string | null;
  sqlMode: string | null;
  timeZone: string | null;
  collationDatabase: string | null;
}

/** `DdlApplied` in m2/ddl.rs. */
export interface DdlApplied {
  output: ExecOutput;
  session: DdlSession | null;
  collationDatabase: string | null;
  warnings: string[];
  durationMs: number;
}

// ---------------------------------------------------------------------------
// Drop impact (app/src-tauri/src/m2/impact.rs)
// ---------------------------------------------------------------------------

/**
 * `blocks`: a plain drop fails while it exists (PostgreSQL CASCADE drops it too); `breaks`: it stays but stops
 * working; `mayBreak`: its body names the object (MySQL and MariaDB routines, triggers, events; found by name).
 */
export type DependentEffect = 'blocks' | 'breaks' | 'mayBreak';

/** One of this app's other sessions (tabs) of the same connection on a database being dropped. */
export interface AppSession {
  sessionId: string;
  /** A statement is running on it now. */
  busy: boolean;
  /** `null` when unknown (busy, or the driver cannot tell). */
  openTransaction: boolean | null;
  /** Known to be on the same server; `false`: it only may be (its server could not be identified, the endpoint matches). */
  certain: boolean;
  /** On a PostgreSQL streaming standby: ended when the drop replays there, with or without FORCE; never blocks the drop. */
  standby: boolean;
}

export interface Dependent {
  /** As the server describes it, for example `view public.order_totals`. */
  description: string;
  effect: DependentEffect;
}

export interface ContentCount {
  /** Plural label (`tables`, `triggers`). */
  label: string;
  /** Decimal text. */
  count: string;
}

export interface ActiveSession {
  /** Backend pid or connection id, as decimal text. */
  id: string;
  user: string | null;
  application: string | null;
  client: string | null;
  state: string | null;
  /** The application name says Varve (this app or another Varve). */
  varve: boolean;
}

/** `DropImpact`: what dropping an object affects. */
export interface DropImpact {
  dependents: Dependent[];
  dependentsTruncated: boolean;
  contents: ContentCount[];
  /** Other sessions connected to a database being dropped. */
  sessions: ActiveSession[];
  /** False when the account may not see every session. */
  sessionsComplete: boolean;
  /** Why the server refuses the drop whatever the sessions do. */
  blockers: string[];
  /** This app's other tabs on the database (database drops), of this connection or another one to the same server. */
  appSessions: AppSession[];
  /** What the lookup could not see (MySQL and MariaDB routine bodies the account may not read). */
  notes: string[];
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/** Extensions the PostgreSQL server can install, with versions and the installed state in the session's database. Reads only. */
export function catalogAvailableExtensions(sessionId: string): Promise<PgAvailableExtension[]> {
  return call<PgAvailableExtension[]>('catalog_available_extensions', { sessionId });
}

/** What dropping `object` affects (dependents, contents, sessions on a database). Reads only. */
export function catalogDropImpact(sessionId: string, object: ObjectRef): Promise<DropImpact> {
  return call<DropImpact>('catalog_drop_impact', { sessionId, object });
}

export function catalogDescribe(sessionId: string, object: ObjectRef): Promise<ObjectDetails> {
  return describe<PgObject, MyObject>(sessionId, object);
}

/** Plans a change; the server keeps it under the returned `planId`. Nothing runs. */
export function editPlan(sessionId: string, change: EngineChange): Promise<Planned<EditPlan>> {
  return call<Planned<EditPlan>>('edit_plan', { sessionId, change });
}

/** Runs a stored plan exactly as planned. */
export function editApply(sessionId: string, planId: string, confirmed: boolean): Promise<EditApplied> {
  return call<EditApplied>('edit_apply', { sessionId, planId, confirmed });
}

/** Runs DDL text on the user's session; `database` is needed for a `collation_database` header. */
export function ddlApply(sessionId: string, ddl: string, database: string | null, confirmed: boolean): Promise<DdlApplied> {
  return call<DdlApplied>('ddl_apply', { sessionId, ddl, database, confirmed });
}
