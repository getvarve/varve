/**
 * Backend calls and wire types for the table designer and grid editing
 * (M2-UI-API.md "Grid editing", "Object editors", "Object browser").
 *
 * Every type mirrors its Rust source exactly (camelCase JSON); the source
 * is named on each type. Optional result fields arrive as `null`.
 */
import type { ObjectKind, ObjectRef } from './catalog';
import type { DriverError } from './errors';
import { call } from './invoke';
import type { ExecOutput, Value } from './sessions';
import { execute } from './sessions';
import type { Planned } from './plans';

// ---------------------------------------------------------------------------
// Server-held plans (app/src-tauri/src/m2/plans.rs)
// ---------------------------------------------------------------------------

export type { EditPlan, Planned } from './plans';

// ---------------------------------------------------------------------------
// Grid editing (crates/catalog/src/data_edit/model.rs)
// ---------------------------------------------------------------------------

/** `TableTarget`: `schema` is required for PostgreSQL and `null` for MySQL / MariaDB. */
export interface TableTarget {
  database: string;
  schema: string | null;
  table: string;
}

/** `RowKey`: `[column, value]` for every key column; values are never NULL. */
export interface RowKey {
  columns: [string, Value][];
}

/** `CellValue`: what a grid cell holds for an insert or update. */
export type CellValue = { kind: 'value'; value: Value } | { kind: 'default' } | { kind: 'expression'; value: string };

/** `RowChange`, tagged by `op`. */
export type RowChange =
  | { op: 'insert'; values: [string, CellValue][] }
  | { op: 'update'; key: RowKey; original: [string, Value][]; set: [string, CellValue][] }
  | { op: 'delete'; key: RowKey; original: [string, Value][] };

/** `KeyKind`. */
export type KeyKind = 'primary' | 'unique';

/** `GeneratedKind`. */
export type GeneratedKind = 'stored' | 'virtual';

/** `IdentityKind`. */
export type IdentityKind = 'always' | 'byDefault';

/** `ColumnInfo`: a column of the edited table. */
export interface ColumnInfo {
  name: string;
  /** Type for display (`character varying(20)`, `int unsigned`). */
  dataType: string;
  /** PostgreSQL: schema of the column's type; `null` for MySQL / MariaDB. */
  typeSchema: string | null;
  /** PostgreSQL type name (`int4`, `_text`), MySQL / MariaDB `DATA_TYPE` in lower case. */
  typeName: string;
  /** PostgreSQL domain: schema and name of the base type. */
  baseType: [string, string] | null;
  /** PostgreSQL: the type (or the base type of its domain) is an array. */
  array: boolean;
  /** PostgreSQL: an array of a domain over an array; never editable. */
  arrayOfArrays: boolean;
  nullable: boolean;
  /** The server fills the column when it is left out. */
  hasDefault: boolean;
  generated: GeneratedKind | null;
  identity: IdentityKind | null;
  autoIncrement: boolean;
  /** False for generated columns and `GENERATED ALWAYS` identity columns. */
  editable: boolean;
}

/** `KeyCollation`. */
export interface KeyCollation {
  schema: string;
  name: string;
}

/** `QualifiedOperator`. */
export interface QualifiedOperator {
  schema: string;
  name: string;
}

/**
 * `KeyInfo`: the key that makes a table editable. Pass it back in
 * `PlanOptions.key` exactly as `editable_key` returned it: the
 * `equalityOperators` and `keyCollations` decide how rows are found.
 */
export interface KeyInfo {
  kind: KeyKind;
  name: string;
  columns: string[];
  tableColumns: ColumnInfo[];
  equalityOperators: QualifiedOperator[];
  keyCollations: (KeyCollation | null)[];
}

/** `PlanOptions`. */
export interface PlanOptions {
  /** Optimistic concurrency: updates and deletes also compare the original values. */
  checkOriginal: boolean;
  key: KeyInfo;
}

/** `ChangeOp`. */
export type ChangeOp = 'insert' | 'update' | 'delete';

/** `PlannedChange`: the SQL for one `RowChange`. */
export interface PlannedChange {
  /** Index of the change in the list given to `row_edit_plan`. */
  change: number;
  op: ChangeOp;
  /** MySQL / MariaDB updates: the `SELECT ... FOR UPDATE` that locks the row. */
  lock: string | null;
  statement: string;
  returnsRow: boolean;
  /** MySQL / MariaDB: the `SELECT` that reads the written row back. */
  refresh: string | null;
  checksOriginal: boolean;
}

/** `RowEditPlan`. */
export interface RowEditPlan {
  engine: 'postgres' | 'mysql' | 'mariadb' | 'redis';
  changes: PlannedChange[];
  warnings: string[];
  hasExpressions: boolean;
}

/** `ConflictReason`. */
export type ConflictReason = 'rowChanged' | 'keyNotUnique';

/** `AppliedChange`: a written row as the server stored it (`null` for deletes and rows that cannot be read back). */
export interface AppliedChange {
  change: number;
  op: ChangeOp;
  row: [string, Value][] | null;
}

/** `ApplyOutcome`, tagged by `status`. Only `applied` changed anything. */
export type ApplyOutcome =
  | { status: 'applied'; rows: AppliedChange[] }
  | { status: 'conflict'; change: number; reason: ConflictReason; rowsMatched: number; message: string }
  /** `error` is the driver's `DriverError` (api/errors.ts), including `partial`. */
  | { status: 'failed'; change: number; error: DriverError };

/** The key that makes `target` editable, or `null` when the grid must stay read only. */
export function editableKey(sessionId: string, target: TableTarget): Promise<KeyInfo | null> {
  return call<KeyInfo | null>('editable_key', { sessionId, target });
}

/** Plans the SQL for grid changes; the server keeps the plan under `planId`. */
export function rowEditPlan(
  sessionId: string,
  target: TableTarget,
  changes: RowChange[],
  options: PlanOptions,
): Promise<Planned<RowEditPlan>> {
  return call<Planned<RowEditPlan>>('row_edit_plan', { sessionId, target, changes, options });
}

/** Runs a stored grid plan in one transaction on the user's session. */
export function rowEditApply(sessionId: string, planId: string, confirmed: boolean): Promise<ApplyOutcome> {
  return call<ApplyOutcome>('row_edit_apply', { sessionId, planId, confirmed });
}

/** Runs a read-only statement the grid built (SELECT of a page, COUNT) on the user's session. */
export function runGridQuery(sessionId: string, sql: string, maxRows: number): Promise<ExecOutput> {
  return execute(sessionId, sql, false, maxRows);
}

// ---------------------------------------------------------------------------
// Object editors (crates/catalog/src/model.rs EditPlan, app/src-tauri/src/m2/editors.rs)
// ---------------------------------------------------------------------------

// One definition of the change unions and the edit calls (api/ddl.ts); the designer's variants are below.
export type { EditApplied, EngineChange, MyChange, PgChange } from './ddl';
export { editApply, editPlan } from './ddl';

// --- PostgreSQL changes (crates/catalog/src/postgres/edit/{common,table,index,sequence,object}.rs) ---

/** `QualifiedName`. */
export interface PgName {
  schema?: string | null;
  name: string;
}

/** `StorageParam`. */
export interface PgStorageParam {
  name: string;
  value: string;
}

/** `DropBehavior`. */
export type PgDropBehavior = 'restrict' | 'cascade';

/** `IdentityGeneration`. */
export type PgIdentityGeneration = 'always' | 'byDefault';

/** `SequenceOptions` (only the fields the designer sets). 64-bit values travel as decimal text. */
export interface PgSequenceOptions {
  increment?: string | null;
  start?: string | null;
  cache?: string | null;
  cycle?: boolean | null;
}

/** `Identity`. */
export interface PgIdentity {
  generation: PgIdentityGeneration;
  options?: PgSequenceOptions;
}

/** `Generated`. */
export interface PgGenerated {
  expression: string;
  storage?: 'stored' | 'virtual';
}

/** `ColumnDef`. */
export interface PgColumnDef {
  name: string;
  dataType: string;
  notNull?: boolean;
  default?: string | null;
  identity?: PgIdentity | null;
  generated?: PgGenerated | null;
  collation?: string | null;
  comment?: string | null;
}

/** `IndexMethod`, tagged by `kind`. */
export type PgIndexMethod =
  | { kind: 'btree' | 'hash' | 'gist' | 'spgist' | 'gin' | 'brin' }
  | { kind: 'custom'; name: string };

/** `IndexKey` (externally tagged). */
export type PgIndexKey = { column: string } | { expression: string };

/** `IndexElement`. */
export interface PgIndexElement {
  key: PgIndexKey;
  collation?: PgName | null;
  opclass?: PgName | null;
  /** Operator class parameters (13+); need `opclass`. */
  opclassOptions?: PgStorageParam[];
  order?: 'asc' | 'desc' | null;
  nulls?: 'first' | 'last' | null;
}

/** `ReferentialAction`, tagged by `kind`. */
export type PgReferentialAction =
  | { kind: 'noAction' | 'restrict' | 'cascade' }
  | { kind: 'setNull' | 'setDefault'; columns?: string[] };

/** `ExclusionElement`. */
export interface PgExclusionElement {
  element: PgIndexElement;
  operator: string;
  operatorSchema?: string | null;
}

/** `ConstraintKind`, tagged by `type`. */
export type PgConstraintKind =
  | { type: 'primaryKey'; columns: string[]; include?: string[] }
  | { type: 'unique'; columns: string[]; include?: string[]; nullsNotDistinct?: boolean }
  | { type: 'check'; expression: string; noInherit?: boolean }
  | {
      type: 'exclusion';
      method?: PgIndexMethod | null;
      elements: PgExclusionElement[];
      include?: string[];
      predicate?: string | null;
    }
  | {
      type: 'foreignKey';
      columns: string[];
      references: PgName;
      referencedColumns?: string[];
      matchType?: 'simple' | 'full' | null;
      onDelete?: PgReferentialAction | null;
      onUpdate?: PgReferentialAction | null;
    };

/** `TableConstraint`. */
export interface PgTableConstraint {
  name?: string | null;
  kind: PgConstraintKind;
  deferrable?: boolean;
  initiallyDeferred?: boolean;
}

/** `BoundValue`, tagged by `kind`. */
export type PgBoundValue =
  | { kind: 'minValue' | 'maxValue' | 'null' }
  | { kind: 'literal'; value: string }
  | { kind: 'expression'; sql: string };

/** `PartitionBound`, tagged by `kind`. */
export type PgPartitionBound =
  | { kind: 'range'; from: PgBoundValue[]; to: PgBoundValue[] }
  | { kind: 'list'; values: PgBoundValue[] }
  | { kind: 'hash'; modulus: number; remainder: number }
  | { kind: 'default' };

/** `DetachMode`. */
export type PgDetachMode = 'normal' | 'concurrently' | 'finalize';

/** `TableOperation`, tagged by `op` (the operations the designer uses). */
export type PgTableOperation =
  | { op: 'replicaIdentityUsingIndex'; index: string }
  | { op: 'clusterOn'; index: string }
  | { op: 'addColumn'; column: PgColumnDef }
  | { op: 'dropColumn'; name: string; behavior?: PgDropBehavior }
  | { op: 'renameColumn'; name: string; newName: string }
  | { op: 'alterColumnType'; name: string; dataType: string; collation?: string | null; using?: string | null }
  | { op: 'setDefault'; column: string; expression: string }
  | { op: 'dropDefault'; column: string }
  | { op: 'setNotNull'; column: string }
  | { op: 'dropNotNull'; column: string }
  | { op: 'addIdentity'; column: string; identity: PgIdentity }
  | { op: 'alterIdentity'; column: string; generation?: PgIdentityGeneration | null }
  | { op: 'dropIdentity'; column: string }
  | { op: 'setExpression'; column: string; expression: string }
  | { op: 'dropExpression'; column: string }
  | { op: 'addConstraint'; constraint: PgTableConstraint; notValid?: boolean }
  | { op: 'dropConstraint'; name: string; behavior?: PgDropBehavior }
  | { op: 'renameConstraint'; name: string; newName: string }
  | { op: 'validateConstraint'; name: string }
  | { op: 'attachPartition'; partition: PgName; bound: PgPartitionBound }
  | { op: 'detachPartition'; partition: PgName; mode?: PgDetachMode }
  | { op: 'rename'; newName: string }
  | { op: 'setSchema'; schema: string }
  | { op: 'owner'; owner: PgRoleSpec }
  | { op: 'setLogged'; logged: boolean }
  | { op: 'setStorageParams'; params: PgStorageParam[] }
  | { op: 'resetStorageParams'; names: string[] }
  | { op: 'setTablespace'; tablespace: string }
  | { op: 'inherit'; parent: PgName }
  | { op: 'noInherit'; parent: PgName };

/** `RoleSpec` (common.rs), tagged by `kind`; the designer names a role. */
export type PgRoleSpec = { kind: 'name'; name: string } | { kind: 'currentUser' | 'sessionUser' | 'currentRole' };

/** `PartitionStrategy`. */
export type PgPartitionStrategy = 'range' | 'list' | 'hash';

/** `PartitionKey`: a column or an expression of `PARTITION BY`. */
export interface PgPartitionKey {
  key: PgIndexKey;
  collation?: string | null;
  opclass?: PgName | null;
}

/** `PartitionBy`. */
export interface PgPartitionBy {
  strategy: PgPartitionStrategy;
  keys: PgPartitionKey[];
}

/** `CreateTable` (the fields the designer sets: a new table, or a partition). */
export interface PgCreateTable {
  name: PgName;
  columns?: PgColumnDef[];
  constraints?: PgTableConstraint[];
  partitionOf?: { parent: PgName; bound: PgPartitionBound } | null;
  /** Makes the new table partitioned. */
  partitionBy?: PgPartitionBy | null;
  unlogged?: boolean;
  inherits?: PgName[];
  storageParams?: PgStorageParam[];
  tablespace?: string | null;
  comment?: string | null;
}

/** `CreateIndex`. */
export interface PgCreateIndex {
  name?: string | null;
  table: PgName;
  unique?: boolean;
  nullsNotDistinct?: boolean;
  concurrently?: boolean;
  method?: PgIndexMethod | null;
  elements: PgIndexElement[];
  include?: string[];
  storageParams?: PgStorageParam[];
  tablespace?: string | null;
  predicate?: string | null;
  comment?: string | null;
}


/** The `PgChange` variants the designer sends (part of `PgChange` in api/ddl.ts), tagged by `object`, inner change by `action`. */
export type PgDesignerChange =
  | ({ object: 'table'; action: 'create' } & PgCreateTable)
  | { object: 'table'; action: 'alter'; name: PgName; operations: PgTableOperation[] }
  | { object: 'table'; action: 'drop'; name: PgName; behavior?: PgDropBehavior }
  | ({ object: 'index'; action: 'create' } & PgCreateIndex)
  | {
      object: 'index';
      action: 'alter';
      name: PgName;
      operations: ({ op: 'rename'; newName: string } | { op: 'setStatistics'; column: number; target: number })[];
    }
  | { object: 'index'; action: 'drop'; name: PgName; concurrently?: boolean; behavior?: PgDropBehavior }
  | {
      object: 'index';
      action: 'reindex';
      target: { kind: 'index'; name: PgName };
      concurrently?: boolean;
    };

// --- MySQL / MariaDB changes (crates/catalog/src/mysql/edit/{types,table,alter,index}.rs) ---

/** `TypeName`. */
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

/** `DataType`. */
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

/** `ColumnDefault`, tagged by `kind`. */
export type MyColumnDefault =
  | { kind: 'null' }
  | { kind: 'literal'; value: Value }
  | { kind: 'currentTimestamp'; precision?: number | null }
  | { kind: 'expression'; sql: string };

/** `ColumnDef`. */
export interface MyColumnDef {
  name: string;
  dataType: MyDataType;
  nullable: boolean;
  default?: MyColumnDefault | null;
  autoIncrement?: boolean;
  onUpdateCurrentTimestamp?: number | null;
  generated?: { expression: string; storage: 'virtual' | 'stored' } | null;
  invisible?: boolean;
  comment?: string | null;
  srid?: number | null;
  rowPeriod?: 'start' | 'end' | null;
}

/** `ColumnPosition`, tagged by `kind`. */
export type MyColumnPosition = { kind: 'first' } | { kind: 'after'; column: string };

/** `KeyPart`, tagged by `kind`. */
export type MyKeyPart =
  | { kind: 'column'; name: string; length?: number | null; descending?: boolean }
  | { kind: 'expression'; sql: string; descending?: boolean };

/** `IndexKind`. */
export type MyIndexKind = 'index' | 'unique' | 'fulltext' | 'spatial';

/** `IndexDef`. */
export interface MyIndexDef {
  name?: string | null;
  kind: MyIndexKind;
  method?: 'btree' | 'hash' | null;
  parts: MyKeyPart[];
  /** Full-text parser plugin (`WITH PARSER`). */
  parser?: string | null;
  comment?: string | null;
  invisible?: boolean;
}

/** `ReferenceAction`. */
export type MyReferenceAction = 'restrict' | 'cascade' | 'setNull' | 'noAction' | 'setDefault';

/** `TableConstraint`, tagged by `kind`. */
export type MyTableConstraint =
  | { kind: 'primaryKey'; parts: MyKeyPart[] }
  | {
      kind: 'foreignKey';
      name?: string | null;
      columns: string[];
      referencesDatabase?: string | null;
      referencesTable: string;
      referencesColumns: string[];
      onDelete?: MyReferenceAction | null;
      onUpdate?: MyReferenceAction | null;
    }
  | { kind: 'check'; name?: string | null; expression: string; enforced?: boolean | null };

/** `PartitionBound` (MySQL), tagged by `kind`. */
export type MyPartitionBound = { kind: 'maxValue' } | { kind: 'value'; value: Value } | { kind: 'expression'; sql: string };

/** `PartitionValues`, tagged by `kind`. */
export type MyPartitionValues =
  | { kind: 'lessThan'; bounds: MyPartitionBound[] }
  | { kind: 'in'; rows: MyPartitionBound[][] }
  | { kind: 'default' };

/** `SubPartitionDef`: a named subpartition. */
export interface MySubPartitionDef {
  name: string;
  comment?: string | null;
}

/** `PartitionDef`. */
export interface MyPartitionDef {
  name: string;
  values?: MyPartitionValues | null;
  comment?: string | null;
  /** Named subpartitions; every partition names the same number, or none (`SUBPARTITIONS n` applies). */
  subpartitions?: MySubPartitionDef[];
}

/** `SubPartitionMethod`, tagged by `kind`. */
export type MySubPartitionMethod = { kind: 'hash'; expression: string; linear?: boolean } | { kind: 'key'; columns?: string[]; linear?: boolean };

/** `SubPartitioning`: `SUBPARTITION BY ...` of range and list partitioning. */
export interface MySubPartitioning {
  method: MySubPartitionMethod;
  /** `SUBPARTITIONS n` when the subpartitions are not named one by one. */
  count?: number | null;
}

/** `PartitionMethod`, tagged by `kind`. */
export type MyPartitionMethod =
  | { kind: 'range' | 'list'; expression: string }
  | { kind: 'rangeColumns' | 'listColumns'; columns: string[] }
  | { kind: 'hash'; expression: string; linear?: boolean }
  | { kind: 'key'; columns?: string[]; linear?: boolean };

/** `Partitioning`. */
export interface MyPartitioning {
  method: MyPartitionMethod;
  count?: number | null;
  /** Subpartitioning (range and list partitioning only). */
  subpartition?: MySubPartitioning | null;
  partitions?: MyPartitionDef[];
}

/** `AlterOp`, tagged by `op` (the operations the designer uses; part of `MyChange.alterTable` in api/ddl.ts). */
export type MyDesignerAlterOp =
  | { op: 'addColumn'; column: MyColumnDef; position?: MyColumnPosition | null }
  | { op: 'dropColumn'; name: string; indexed?: boolean }
  | { op: 'modifyColumn'; previous: MyColumnDef; column: MyColumnDef; position?: MyColumnPosition | null; indexed?: boolean }
  | { op: 'changeColumn'; previous: MyColumnDef; column: MyColumnDef; position?: MyColumnPosition | null; indexed?: boolean }
  | { op: 'addIndex'; index: MyIndexDef }
  | { op: 'dropIndex'; name: string }
  | { op: 'renameIndex'; oldName: string; newName: string }
  | { op: 'setIndexVisibility'; name: string; invisible: boolean }
  | { op: 'addConstraint'; constraint: MyTableConstraint }
  | { op: 'dropPrimaryKey' }
  | { op: 'dropForeignKey'; name: string }
  | { op: 'dropCheck'; name: string }
  | { op: 'setCheckEnforced'; name: string; enforced: boolean }
  | { op: 'partitionBy'; partitioning: MyPartitioning }
  | { op: 'removePartitioning' }
  | { op: 'addPartitions'; partitions: MyPartitionDef[] }
  | { op: 'addPartitionCount'; count: number }
  | { op: 'dropPartitions'; names: string[] }
  | { op: 'truncatePartitions'; names: string[] }
  | { op: 'coalescePartitions'; count: number }
  /** `REORGANIZE PARTITION names INTO (into)`: split, merge or redefine partitions, keeping their rows. */
  | { op: 'reorganizePartitions'; names: string[]; into: MyPartitionDef[] }
  | { op: 'rename'; database?: string | null; name: string }
  | { op: 'setOptions'; options: MyTableOptions }
  | { op: 'convertCharset'; charset: string; collation?: string | null };

/** `RowFormat`. */
export type MyRowFormat = 'default' | 'dynamic' | 'fixed' | 'compressed' | 'redundant' | 'compact' | 'page';

/** `TableOptions` (the fields the designer sets); fields left out are not written. */
export interface MyTableOptions {
  engine?: string | null;
  autoIncrement?: number | null;
  charset?: string | null;
  collation?: string | null;
  comment?: string | null;
  rowFormat?: MyRowFormat | null;
}

/** The `MyChange` variant the designer adds to the editors' (api/ddl.ts), tagged by `change`. */
export type MyDesignerChange =
  | {
      change: 'createTable';
      database: string;
      name: string;
      columns: MyColumnDef[];
      constraints?: MyTableConstraint[];
      indexes?: MyIndexDef[];
      options?: MyTableOptions;
    };

// ---------------------------------------------------------------------------
// Table details (catalog_describe; crates/catalog/src/postgres/relations/types.rs, mysql/types.rs)
// ---------------------------------------------------------------------------

/** `PgQualifiedName` (both fields always present). */
export interface PgQualifiedName {
  schema: string;
  name: string;
}

/** `PgOption`. */
export interface PgOption {
  name: string;
  value: string;
}

/** `PgIdentity` (catalog side). 64-bit values arrive as decimal text. */
export interface PgIdentityInfo {
  generation: string;
  sequence: PgQualifiedName | null;
  start: string | null;
  increment: string | null;
  min: string | null;
  max: string | null;
  cache: string | null;
  cycle: boolean | null;
}

/** `PgColumn` (the fields the designer reads). */
export interface PgColumnInfo {
  name: string;
  position: number;
  dataType: string;
  notNull: boolean;
  default: string | null;
  identity: PgIdentityInfo | null;
  generated: { expression: string; stored: boolean } | null;
  collation: PgQualifiedName | null;
  local: boolean;
  inheritedCount: number;
  comment: string | null;
}

/** `PgIndexKey`. */
export interface PgIndexKeyInfo {
  /** Display text: the quoted column name or the expression. */
  definition: string;
  /** The column name as stored (not quoted); `null` for an expression. */
  column: string | null;
  expression: boolean;
  descending: boolean;
  nullsFirst: boolean;
  /** `COLLATE` when it differs from the key's own collation. */
  collation: PgQualifiedName | null;
  /** Operator class when it is not the default for the key type. */
  opclass: PgQualifiedName | null;
  opclassOptions: PgOption[];
  /** Per-column statistics target (expression keys); `null` for the default. */
  statisticsTarget: number | null;
}

/** `PgIndex` (the fields the designer reads). */
export interface PgIndexInfo {
  schema: string;
  name: string;
  method: string;
  unique: boolean;
  primary: boolean;
  exclusion: boolean;
  /** `NULLS NOT DISTINCT` (15+; `null` before). */
  nullsNotDistinct: boolean | null;
  /** The table is clustered on this index. */
  clustered: boolean;
  valid: boolean;
  /** The table's REPLICA IDENTITY index. */
  replicaIdentity: boolean;
  partitioned: boolean;
  parentIndex: PgQualifiedName | null;
  keys: PgIndexKeyInfo[];
  /** `INCLUDE` columns, names as stored (not quoted). */
  includeColumns: string[];
  predicate: string | null;
  storageParameters: PgOption[];
  tablespace: string | null;
  sizeBytes: number | null;
  constraint: string | null;
  definition: string;
  /** Foreign keys referencing this index (`schema.table.constraint`): it cannot be dropped while they exist. */
  foreignKeys: string[];
  comment: string | null;
}

/** `PgForeignKey`. */
export interface PgForeignKeyInfo {
  referenced: PgQualifiedName;
  referencedColumns: string[];
  onUpdate: string;
  onDelete: string;
  /** `simple`, `full` or `partial` (the designer writes only the first two). */
  matchType: string;
  onDeleteSetColumns: string[];
}

/** `PgConstraint` (the fields the designer reads). */
export interface PgConstraintInfo {
  name: string;
  /**
   * `primaryKey`, `unique`, `check`, `exclusion`, `foreignKey`, `notNull` (18+), or the raw
   * `pg_constraint.contype` letter of a type the catalog does not name (the designer locks those).
   */
  constraintType: string;
  columns: string[];
  definition: string;
  deferrable: boolean;
  deferred: boolean;
  validated: boolean;
  local: boolean;
  inheritedCount: number;
  hasParentConstraint: boolean;
  index: string | null;
  foreignKey: PgForeignKeyInfo | null;
  comment: string | null;
}

/** `PgPartitionEntry`. */
export interface PgPartitionEntry {
  schema: string;
  name: string;
  bound: string;
  isDefault: boolean;
  partitioned: boolean;
  foreign: boolean;
  detachPending: boolean;
}

/** `PgPartitioning`. */
export interface PgPartitioningInfo {
  strategy: string;
  key: string;
  partitions: PgPartitionEntry[];
  defaultPartition: PgQualifiedName | null;
}

/** `PgTable` (the fields the designer and grid read), `kind` from `PgRelationObject`. */
export interface PgTableInfo {
  kind: 'table' | 'partitionedTable' | 'partition' | 'foreignTable';
  schema: string;
  name: string;
  owner: string;
  unlogged: boolean;
  tablespace: string | null;
  /** Storage parameters, TOAST ones prefixed with `toast.`. */
  storageParameters: PgOption[];
  /** Parents in `INHERITS` (not the partition parent). */
  inherits: PgQualifiedName[];
  partitioning: PgPartitioningInfo | null;
  partitionOf: { parent: PgQualifiedName; bound: string; isDefault: boolean; detachPending: boolean } | null;
  rowEstimate: number | null;
  totalBytes: number | null;
  comment: string | null;
  system: boolean;
  columns: PgColumnInfo[];
  constraints: PgConstraintInfo[];
  indexes: PgIndexInfo[];
}

/** `MyDefault`, tagged by `type` with `value`. */
export type MyDefaultInfo = { type: 'null' } | { type: 'literal'; value: string } | { type: 'expression'; value: string };

/** `MyColumn`. */
export interface MyColumnInfo {
  table: string;
  name: string;
  position: number;
  dataType: string;
  columnType: string;
  nullable: boolean;
  default: MyDefaultInfo | null;
  autoIncrement: boolean;
  onUpdate: string | null;
  generated: { generatedKind: 'virtual' | 'stored' | 'rowStart' | 'rowEnd'; expression: string | null } | null;
  invisible: boolean;
  charset: string | null;
  collation: string | null;
  comment: string | null;
  srid: number | null;
  json: boolean;
  check: string | null;
  key: string | null;
}

/** `MyKeyPart` (catalog side). */
export interface MyKeyPartInfo {
  column: string | null;
  expression: string | null;
  prefixLength: number | null;
  descending: boolean;
}

/** `MyIndex`. */
export interface MyIndexInfo {
  table: string;
  name: string;
  indexKind: 'primary' | 'unique' | 'index' | 'fulltext' | 'spatial';
  method: string | null;
  parts: MyKeyPartInfo[];
  invisible: boolean;
  ignored: boolean;
  comment: string | null;
  /** Full-text parser (`WITH PARSER`), read in table details. */
  parser: string | null;
  /** The definition names `method` (`USING BTREE`); read in table details. */
  methodExplicit: boolean;
}

/** `MyConstraint`. */
export interface MyConstraintInfo {
  table: string;
  name: string;
  constraintType: 'primaryKey' | 'unique' | 'foreignKey' | 'check';
  columns: string[];
  references: {
    database: string;
    table: string;
    columns: string[];
    onUpdate: string | null;
    onDelete: string | null;
    matchOption: string | null;
  } | null;
  checkClause: string | null;
  enforced: boolean | null;
  level: 'column' | 'table' | null;
  json: boolean;
  period: boolean;
}

/** `MyPartition`. */
export interface MyPartitionInfo {
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
  subpartitions: MyPartitionInfo[];
}

/** `MyPartitioning`. */
export interface MyPartitioningInfo {
  method: string;
  linear: boolean;
  expression: string | null;
  subpartitionMethod: string | null;
  subpartitionExpression: string | null;
  partitions: MyPartitionInfo[];
}

/** `MyTable`, `kind` from `MyObject`. */
export interface MyTableInfo {
  kind: 'table';
  database: string;
  name: string;
  tableType: string;
  engine: string | null;
  /** `ROW_FORMAT` as the server writes it (`Dynamic`). */
  rowFormat: string | null;
  /** Next `AUTO_INCREMENT` value. */
  autoIncrement: number | null;
  collation: string | null;
  charset: string | null;
  rowEstimate: number | null;
  dataLength: number | null;
  indexLength: number | null;
  comment: string | null;
  systemVersioning: { rowStart: string | null; rowEnd: string | null } | null;
  columns: MyColumnInfo[];
  indexes: MyIndexInfo[];
  constraints: MyConstraintInfo[];
  partitioning: MyPartitioningInfo | null;
  system: boolean;
}

/** A table as `catalog_describe` returns it, narrowed to tables. */
export type TableDetails = { engine: 'postgres'; object: PgTableInfo } | { engine: 'mysql'; object: MyTableInfo };

/** `PgMaterializedView` (the fields the designer reads: its indexes are designed like a table's). */
export interface PgMatViewInfo {
  kind: 'materializedView';
  schema: string;
  name: string;
  owner: string;
  populated: boolean;
  tablespace: string | null;
  comment: string | null;
  system: boolean;
  columns: PgColumnInfo[];
  indexes: PgIndexInfo[];
}

/** What the designer edits: a table, or a PostgreSQL materialized view (indexes only). */
export type DesignerDetails = TableDetails | { engine: 'postgres'; object: PgMatViewInfo };

/** Reads a table or (PostgreSQL) a materialized view through `catalog_describe`. */
export async function describeDesignable(sessionId: string, object: ObjectRef): Promise<DesignerDetails> {
  if (object.kind !== 'materializedView') return describeTable(sessionId, object);
  const details = await call<{ engine: string; object: { kind?: string } }>('catalog_describe', { sessionId, object });
  if (details.engine === 'postgres' && details.object.kind === 'materializedView') return details as DesignerDetails;
  throw new Error(`${object.name} is not a materialized view.`);
}

/** `MyEngine` (crates/catalog/src/mysql/engines.rs): a storage engine the server offers. */
export interface MyEngine {
  name: string;
  /** The server's default engine for new tables. */
  default: boolean;
  transactions: boolean | null;
  comment: string | null;
  /** What moving a table to this engine does to its rows or the table (BLACKHOLE, MEMORY, ARCHIVE, CSV, ...); `null` for engines that keep rows on disk and take any table. */
  warning: string | null;
}

/**
 * The exact number of rows of a MySQL or MariaDB table (`catalog_row_count`): counted on the
 * metadata session (not the tab's, not in its query history) under a 2 second server-side cap;
 * `null` when the server stopped the count at the cap.
 */
export function catalogRowCount(sessionId: string, database: string, table: string): Promise<RowCount> {
  return call<RowCount>('catalog_row_count', { sessionId, database, table });
}

/** `catalog_row_count`'s result (`Option<u64>`): the rows, or `null` when the server stopped the count at its cap. */
export type RowCount = number | null;

/** The storage engines a MySQL or MariaDB server offers for new tables (`catalog_engines`). */
export function catalogEngines(sessionId: string): Promise<MyEngine[]> {
  return call<MyEngine[]>('catalog_engines', { sessionId });
}

const PG_TABLE_KINDS: readonly string[] = ['table', 'partitionedTable', 'partition', 'foreignTable'];

/**
 * Reads a table's columns, constraints, indexes and partitions through
 * `catalog_describe`. Rejects with an `internal` failure when the object
 * is not a table.
 */
export async function describeTable(sessionId: string, object: ObjectRef): Promise<TableDetails> {
  const details = await call<{ engine: string; object: { kind?: string } }>('catalog_describe', { sessionId, object });
  if (details.engine === 'postgres' && PG_TABLE_KINDS.includes(details.object.kind ?? '')) {
    return details as TableDetails;
  }
  if (details.engine === 'mysql' && details.object.kind === 'table') return details as TableDetails;
  throw new Error(`${object.name} is not a table.`);
}

/** The `ObjectRef` of a table for `catalog_describe`. */
export function tableRef(target: TableTarget, kind: ObjectKind = 'table'): ObjectRef {
  return { kind, database: target.database, schema: target.schema, parent: null, name: target.table, signature: null };
}
