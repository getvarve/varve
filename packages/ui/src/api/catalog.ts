import { call } from './invoke';
import type { Engine } from './records';

/** `ObjectKind` in crates/catalog/src/model.rs (camelCase on the wire). */
export type ObjectKind =
  | 'database'
  | 'schema'
  | 'table'
  | 'partitionedTable'
  | 'partition'
  | 'view'
  | 'materializedView'
  | 'column'
  | 'index'
  | 'constraint'
  | 'sequence'
  | 'function'
  | 'procedure'
  | 'aggregate'
  | 'trigger'
  | 'eventTrigger'
  | 'enumType'
  | 'compositeType'
  | 'domain'
  | 'rangeType'
  | 'extension'
  | 'rule'
  | 'policy'
  | 'publication'
  | 'subscription'
  | 'foreignDataWrapper'
  | 'foreignServer'
  | 'foreignTable'
  | 'tablespace'
  | 'collation'
  | 'cast'
  | 'operator'
  | 'event'
  | 'package'
  | 'packageBody';

/** Every kind, in the order of the Rust enum (the order `catalog_kinds` sorts by). */
export const OBJECT_KINDS: readonly ObjectKind[] = [
  'database',
  'schema',
  'table',
  'partitionedTable',
  'partition',
  'view',
  'materializedView',
  'column',
  'index',
  'constraint',
  'sequence',
  'function',
  'procedure',
  'aggregate',
  'trigger',
  'eventTrigger',
  'enumType',
  'compositeType',
  'domain',
  'rangeType',
  'extension',
  'rule',
  'policy',
  'publication',
  'subscription',
  'foreignDataWrapper',
  'foreignServer',
  'foreignTable',
  'tablespace',
  'collation',
  'cast',
  'operator',
  'event',
  'package',
  'packageBody',
];

/** `ObjectRef` in crates/catalog/src/model.rs. Fields that do not apply are `null`. */
export interface ObjectRef {
  kind: ObjectKind;
  /** PostgreSQL database or MySQL/MariaDB schema; `null` only for server-wide objects. */
  database: string | null;
  /** PostgreSQL schema. */
  schema: string | null;
  /** The table (or view) a column, index, constraint, trigger, rule, policy or partition belongs to. */
  parent: string | null;
  name: string;
  /** Identity arguments of an overloaded routine, or a cast's `source AS target`. */
  signature: string | null;
}

/** `Container` in crates/catalog/src/model.rs: a place in the tree whose objects are listed. */
export interface Container {
  /** `null` lists server-wide objects. */
  database: string | null;
  schema: string | null;
  /** The object whose children are listed (a table for its indexes, ...). */
  parent: ObjectRef | null;
}

/** `ObjectSummary` in crates/catalog/src/model.rs: one entry of the object tree. */
export interface ObjectSummary {
  object: ObjectRef;
  comment: string | null;
  /** Short secondary text (row estimate, size, language, index method, ...). */
  detail: string | null;
  /** Short badges (`unlogged`, `invisible`, `disabled`, ...). */
  flags: string[];
  hasChildren: boolean;
  /** Owned by the system or an extension; hidden unless system objects are shown. */
  system: boolean;
  /**
   * Estimated row count from the same list query (PostgreSQL `reltuples`,
   * MySQL/MariaDB `TABLE_ROWS`) for tables, partitions and materialized
   * views; `null` for other objects and when unknown (never analyzed).
   */
  rowEstimate: number | null;
  /** Size on disk in bytes (data, TOAST and indexes), for the same objects as `rowEstimate`. */
  sizeBytes: number | null;
}

/** `KindCount` in crates/catalog/src/model.rs. */
export interface KindCount {
  kind: ObjectKind;
  count: number;
}

/** `Capabilities` in crates/catalog/src/model.rs: the UI renders only what is listed. */
export interface Capabilities {
  engine: Engine;
  kinds: ObjectKind[];
  /** Kinds with a dedicated editor (create, alter, drop). */
  editable: ObjectKind[];
  /** Named features, for example `explainAnalyze`, `processList`, `galera`. */
  features: string[];
}

/**
 * `ObjectDetails` in crates/catalog/src/model.rs. The engine-specific
 * structures (`PgObject` in crates/catalog/src/postgres/mod.rs, `MyObject` in
 * crates/catalog/src/mysql/mod.rs) are typed by the detail and editor views;
 * pass them as type arguments to `catalogDescribe`.
 */
export type ObjectDetails<Pg = unknown, My = unknown> = { engine: 'postgres'; object: Pg } | { engine: 'mysql'; object: My };

export function catalogCapabilities(sessionId: string): Promise<Capabilities> {
  return call<Capabilities>('catalog_capabilities', { sessionId });
}

export function catalogDatabases(sessionId: string): Promise<ObjectSummary[]> {
  return call<ObjectSummary[]>('catalog_databases', { sessionId });
}

/** PostgreSQL schemas of a database (empty for MySQL/MariaDB). */
export function catalogSchemas(sessionId: string, database: string): Promise<ObjectSummary[]> {
  return call<ObjectSummary[]>('catalog_schemas', { sessionId, database });
}

/** The kinds present in a container with their counts. `includeSystem` `null` uses the profile's option. */
export function catalogKinds(sessionId: string, container: Container, includeSystem: boolean | null = null): Promise<KindCount[]> {
  return call<KindCount[]>('catalog_kinds', { sessionId, container, includeSystem });
}

export function catalogList(
  sessionId: string,
  container: Container,
  kind: ObjectKind,
  includeSystem: boolean | null = null,
): Promise<ObjectSummary[]> {
  return call<ObjectSummary[]>('catalog_list', { sessionId, container, kind, includeSystem });
}

export function catalogDescribe<Pg = unknown, My = unknown>(sessionId: string, object: ObjectRef): Promise<ObjectDetails<Pg, My>> {
  return call<ObjectDetails<Pg, My>>('catalog_describe', { sessionId, object });
}

export function catalogDdl(sessionId: string, object: ObjectRef): Promise<string> {
  return call<string>('catalog_ddl', { sessionId, object });
}

const KIND_LABELS: Record<ObjectKind, { one: string; many: string }> = {
  database: { one: 'Database', many: 'Databases' },
  schema: { one: 'Schema', many: 'Schemas' },
  table: { one: 'Table', many: 'Tables' },
  partitionedTable: { one: 'Partitioned table', many: 'Partitioned tables' },
  partition: { one: 'Partition', many: 'Partitions' },
  view: { one: 'View', many: 'Views' },
  materializedView: { one: 'Materialized view', many: 'Materialized views' },
  column: { one: 'Column', many: 'Columns' },
  index: { one: 'Index', many: 'Indexes' },
  constraint: { one: 'Constraint', many: 'Constraints' },
  sequence: { one: 'Sequence', many: 'Sequences' },
  function: { one: 'Function', many: 'Functions' },
  procedure: { one: 'Procedure', many: 'Procedures' },
  aggregate: { one: 'Aggregate', many: 'Aggregates' },
  trigger: { one: 'Trigger', many: 'Triggers' },
  eventTrigger: { one: 'Event trigger', many: 'Event triggers' },
  enumType: { one: 'Enum type', many: 'Enum types' },
  compositeType: { one: 'Composite type', many: 'Composite types' },
  domain: { one: 'Domain', many: 'Domains' },
  rangeType: { one: 'Range type', many: 'Range types' },
  extension: { one: 'Extension', many: 'Extensions' },
  rule: { one: 'Rule', many: 'Rules' },
  policy: { one: 'Policy', many: 'Policies' },
  publication: { one: 'Publication', many: 'Publications' },
  subscription: { one: 'Subscription', many: 'Subscriptions' },
  foreignDataWrapper: { one: 'Foreign data wrapper', many: 'Foreign data wrappers' },
  foreignServer: { one: 'Foreign server', many: 'Foreign servers' },
  foreignTable: { one: 'Foreign table', many: 'Foreign tables' },
  tablespace: { one: 'Tablespace', many: 'Tablespaces' },
  collation: { one: 'Collation', many: 'Collations' },
  cast: { one: 'Cast', many: 'Casts' },
  operator: { one: 'Operator', many: 'Operators' },
  event: { one: 'Event', many: 'Events' },
  package: { one: 'Package', many: 'Packages' },
  packageBody: { one: 'Package body', many: 'Package bodies' },
};

/** "Table" for one object of a kind, "Tables" for its group in the tree. */
export function kindLabel(kind: ObjectKind, plural = false): string {
  const label = KIND_LABELS[kind];
  return plural ? label.many : label.one;
}

/** Kinds whose rows can be read as a table (data tab, "select rows"). */
export const DATA_KINDS: readonly ObjectKind[] = ['table', 'partitionedTable', 'partition', 'view', 'materializedView', 'foreignTable'];

/** Kinds the table designer edits. */
export const TABLE_KINDS: readonly ObjectKind[] = ['table', 'partitionedTable'];

/** True when two references name the same object. */
export function sameObject(a: ObjectRef, b: ObjectRef): boolean {
  return (
    a.kind === b.kind &&
    a.database === b.database &&
    a.schema === b.schema &&
    a.parent === b.parent &&
    a.name === b.name &&
    a.signature === b.signature
  );
}

/** A stable text identity of an object, for tab deduplication and tree keys. */
export function objectIdentity(ref: ObjectRef): string {
  return JSON.stringify([ref.kind, ref.database, ref.schema, ref.parent, ref.name, ref.signature]);
}

/**
 * The object's name as shown to people: with the overload signature of a
 * routine and, unless `withParent` is false, the table of a child object.
 */
export function displayName(ref: ObjectRef, withParent = true): string {
  const name = ref.signature !== null && ['function', 'procedure', 'aggregate'].includes(ref.kind) ? `${ref.name}(${ref.signature})` : ref.name;
  return withParent && ref.parent !== null ? `${ref.parent}.${name}` : name;
}
