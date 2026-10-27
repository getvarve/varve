/**
 * The table designer's working copy and the changes it turns into.
 *
 * The designer starts from `catalog_describe`, edits a draft of columns,
 * indexes and constraints, and on Save diffs the draft against the table
 * as it was read. The result is a short list of `EngineChange`s, each
 * planned with `edit_plan` and applied in order:
 *
 * - PostgreSQL: index drops, then one `ALTER TABLE` with every column and
 *   constraint operation (one statement per operation, transactional),
 *   then index renames and creates, then column comments.
 * - MySQL / MariaDB: one `ALTER TABLE` with every operation.
 *
 * Partition actions and maintenance (validate, reindex) are single
 * changes of their own, built by the helpers at the end.
 */
import type {
  EngineChange,
  MyDesignerAlterOp as MyAlterOp,
  MyColumnDef,
  MyColumnDefault,
  MyColumnInfo,
  MyDataType,
  MyIndexDef,
  MyKeyPart,
  MyRowFormat,
  MyTableOptions,
  MyPartitionBound,
  MyPartitionDef,
  MyPartitionValues,
  MySubPartitioning,
  MyReferenceAction,
  MyTableConstraint,
  MyTableInfo,
  MyTypeName,
  PgBoundValue,
  PgChange,
  PgColumnDef,
  PgConstraintKind,
  PgIndexElement,
  PgIndexMethod,
  PgMatViewInfo,
  PgName,
  PgPartitionBy,
  PgPartitionStrategy,
  PgStorageParam,
  PgReferentialAction,
  PgTableConstraint,
  PgTableInfo,
  PgTableOperation,
  DesignerDetails,
} from '../api/grid';
import type { Value } from '../api/sessions';
import { checkExpression } from './gridSql';

// ---------------------------------------------------------------------------
// Draft
// ---------------------------------------------------------------------------

/** How a MySQL / MariaDB column default is written; PostgreSQL uses `none` or `expression`. */
export type DefaultKind = 'none' | 'null' | 'literal' | 'expression' | 'currentTimestamp';

/** One column of the working copy. */
export interface ColumnDraft {
  /** Stable id for lists and selection. */
  id: string;
  /** Name in the table as read; `null` for a new column. */
  origName: string | null;
  name: string;
  /** PostgreSQL: the type as `format_type` writes it. MySQL / MariaDB: `COLUMN_TYPE` (`varchar(20)`, `int unsigned`). */
  dataType: string;
  nullable: boolean;
  defaultKind: DefaultKind;
  /** The default: SQL (PostgreSQL, MySQL expression), the literal text, or the CURRENT_TIMESTAMP precision. */
  defaultText: string;
  /** PostgreSQL identity. */
  identity: 'none' | 'always' | 'byDefault';
  /** MySQL / MariaDB AUTO_INCREMENT. */
  autoIncrement: boolean;
  /** Generation expression; empty for none. */
  generated: string;
  generatedStored: boolean;
  collation: string;
  comment: string;
  /** PostgreSQL: `USING` for a type change. */
  using: string;
  /** MySQL / MariaDB: ON UPDATE CURRENT_TIMESTAMP precision; empty for none. */
  onUpdate: string;
  /** MySQL / MariaDB: INVISIBLE. */
  invisible: boolean;
  /** MySQL / MariaDB: moved with the arrows (written with FIRST / AFTER). */
  moved: boolean;
  dropped: boolean;
  /** Set when the designer cannot change this column (system-versioning columns, inherited columns, unknown types). */
  locked: string | null;
}

/** One key element of an index. */
export interface IndexPartDraft {
  /** A column name as stored (not quoted), or SQL for an expression part. */
  text: string;
  expression: boolean;
  descending: boolean;
  /** PostgreSQL NULLS FIRST / LAST; empty for the default. */
  nulls: '' | 'first' | 'last';
  /** PostgreSQL operator class (`gin_trgm_ops`, `"My Schema".my_ops`), MySQL prefix length; empty for none. */
  extra: string;
  /** PostgreSQL: the key as read from the server; absent for parts added in the designer. */
  read?: PgPartRead;
}

/** A PostgreSQL index key as read, so a redefined index keeps what the designer does not edit. */
export interface PgPartRead {
  /** `text` and `expression` as read: the collation is kept while the key is unchanged. */
  text: string;
  expression: boolean;
  /** `COLLATE` when it is not the key's own collation. */
  collation: PgName | null;
  /** Operator class as read and as shown in `extra`: kept (exactly, with its schema) while `extra` still shows it. */
  opclass: PgName | null;
  opclassText: string;
  /** Operator class parameters (13+), kept with the class. */
  opclassOptions: PgStorageParam[];
  /** Statistics target of the column (expression keys), kept while the key is unchanged. */
  statisticsTarget: number | null;
}

/** One index of the working copy. */
export interface IndexDraft {
  id: string;
  /** Name as read; `null` for a new index. */
  origName: string | null;
  name: string;
  /** PostgreSQL access method or MySQL `index`, `unique`, `fulltext`, `spatial`. */
  method: string;
  unique: boolean;
  parts: IndexPartDraft[];
  /** PostgreSQL INCLUDE columns. */
  include: string[];
  /** PostgreSQL partial index predicate. */
  predicate: string;
  /** PostgreSQL: build or drop CONCURRENTLY. */
  concurrently: boolean;
  nullsNotDistinct: boolean;
  /** PostgreSQL fillfactor; empty for the default. */
  fillfactor: string;
  /** PostgreSQL storage parameters as read, in stored order (kept when the index is redefined); fillfactor's value comes from `fillfactor`. */
  storageParams: PgStorageParam[];
  /** PostgreSQL tablespace as read; `null` for the database default. */
  tablespace: string | null;
  /** PostgreSQL: the table's REPLICA IDENTITY index; a redefinition sets it again. */
  replicaIdentity: boolean;
  /** PostgreSQL: the table is clustered on this index (`CLUSTER ON`); a redefinition sets it again. */
  clustered: boolean;
  /** MySQL / MariaDB: the method the definition names (`USING BTREE`, `USING HASH`), kept when redefined; `null` for the engine's default. */
  indexType: string | null;
  /** MySQL: the full-text parser (`ngram`); kept when redefined. */
  parser: string | null;
  /** Why the designer cannot drop or redefine this index (the user must change something else first). */
  locked: string | null;
  /** MySQL / MariaDB: invisible (MySQL) or ignored (MariaDB). */
  invisible: boolean;
  comment: string;
  /** The definition as the server writes it, for existing indexes. */
  definition: string;
  /** The constraint this index enforces (PRIMARY KEY, UNIQUE, EXCLUDE); changed through Constraints. */
  constraint: string | null;
  sizeBytes: number | null;
  /** Primary key index (MySQL `PRIMARY`). */
  primary: boolean;
  dropped: boolean;
  /** Set when the index definition changed (dropped and created again). */
  changed: boolean;
}

/** Constraint types of the designer. */
export type ConstraintType = 'primaryKey' | 'unique' | 'check' | 'foreignKey' | 'exclusion' | 'notNull';

/** Referential actions as the designer shows them. */
export type RefAction = 'noAction' | 'restrict' | 'cascade' | 'setNull' | 'setDefault';

/** One element of an exclusion constraint. */
export interface ExclusionPartDraft {
  text: string;
  expression: boolean;
  operator: string;
}

/** One constraint of the working copy. */
export interface ConstraintDraft {
  id: string;
  origName: string | null;
  name: string;
  type: ConstraintType;
  columns: string[];
  /** CHECK expression. */
  expression: string;
  /** Foreign key target: schema (PostgreSQL) or database (MySQL), and table. */
  refSchema: string;
  refTable: string;
  refColumns: string[];
  onDelete: RefAction;
  onUpdate: RefAction;
  matchFull: boolean;
  deferrable: boolean;
  initiallyDeferred: boolean;
  /** Add as NOT VALID (PostgreSQL CHECK and FOREIGN KEY). */
  notValid: boolean;
  /** MySQL CHECK enforced. */
  enforced: boolean;
  /** PostgreSQL exclusion constraint. */
  exclusionMethod: string;
  exclusion: ExclusionPartDraft[];
  /** The definition as the server writes it. */
  definition: string;
  validated: boolean;
  /** PostgreSQL: the constraint's comment (`COMMENT ON CONSTRAINT ... ON table`); empty for none. */
  comment: string;
  /** Inherited or partition-cloned constraints cannot be changed here. */
  locked: string | null;
  dropped: boolean;
  /** The definition changed: dropped and added again under the same name. */
  changed: boolean;
}

/**
 * Table properties (the designer's Properties section). One shape for both
 * engines; each engine reads its own fields.
 */
export interface TablePropsDraft {
  /** The table's name (a rename runs after every other change). */
  name: string;
  /** PostgreSQL schema (`SET SCHEMA`, after every other change). */
  schema: string;
  /** PostgreSQL owner role; empty keeps the creating role for a new table. */
  owner: string;
  comment: string;
  /** PostgreSQL UNLOGGED. */
  unlogged: boolean;
  /** PostgreSQL INHERITS parents as `schema.name` text. */
  inherits: string[];
  /** PostgreSQL storage parameters (`fillfactor`, `autovacuum_enabled`, `toast.*`). */
  storageParams: PgStorageParam[];
  /** PostgreSQL tablespace; empty for the database default. */
  tablespace: string;
  /** PostgreSQL: the database's default tablespace (read by the designer), where an emptied tablespace moves the table; empty when unknown (`pg_default`). Not a property of the table. */
  databaseTablespace: string;
  /** MySQL / MariaDB storage engine; empty for the server default. */
  engine: string;
  /** MySQL / MariaDB ROW_FORMAT in lower case; empty for the engine default. */
  rowFormat: string;
  /** MySQL / MariaDB next AUTO_INCREMENT value; empty for the default. */
  autoIncrement: string;
  /** MySQL / MariaDB default character set and collation. */
  charset: string;
  collation: string;
  /** MySQL / MariaDB: change the character set with CONVERT TO (every text column), not only the default for new columns. */
  convertCharset: boolean;
}

/** One key of a new PostgreSQL partitioned table: a column name or an expression. */
export interface PartitionKeyDraft {
  text: string;
  expression: boolean;
}

/** `PARTITION BY` of a new PostgreSQL table. */
export interface PartitionByDraft {
  enabled: boolean;
  strategy: PgPartitionStrategy;
  keys: PartitionKeyDraft[];
  /** Also create `<table>_default` as the DEFAULT partition (range and list only). */
  defaultPartition: boolean;
}

/** What the designer edits: a table kind, or a PostgreSQL materialized view (its indexes only). */
export type DesignedKind = 'table' | 'partitionedTable' | 'partition' | 'foreignTable' | 'materializedView';

/** The designer's working copy of a table. */
export interface TableDraft {
  engine: 'postgres' | 'mysql';
  /** Catalog kind as read; `table` for a new table (`partitionBy` decides). */
  kind: DesignedKind;
  /** PostgreSQL schema; MySQL / MariaDB database. */
  schema: string;
  table: string;
  database: string;
  columns: ColumnDraft[];
  indexes: IndexDraft[];
  constraints: ConstraintDraft[];
  /** MariaDB system-versioned table. */
  systemVersioned: boolean;
  /** PostgreSQL partitioned table: its indexes cannot be built or dropped CONCURRENTLY. */
  partitioned: boolean;
  /** A table that does not exist yet: Save creates it. */
  isNew: boolean;
  props: TablePropsDraft;
  /** New PostgreSQL tables only. */
  partitionBy: PartitionByDraft;
}

/** Properties of a table with nothing set. */
export function emptyProps(name: string, schema: string): TablePropsDraft {
  return {
    name,
    schema,
    owner: '',
    comment: '',
    unlogged: false,
    inherits: [],
    storageParams: [],
    tablespace: '',
    databaseTablespace: '',
    engine: '',
    rowFormat: '',
    autoIncrement: '',
    charset: '',
    collation: '',
    convertCharset: false,
  };
}

/** `PARTITION BY` turned off. */
export function noPartitionBy(): PartitionByDraft {
  return { enabled: false, strategy: 'range', keys: [], defaultPartition: false };
}

/** The working copy of a new table in `schema` (PostgreSQL) or `database` (MySQL / MariaDB), with one key column. */
export function newTableDraft(engine: 'postgres' | 'mysql', schema: string, database: string, partitioned = false): TableDraft {
  const id = newColumn(engine, 'id');
  id.nullable = false;
  if (engine === 'postgres') {
    id.dataType = 'bigint';
    id.identity = 'always';
  } else {
    id.dataType = 'bigint unsigned';
    id.autoIncrement = true;
  }
  const pk = newConstraint('primaryKey', engine === 'postgres' ? '' : 'PRIMARY');
  pk.columns = ['id'];
  const partitionBy = noPartitionBy();
  if (partitioned && engine === 'postgres') {
    partitionBy.enabled = true;
    partitionBy.keys = [{ text: 'id', expression: false }];
  }
  return {
    engine,
    kind: 'table',
    schema,
    table: 'new_table',
    database,
    columns: [id],
    indexes: [],
    constraints: [pk],
    systemVersioned: false,
    partitioned: partitionBy.enabled,
    isNew: true,
    props: emptyProps('new_table', schema),
    partitionBy,
  };
}

let nextDraftId = 1;
function draftId(prefix: string): string {
  const id = `${prefix}${nextDraftId}`;
  nextDraftId += 1;
  return id;
}

/** A blank new column. */
export function newColumn(engine: 'postgres' | 'mysql', name: string): ColumnDraft {
  return {
    id: draftId('c'),
    origName: null,
    name,
    dataType: engine === 'postgres' ? 'text' : 'varchar(255)',
    nullable: true,
    defaultKind: 'none',
    defaultText: '',
    identity: 'none',
    autoIncrement: false,
    generated: '',
    generatedStored: true,
    collation: '',
    comment: '',
    using: '',
    onUpdate: '',
    invisible: false,
    moved: false,
    dropped: false,
    locked: null,
  };
}

/** A blank new index. */
export function newIndex(engine: 'postgres' | 'mysql', name: string): IndexDraft {
  return {
    id: draftId('i'),
    origName: null,
    name,
    method: engine === 'postgres' ? 'btree' : 'index',
    unique: false,
    parts: [],
    include: [],
    predicate: '',
    concurrently: engine === 'postgres',
    nullsNotDistinct: false,
    fillfactor: '',
    storageParams: [],
    tablespace: null,
    replicaIdentity: false,
    clustered: false,
    indexType: null,
    parser: null,
    locked: null,
    invisible: false,
    comment: '',
    definition: '',
    constraint: null,
    sizeBytes: null,
    primary: false,
    dropped: false,
    changed: false,
  };
}

/** A blank new constraint. */
export function newConstraint(type: ConstraintType, name: string): ConstraintDraft {
  return {
    id: draftId('k'),
    origName: null,
    name,
    type,
    columns: [],
    expression: '',
    refSchema: '',
    refTable: '',
    refColumns: [],
    onDelete: 'noAction',
    onUpdate: 'noAction',
    matchFull: false,
    deferrable: false,
    initiallyDeferred: false,
    notValid: false,
    enforced: true,
    exclusionMethod: 'gist',
    exclusion: [],
    definition: '',
    validated: true,
    comment: '',
    locked: null,
    dropped: false,
    changed: false,
  };
}

// --- PostgreSQL ---

function stripParens(text: string): string {
  let t = text.trim();
  while (t.startsWith('(') && t.endsWith(')') && checkExpression('postgres', t.slice(1, -1)) === null) t = t.slice(1, -1).trim();
  return t;
}

/** The expression of a `CHECK (...)` definition as `pg_get_constraintdef` writes it. */
export function pgCheckExpression(definition: string): string {
  const m = /^CHECK\s*([\s\S]*?)(\s+NO INHERIT)?(\s+NOT VALID)?$/i.exec(definition.trim());
  return m?.[1] ? stripParens(m[1]) : definition;
}

const PG_ACTIONS: readonly RefAction[] = ['noAction', 'restrict', 'cascade', 'setNull', 'setDefault'];

function pgAction(text: string): RefAction {
  return (PG_ACTIONS as readonly string[]).includes(text) ? (text as RefAction) : 'noAction';
}

function pgColumnDraft(c: PgTableInfo['columns'][number]): ColumnDraft {
  const generated = c.generated?.expression ?? '';
  return {
    ...newColumn('postgres', c.name),
    origName: c.name,
    dataType: c.dataType,
    nullable: !c.notNull,
    defaultKind: c.default && !c.identity && !c.generated ? 'expression' : 'none',
    defaultText: c.identity || c.generated ? '' : (c.default ?? ''),
    identity: c.identity ? (c.identity.generation === 'always' ? 'always' : 'byDefault') : 'none',
    generated,
    generatedStored: c.generated?.stored ?? true,
    collation: c.collation?.name ?? '',
    comment: c.comment ?? '',
    locked: c.inheritedCount > 0 && !c.local ? 'Inherited from a parent table; change it there.' : null,
  };
}

/** The PostgreSQL index methods the designer offers; others (extensions such as `hnsw`, `bloom`) travel as `custom`. */
export const PG_CORE_INDEX_METHODS = ['btree', 'hash', 'gist', 'spgist', 'gin', 'brin'] as const;

/** `IndexMethod` for a method name: the core methods by kind, anything else as `custom` with its name. */
export function pgIndexMethod(method: string): PgIndexMethod {
  const m = method.trim();
  const core = PG_CORE_INDEX_METHODS.find((c) => c === m);
  return core ? { kind: core } : { kind: 'custom', name: m };
}

/** A PostgreSQL identifier as SQL: bare when it is a plain lower-case name, double-quoted otherwise. */
export function pgIdentText(name: string): string {
  return /^[a-z_][a-z0-9_$]*$/.test(name) ? name : `"${name.replace(/"/g, '""')}"`;
}

/** A qualified name as SQL text (`schema.name`, quoted where needed); `pg_catalog` is left out. */
export function pgNameText(name: PgName): string {
  return name.schema && name.schema !== 'pg_catalog' ? `${pgIdentText(name.schema)}.${pgIdentText(name.name)}` : pgIdentText(name.name);
}

/**
 * Parses `name`, `schema.name`, `"Quoted Name"` or `"My Schema"."x"` as PostgreSQL does:
 * unquoted parts fold to lower case, `""` inside quotes is one quote. `null` when it is not one or two names.
 */
export function parsePgName(text: string): PgName | null {
  const parts: string[] = [];
  let i = 0;
  const t = text.trim();
  while (i < t.length) {
    if (t[i] === '"') {
      let name = '';
      i += 1;
      for (;;) {
        if (i >= t.length) return null;
        if (t[i] === '"') {
          if (t[i + 1] === '"') {
            name += '"';
            i += 2;
            continue;
          }
          i += 1;
          break;
        }
        name += t[i];
        i += 1;
      }
      if (name === '') return null;
      parts.push(name);
    } else {
      const m = /^[A-Za-z_\u0080-\uffff][A-Za-z0-9_$\u0080-\uffff]*/.exec(t.slice(i));
      if (!m) return null;
      parts.push(m[0].toLowerCase());
      i += m[0].length;
    }
    if (i === t.length) break;
    if (t[i] !== '.') return null;
    i += 1;
    if (i === t.length) return null;
  }
  if (parts.length === 1) return { schema: null, name: parts[0] as string };
  if (parts.length === 2) return { schema: parts[0] as string, name: parts[1] as string };
  return null;
}

/** Why a PostgreSQL index cannot be dropped or redefined here, or `null`. */
function pgIndexLock(i: PgTableInfo['indexes'][number]): string | null {
  if (i.parentIndex) {
    return `Attached to the partitioned index ${i.parentIndex.schema}.${i.parentIndex.name}; change it on the partitioned table.`;
  }
  if (i.foreignKeys.length > 0) {
    return `Foreign keys reference this index (${i.foreignKeys.join(', ')}); drop or change them first.`;
  }
  return null;
}

function pgIndexDraft(i: PgTableInfo['indexes'][number]): IndexDraft {
  return {
    ...newIndex('postgres', i.name),
    origName: i.name,
    method: i.method,
    unique: i.unique,
    parts: i.keys.map((k) => {
      // A column key is sent back by its stored name; `definition` is quoted display text.
      const text = k.expression ? k.definition : (k.column ?? k.definition);
      const opclassText = k.opclass ? pgNameText(k.opclass) : '';
      return {
        text,
        expression: k.expression,
        descending: k.descending,
        nulls: k.nullsFirst !== k.descending ? (k.nullsFirst ? 'first' : 'last') : '',
        extra: opclassText,
        read: {
          text,
          expression: k.expression,
          collation: k.collation,
          opclass: k.opclass,
          opclassText,
          opclassOptions: k.opclassOptions.map((o) => ({ ...o })),
          statisticsTarget: k.statisticsTarget,
        },
      };
    }),
    include: [...i.includeColumns],
    predicate: i.predicate ?? '',
    // A partitioned index cannot be dropped or built CONCURRENTLY.
    concurrently: !i.partitioned,
    nullsNotDistinct: i.nullsNotDistinct ?? false,
    fillfactor: i.storageParameters.find((p) => p.name === 'fillfactor')?.value ?? '',
    storageParams: i.storageParameters.map((p) => ({ ...p })),
    tablespace: i.tablespace,
    replicaIdentity: i.replicaIdentity,
    clustered: i.clustered,
    locked: pgIndexLock(i),
    comment: i.comment ?? '',
    definition: i.definition,
    constraint: i.constraint,
    sizeBytes: i.sizeBytes,
    primary: i.primary,
  };
}

const PG_CONSTRAINT_TYPES: readonly ConstraintType[] = ['primaryKey', 'unique', 'check', 'foreignKey', 'exclusion', 'notNull'];

/** Why the designer cannot rewrite a constraint it read (an unknown type, `MATCH PARTIAL`), or `null`. */
function pgConstraintLock(k: PgTableInfo['constraints'][number]): string | null {
  if (!PG_CONSTRAINT_TYPES.includes(k.constraintType as ConstraintType)) {
    return `PostgreSQL reports a constraint type the designer does not know (${k.constraintType}); change it in a query.`;
  }
  const match = k.foreignKey?.matchType;
  if (match !== undefined && match !== 'simple' && match !== 'full') {
    return `The designer cannot write MATCH ${match.toUpperCase()}; change this foreign key in a query.`;
  }
  if (!k.local || k.hasParentConstraint) return 'Inherited from the parent table; change it there.';
  return null;
}

function pgConstraintDraft(k: PgTableInfo['constraints'][number]): ConstraintDraft {
  const known = PG_CONSTRAINT_TYPES.find((t) => t === k.constraintType);
  // An unknown type is shown read only by its definition (as a check would be).
  const base = newConstraint(known ?? 'check', k.name);
  const fk = k.foreignKey;
  return {
    ...base,
    origName: k.name,
    columns: [...k.columns],
    expression: k.constraintType === 'check' ? pgCheckExpression(k.definition) : known ? '' : k.definition,
    refSchema: fk?.referenced.schema ?? '',
    refTable: fk?.referenced.name ?? '',
    refColumns: fk ? [...fk.referencedColumns] : [],
    onDelete: fk ? pgAction(fk.onDelete) : 'noAction',
    onUpdate: fk ? pgAction(fk.onUpdate) : 'noAction',
    matchFull: fk?.matchType === 'full',
    deferrable: k.deferrable,
    initiallyDeferred: k.deferred,
    definition: k.definition,
    validated: k.validated,
    comment: k.comment ?? '',
    locked: pgConstraintLock(k),
  };
}

// --- MySQL / MariaDB ---

const MY_TYPES: readonly MyTypeName[] = [
  'tinyint', 'smallint', 'mediumint', 'int', 'bigint', 'decimal', 'float', 'double', 'bit', 'boolean', 'char',
  'varchar', 'binary', 'varbinary', 'tinytext', 'text', 'mediumtext', 'longtext', 'tinyblob', 'blob', 'mediumblob',
  'longblob', 'enum', 'set', 'date', 'time', 'datetime', 'timestamp', 'year', 'json', 'geometry', 'point',
  'linestring', 'polygon', 'multipoint', 'multilinestring', 'multipolygon', 'geometrycollection', 'inet4', 'inet6',
  'uuid', 'vector',
];

const MY_ALIASES: Record<string, MyTypeName> = {
  integer: 'int',
  bool: 'boolean',
  dec: 'decimal',
  numeric: 'decimal',
  fixed: 'decimal',
  real: 'double',
  'double precision': 'double',
  geomcollection: 'geometrycollection',
};

/** Splits `'a','b''c'` into its quoted members. */
function parseMembers(text: string): string[] | null {
  const out: string[] = [];
  let i = 0;
  while (i < text.length) {
    while (text[i] === ' ' || text[i] === ',') i += 1;
    if (i >= text.length) break;
    if (text[i] !== "'") return null;
    let value = '';
    i += 1;
    for (;;) {
      if (i >= text.length) return null;
      const c = text[i] as string;
      if (c === "'" && text[i + 1] === "'") {
        value += "'";
        i += 2;
      } else if (c === "'") {
        i += 1;
        break;
      } else if (c === '\\' && i + 1 < text.length) {
        value += text[i + 1];
        i += 2;
      } else {
        value += c;
        i += 1;
      }
    }
    out.push(value);
  }
  return out;
}

/**
 * Parses a MySQL / MariaDB column type (`COLUMN_TYPE` as the catalog
 * shows it, or what the user types) into a `DataType`. Throws with the
 * reason for a type the editor model does not know.
 */
export function parseMyColumnType(columnType: string, json = false): MyDataType {
  const text = columnType.trim();
  const m = /^([a-z][a-z0-9 ]*?)\s*(?:\(([\s\S]*)\))?((?:\s+(?:unsigned|signed|zerofill))*)\s*$/i.exec(text);
  if (!m?.[1]) throw new Error(`"${columnType}" is not a type Varve can edit.`);
  const raw = m[1].toLowerCase().trim();
  const known = (MY_TYPES as readonly string[]).includes(raw) ? (raw as MyTypeName) : null;
  const name: MyTypeName | null = json ? 'json' : (MY_ALIASES[raw] ?? known);
  if (!name) throw new Error(`"${raw}" is not a type Varve can edit.`);
  const out: MyDataType = { name };
  const args = m[2];
  if (args !== undefined && !json) {
    if (name === 'enum' || name === 'set') {
      const members = parseMembers(args);
      if (!members) throw new Error(`The ${name} values of "${columnType}" are not quoted correctly.`);
      out.values = members;
    } else {
      const nums = args.split(',').map((s) => s.trim());
      if (!nums.every((n) => /^\d+$/.test(n)) || nums.length > 2) throw new Error(`"${columnType}" has a length Varve cannot read.`);
      out.length = Number(nums[0]);
      if (nums[1] !== undefined) out.scale = Number(nums[1]);
    }
  }
  const flags = (m[3] ?? '').toLowerCase();
  if (flags.includes('unsigned')) out.unsigned = true;
  if (flags.includes('zerofill')) out.zerofill = true;
  return out;
}

function myLiteral(text: string, type: MyDataType): Value {
  if (/^-?\d+$/.test(text) && ['tinyint', 'smallint', 'mediumint', 'int', 'bigint', 'year'].includes(type.name)) {
    const n = Number(text);
    return Number.isSafeInteger(n) ? { t: 'int', v: n } : { t: 'decimal', v: text };
  }
  if (/^-?\d+(\.\d+)?([eE][+-]?\d+)?$/.test(text) && ['decimal', 'float', 'double'].includes(type.name)) {
    return { t: 'decimal', v: text };
  }
  if (type.name === 'bit') {
    const bits = /^b'([01]*)'$/i.exec(text);
    if (bits) {
      // BIT(64) values pass 2^53: those travel as exact decimal text.
      const value = BigInt(`0b${bits[1] || '0'}`);
      return { t: 'uInt', v: value <= BigInt(Number.MAX_SAFE_INTEGER) ? Number(value) : value.toString() };
    }
  }
  return { t: 'text', v: text };
}

const CURRENT_TS = /^current_timestamp(?:\((\d*)\))?$/i;

/** The MySQL default of a column draft, or `undefined` when it has none. */
function myDefault(c: ColumnDraft, type: MyDataType): MyColumnDefault | null {
  switch (c.defaultKind) {
    case 'none':
      return null;
    case 'null':
      return { kind: 'null' };
    case 'literal':
      return { kind: 'literal', value: myLiteral(c.defaultText, type) };
    case 'currentTimestamp': {
      const p = c.defaultText.trim();
      return { kind: 'currentTimestamp', precision: p === '' ? null : Number(p) };
    }
    case 'expression':
      return { kind: 'expression', sql: c.defaultText };
  }
}

function myColumnDraft(c: MyColumnInfo): ColumnDraft {
  let defaultKind: DefaultKind = 'none';
  let defaultText = '';
  if (c.default?.type === 'null') defaultKind = 'null';
  else if (c.default?.type === 'literal') {
    defaultKind = 'literal';
    defaultText = c.default.value;
  } else if (c.default?.type === 'expression') {
    const ts = CURRENT_TS.exec(c.default.value.trim());
    if (ts) {
      defaultKind = 'currentTimestamp';
      defaultText = ts[1] ?? '';
    } else {
      defaultKind = 'expression';
      defaultText = c.default.value;
    }
  }
  const onUpdateMatch = c.onUpdate ? CURRENT_TS.exec(c.onUpdate.trim()) : null;
  const onUpdate = onUpdateMatch ? onUpdateMatch[1] || '0' : '';
  const period = c.generated?.generatedKind === 'rowStart' || c.generated?.generatedKind === 'rowEnd';
  let locked: string | null = period ? 'A system-versioning period column; the server maintains it.' : null;
  if (!locked) {
    try {
      parseMyColumnType(c.columnType, c.json);
    } catch (e: unknown) {
      locked = e instanceof Error ? e.message : String(e);
    }
  }
  return {
    ...newColumn('mysql', c.name),
    origName: c.name,
    dataType: c.json ? 'json' : c.columnType,
    nullable: c.nullable,
    defaultKind,
    defaultText,
    autoIncrement: c.autoIncrement,
    generated: period ? '' : (c.generated?.expression ?? ''),
    generatedStored: c.generated?.generatedKind === 'stored',
    collation: c.collation ?? '',
    comment: c.comment ?? '',
    onUpdate,
    invisible: c.invisible,
    locked,
  };
}

/** The MySQL / MariaDB `ColumnDef` of a draft. Throws with the reason when the draft is not valid. */
export function myColumnDef(c: ColumnDraft, info?: MyColumnInfo): MyColumnDef {
  const type = parseMyColumnType(c.dataType, c.dataType.trim().toLowerCase() === 'json');
  if (c.collation.trim()) type.collation = c.collation.trim();
  if (info?.charset && ['char', 'varchar', 'tinytext', 'text', 'mediumtext', 'longtext', 'enum', 'set'].includes(type.name)) {
    type.charset = info.charset;
  }
  const def: MyColumnDef = { name: c.name, dataType: type, nullable: c.nullable };
  const d = myDefault(c, type);
  if (d) def.default = d;
  if (c.autoIncrement) def.autoIncrement = true;
  if (c.onUpdate.trim() !== '') def.onUpdateCurrentTimestamp = Number(c.onUpdate.trim());
  if (c.generated.trim() !== '') def.generated = { expression: c.generated.trim(), storage: c.generatedStored ? 'stored' : 'virtual' };
  if (c.invisible) def.invisible = true;
  if (c.comment !== '') def.comment = c.comment;
  if (info?.srid !== null && info?.srid !== undefined) def.srid = info.srid;
  if (info?.generated?.generatedKind === 'rowStart') def.rowPeriod = 'start';
  if (info?.generated?.generatedKind === 'rowEnd') def.rowPeriod = 'end';
  return def;
}

function myIndexDraft(i: MyTableInfo['indexes'][number]): IndexDraft {
  return {
    ...newIndex('mysql', i.name),
    origName: i.name,
    method: i.indexKind === 'primary' ? 'primary' : i.indexKind,
    unique: i.indexKind === 'unique' || i.indexKind === 'primary',
    parts: i.parts.map((p) => ({
      text: p.column ?? p.expression ?? '',
      expression: p.column === null,
      descending: p.descending,
      nulls: '',
      extra: p.prefixLength === null ? '' : String(p.prefixLength),
    })),
    concurrently: false,
    // Repeated only when the definition names it (MEMORY tables default to HASH).
    indexType: i.methodExplicit ? i.method : null,
    parser: i.parser,
    invisible: i.invisible || i.ignored,
    comment: i.comment ?? '',
    definition: `${i.indexKind.toUpperCase()} (${i.parts.map((p) => p.column ?? `(${p.expression ?? ''})`).join(', ')})${i.method ? ` USING ${i.method}` : ''}${i.parser ? ` WITH PARSER ${i.parser}` : ''}`,
    constraint: i.indexKind === 'primary' ? 'PRIMARY' : null,
    primary: i.indexKind === 'primary',
  };
}

const MY_ACTIONS: Record<string, RefAction> = {
  RESTRICT: 'restrict',
  CASCADE: 'cascade',
  'SET NULL': 'setNull',
  'NO ACTION': 'noAction',
  'SET DEFAULT': 'setDefault',
};

function myConstraintDraft(k: MyTableInfo['constraints'][number]): ConstraintDraft {
  const r = k.references;
  let locked: string | null = null;
  if (k.json) locked = 'The JSON check MariaDB adds to a JSON column; change the column type instead.';
  else if (k.period) locked = 'Part of an application-time period.';
  return {
    ...newConstraint(k.constraintType, k.name),
    origName: k.name,
    columns: [...k.columns],
    expression: k.checkClause ?? '',
    refSchema: r?.database ?? '',
    refTable: r?.table ?? '',
    refColumns: r ? [...r.columns] : [],
    onDelete: MY_ACTIONS[(r?.onDelete ?? 'NO ACTION').toUpperCase()] ?? 'noAction',
    onUpdate: MY_ACTIONS[(r?.onUpdate ?? 'NO ACTION').toUpperCase()] ?? 'noAction',
    enforced: k.enforced ?? true,
    definition: describeMyConstraint(k),
    locked,
  };
}

function describeMyConstraint(k: MyTableInfo['constraints'][number]): string {
  switch (k.constraintType) {
    case 'primaryKey':
      return `PRIMARY KEY (${k.columns.join(', ')})`;
    case 'unique':
      return `UNIQUE (${k.columns.join(', ')})`;
    case 'check':
      return `CHECK (${k.checkClause ?? ''})${k.enforced === false ? ' NOT ENFORCED' : ''}`;
    case 'foreignKey': {
      const r = k.references;
      return `(${k.columns.join(', ')}) REFERENCES ${r ? `${r.database}.${r.table} (${r.columns.join(', ')})` : '?'}${r?.onUpdate ? ` ON UPDATE ${r.onUpdate}` : ''}${r?.onDelete ? ` ON DELETE ${r.onDelete}` : ''}`;
    }
  }
}

/** The working copy of a table as read. */
export function draftFromDetails(details: DesignerDetails): TableDraft {
  if (details.engine === 'postgres') {
    const t = details.object;
    if (t.kind === 'materializedView') return matViewDraft(t);
    return {
      engine: 'postgres',
      kind: t.kind,
      schema: t.schema,
      table: t.name,
      database: '',
      columns: [...t.columns].sort((a, b) => a.position - b.position).map(pgColumnDraft),
      indexes: t.indexes.map(pgIndexDraft),
      constraints: t.constraints.filter((k) => k.constraintType !== 'notNull').map(pgConstraintDraft),
      systemVersioned: false,
      partitioned: t.kind === 'partitionedTable',
      isNew: false,
      props: {
        ...emptyProps(t.name, t.schema),
        owner: t.owner,
        comment: t.comment ?? '',
        unlogged: t.unlogged,
        inherits: t.inherits.map((p) => pgNameText({ schema: p.schema, name: p.name })),
        storageParams: t.storageParameters.map((o) => ({ name: o.name, value: o.value })),
        tablespace: t.tablespace ?? '',
      },
      partitionBy: noPartitionBy(),
    };
  }
  const t = details.object;
  return {
    engine: 'mysql',
    kind: 'table',
    schema: t.database,
    table: t.name,
    database: t.database,
    columns: [...t.columns].sort((a, b) => a.position - b.position).map(myColumnDraft),
    indexes: t.indexes.map(myIndexDraft),
    constraints: t.constraints.filter((k) => k.constraintType !== 'unique').map(myConstraintDraft),
    systemVersioned: t.systemVersioning !== null,
    partitioned: false,
    isNew: false,
    props: {
      ...emptyProps(t.name, t.database),
      comment: t.comment ?? '',
      engine: t.engine ?? '',
      rowFormat: (t.rowFormat ?? '').toLowerCase(),
      autoIncrement: t.autoIncrement === null || t.autoIncrement === undefined ? '' : String(t.autoIncrement),
      charset: t.charset ?? '',
      collation: t.collation ?? '',
    },
    partitionBy: noPartitionBy(),
  };
}

/** The working copy of a materialized view: its indexes; the columns (from its query) are only listed. */
function matViewDraft(v: PgMatViewInfo): TableDraft {
  const columns = [...v.columns].sort((a, b) => a.position - b.position).map(pgColumnDraft);
  for (const c of columns) c.locked = "A materialized view's columns come from its query; change them in its editor.";
  return {
    engine: 'postgres',
    kind: 'materializedView',
    schema: v.schema,
    table: v.name,
    database: '',
    columns,
    indexes: v.indexes.map(pgIndexDraft),
    constraints: [],
    systemVersioned: false,
    partitioned: false,
    isNew: false,
    props: { ...emptyProps(v.name, v.schema), owner: v.owner, comment: v.comment ?? '', tablespace: v.tablespace ?? '' },
    partitionBy: noPartitionBy(),
  };
}

/** A deep copy of a draft (the designer edits a copy and diffs it against the original). */
export function cloneDraft(draft: TableDraft): TableDraft {
  return JSON.parse(JSON.stringify(draft)) as TableDraft;
}

// ---------------------------------------------------------------------------
// Diff
// ---------------------------------------------------------------------------

/** What changed, for the footer ("1 added, 1 modified"). */
export interface DraftSummary {
  added: number;
  modified: number;
  dropped: number;
  total: number;
}

function columnChanged(a: ColumnDraft, b: ColumnDraft): boolean {
  const keys: (keyof ColumnDraft)[] = [
    'name', 'dataType', 'nullable', 'defaultKind', 'defaultText', 'identity', 'autoIncrement', 'generated',
    'generatedStored', 'collation', 'comment', 'onUpdate', 'invisible', 'moved',
  ];
  return keys.some((k) => (typeof a[k] === 'string' ? (a[k] as string).trim() !== (b[k] as string).trim() : a[k] !== b[k]));
}

/** True when a column of the draft differs from the table as read. */
export function isColumnModified(original: TableDraft, c: ColumnDraft): boolean {
  if (c.origName === null || c.dropped) return false;
  const was = original.columns.find((o) => o.id === c.id);
  return !!was && columnChanged(was, c);
}

/** True when an index was renamed, its visibility changed, or it was redefined. */
export function isIndexModified(original: TableDraft, i: IndexDraft): boolean {
  if (i.origName === null || i.dropped) return false;
  const was = original.indexes.find((o) => o.id === i.id);
  return !!was && (i.changed || was.name !== i.name || was.invisible !== i.invisible);
}

/** True when a constraint was renamed, redefined or (MySQL) its enforcement changed. */
export function isConstraintModified(original: TableDraft, k: ConstraintDraft): boolean {
  if (k.origName === null || k.dropped) return false;
  const was = original.constraints.find((o) => o.id === k.id);
  return !!was && (k.changed || was.name !== k.name || was.enforced !== k.enforced || was.comment !== k.comment);
}

/** The table properties that differ from the table as read (field names of {@link TablePropsDraft}). */
export function changedProps(original: TableDraft, draft: TableDraft): (keyof TablePropsDraft)[] {
  const a = original.props;
  const b = draft.props;
  const keys: (keyof TablePropsDraft)[] =
    draft.engine === 'postgres'
      ? ['name', 'schema', 'owner', 'comment', 'unlogged', 'inherits', 'storageParams', 'tablespace']
      : ['name', 'comment', 'engine', 'rowFormat', 'autoIncrement', 'charset', 'collation'];
  const same = (k: keyof TablePropsDraft): boolean => {
    const x = a[k];
    const y = b[k];
    if (typeof x === 'string' && typeof y === 'string') {
      // Engine names are case-insensitive; the rest compare as typed (trimmed, except the comment).
      if (k === 'engine') return x.trim().toLowerCase() === y.trim().toLowerCase();
      return k === 'comment' ? x === y : x.trim() === y.trim();
    }
    return JSON.stringify(x) === JSON.stringify(y);
  };
  return keys.filter((k) => !same(k));
}

/** Counts of added, modified and dropped items per section and in total. */
export function summarize(original: TableDraft, draft: TableDraft): Record<'columns' | 'indexes' | 'constraints' | 'properties', DraftSummary> & { total: number } {
  const count = <T extends { origName: string | null; dropped: boolean }>(items: T[], modified: (t: T) => boolean): DraftSummary => {
    const added = items.filter((i) => i.origName === null && !i.dropped).length;
    const dropped = items.filter((i) => i.origName !== null && i.dropped).length;
    const mod = items.filter(modified).length;
    return { added, dropped, modified: mod, total: added + dropped + mod };
  };
  const columns = count(draft.columns, (c) => isColumnModified(original, c));
  const indexes = count(draft.indexes, (i) => isIndexModified(original, i));
  const constraints = count(draft.constraints, (k) => isConstraintModified(original, k));
  const changed = draft.isNew ? 0 : changedProps(original, draft).length;
  const properties: DraftSummary = { added: 0, modified: changed, dropped: 0, total: changed };
  return { columns, indexes, constraints, properties, total: columns.total + indexes.total + constraints.total + properties.total };
}

/** The result of turning a draft into changes. */
export interface DraftChanges {
  changes: EngineChange[];
  /** Problems that keep the draft from being saved, in words. */
  errors: string[];
}

function exprProblem(dialect: 'postgres' | 'mysql', what: string, text: string): string | null {
  const p = checkExpression(dialect, text);
  return p ? `${what}: ${p}` : null;
}

function pgName(draft: TableDraft, name = draft.table): PgName {
  return { schema: draft.schema, name };
}

function pgRefAction(a: RefAction): PgReferentialAction {
  return { kind: a };
}

/** The PostgreSQL constraint definition of a draft. */
export function pgConstraintDef(draft: TableDraft, k: ConstraintDraft): PgTableConstraint {
  let kind: PgConstraintKind;
  switch (k.type) {
    case 'primaryKey':
      kind = { type: 'primaryKey', columns: k.columns };
      break;
    case 'unique':
      kind = { type: 'unique', columns: k.columns };
      break;
    case 'check':
      kind = { type: 'check', expression: k.expression.trim() };
      break;
    case 'foreignKey':
      kind = {
        type: 'foreignKey',
        columns: k.columns,
        references: { schema: k.refSchema || draft.schema, name: k.refTable },
        referencedColumns: k.refColumns,
        matchType: k.matchFull ? 'full' : null,
        onDelete: pgRefAction(k.onDelete),
        onUpdate: pgRefAction(k.onUpdate),
      };
      break;
    case 'exclusion':
      kind = {
        type: 'exclusion',
        method: { kind: k.exclusionMethod as 'gist' },
        elements: k.exclusion.map((e) => ({
          element: { key: e.expression ? { expression: e.text.trim() } : { column: e.text.trim() } },
          operator: e.operator.trim(),
        })),
      };
      break;
    case 'notNull':
      throw new Error('NOT NULL is set on the column.');
  }
  const out: PgTableConstraint = { name: k.name.trim() || null, kind };
  if (k.type === 'foreignKey' || k.type === 'unique' || k.type === 'primaryKey' || k.type === 'exclusion') {
    out.deferrable = k.deferrable;
    out.initiallyDeferred = k.deferrable && k.initiallyDeferred;
  }
  return out;
}

function constraintErrors(dialect: 'postgres' | 'mysql', k: ConstraintDraft): string[] {
  const label = k.name.trim() || `new ${k.type}`;
  const errors: string[] = [];
  if ((k.type === 'primaryKey' || k.type === 'unique' || k.type === 'foreignKey') && k.columns.length === 0) {
    errors.push(`Constraint ${label}: pick at least one column.`);
  }
  if (k.type === 'check') {
    const p = exprProblem(dialect, `Constraint ${label}`, k.expression);
    if (p) errors.push(p);
  }
  if (k.type === 'foreignKey') {
    if (!k.refTable.trim()) errors.push(`Constraint ${label}: choose the referenced table.`);
    if (k.refColumns.length !== k.columns.length) errors.push(`Constraint ${label}: map every column to a referenced column.`);
  }
  if (k.type === 'exclusion') {
    if (k.exclusion.length === 0) errors.push(`Constraint ${label}: add at least one element.`);
    for (const e of k.exclusion) {
      if (!e.text.trim() || !e.operator.trim()) errors.push(`Constraint ${label}: every element needs a column or expression and an operator.`);
      else if (e.expression) {
        const p = exprProblem(dialect, `Constraint ${label}`, e.text);
        if (p) errors.push(p);
      }
    }
  }
  return errors;
}

function pgColumnDef(c: ColumnDraft): PgColumnDef {
  const def: PgColumnDef = { name: c.name.trim(), dataType: c.dataType.trim() };
  if (!c.nullable) def.notNull = true;
  if (c.defaultKind === 'expression' && c.defaultText.trim()) def.default = c.defaultText.trim();
  if (c.identity !== 'none') def.identity = { generation: c.identity };
  if (c.generated.trim()) def.generated = { expression: c.generated.trim(), storage: c.generatedStored ? 'stored' : 'virtual' };
  if (c.collation.trim()) def.collation = c.collation.trim();
  if (c.comment !== '') def.comment = c.comment;
  return def;
}

function pgIndexElements(i: IndexDraft): PgIndexElement[] {
  return i.parts.map((p) => {
    const text = p.expression ? p.text.trim() : p.text;
    const e: PgIndexElement = { key: p.expression ? { expression: text } : { column: text } };
    const read = p.read;
    // The collation belongs to the key as read; a changed key gets its own.
    if (read?.collation && read.text === text && read.expression === p.expression) e.collation = read.collation;
    const opclass = p.extra.trim();
    if (opclass) {
      if (read?.opclass && opclass === read.opclassText) {
        e.opclass = read.opclass;
        if (read.opclassOptions.length > 0) e.opclassOptions = read.opclassOptions;
      } else {
        const name = parsePgName(opclass);
        if (name) e.opclass = name;
      }
    }
    if (p.descending) e.order = 'desc';
    if (p.nulls) e.nulls = p.nulls;
    return e;
  });
}

/** Storage parameters of a PostgreSQL index in the order read, with fillfactor from its field (first when it is new). */
function pgIndexParams(i: IndexDraft): PgStorageParam[] {
  const fillfactor = i.fillfactor.trim();
  const out: PgStorageParam[] = [];
  let placed = false;
  for (const p of i.storageParams) {
    if (p.name !== 'fillfactor') out.push(p);
    else if (fillfactor) {
      out.push({ name: 'fillfactor', value: fillfactor });
      placed = true;
    }
  }
  return fillfactor && !placed ? [{ name: 'fillfactor', value: fillfactor }, ...out] : out;
}

function indexErrors(dialect: 'postgres' | 'mysql', i: IndexDraft): string[] {
  const label = i.name.trim() || 'new index';
  const errors: string[] = [];
  if (i.parts.length === 0) errors.push(`Index ${label}: add at least one column or expression.`);
  for (const p of i.parts) {
    if (!p.text.trim()) errors.push(`Index ${label}: a key part is empty.`);
    else if (p.expression) {
      const e = exprProblem(dialect, `Index ${label}`, p.text);
      if (e) errors.push(e);
    }
    if (dialect === 'mysql' && p.extra.trim() && !/^\d+$/.test(p.extra.trim())) errors.push(`Index ${label}: a prefix length must be a number.`);
    if (dialect === 'postgres' && p.extra.trim() && p.extra.trim() !== p.read?.opclassText && !parsePgName(p.extra)) {
      errors.push(`Index ${label}: write the operator class as name or schema.name (double-quote names with capitals or spaces).`);
    }
  }
  if (i.predicate.trim()) {
    const e = exprProblem(dialect, `Index ${label} WHERE`, i.predicate);
    if (e) errors.push(e);
  }
  if (i.fillfactor.trim() && !/^\d+$/.test(i.fillfactor.trim())) errors.push(`Index ${label}: fillfactor must be a whole number from 10 to 100.`);
  return errors;
}

function pgDiff(original: TableDraft, draft: TableDraft): DraftChanges {
  const errors: string[] = [];
  const ops: PgTableOperation[] = [];
  const before: EngineChange[] = [];
  const after: EngineChange[] = [];
  const comments: EngineChange[] = [];
  const pg = (change: PgChange): EngineChange => ({ engine: 'postgres', change });

  // Indexes dropped (or redefined) before the table changes, so a column drop does not take them first.
  for (const i of draft.indexes) {
    if (i.origName === null || i.constraint) continue;
    if ((i.dropped || i.changed) && i.locked) {
      errors.push(`Index ${i.origName}: ${i.locked}`);
      continue;
    }
    // The replica identity index is swapped (built under another name first, see below), never dropped first.
    if (i.dropped || (i.changed && !swapsReplicaIdentity(i))) {
      before.push(pg({ object: 'index', action: 'drop', name: pgName(draft, i.origName), concurrently: i.concurrently }));
    }
  }

  // Columns.
  const names = new Set<string>();
  for (const c of draft.columns) {
    if (c.dropped) {
      if (c.origName !== null) ops.push({ op: 'dropColumn', name: c.origName });
      continue;
    }
    const name = c.name.trim();
    if (!name) errors.push('A column has no name.');
    if (names.has(name)) errors.push(`Two columns are called ${name}.`);
    names.add(name);
    if (!c.dataType.trim()) errors.push(`Column ${name}: choose a type.`);
    if (c.defaultKind === 'expression' && c.defaultText.trim()) {
      const p = exprProblem('postgres', `Default of ${name}`, c.defaultText);
      if (p) errors.push(p);
    }
    if (c.generated.trim()) {
      const p = exprProblem('postgres', `Generation of ${name}`, c.generated);
      if (p) errors.push(p);
    }
    if ([c.defaultKind === 'expression' && c.defaultText.trim() !== '', c.identity !== 'none', c.generated.trim() !== ''].filter(Boolean).length > 1) {
      errors.push(`Column ${name}: choose one of a default, an identity and a generation expression.`);
    }
    if (c.origName === null) {
      ops.push({ op: 'addColumn', column: pgColumnDef(c) });
      continue;
    }
    const was = original.columns.find((o) => o.id === c.id);
    if (!was || !columnChanged(was, c)) continue;
    if (c.locked) {
      errors.push(`Column ${was.name}: ${c.locked}`);
      continue;
    }
    if (was.name !== name) ops.push({ op: 'renameColumn', name: was.name, newName: name });
    if (was.dataType.trim() !== c.dataType.trim() || was.collation.trim() !== c.collation.trim()) {
      ops.push({
        op: 'alterColumnType',
        name,
        dataType: c.dataType.trim(),
        collation: c.collation.trim() || null,
        using: c.using.trim() || null,
      });
      if (c.using.trim()) {
        const p = exprProblem('postgres', `USING of ${name}`, c.using);
        if (p) errors.push(p);
      }
    }
    const wasDefault = was.defaultKind === 'expression' ? was.defaultText.trim() : '';
    const nowDefault = c.defaultKind === 'expression' ? c.defaultText.trim() : '';
    if (wasDefault !== nowDefault) {
      ops.push(nowDefault ? { op: 'setDefault', column: name, expression: nowDefault } : { op: 'dropDefault', column: name });
    }
    if (was.nullable !== c.nullable) ops.push(c.nullable ? { op: 'dropNotNull', column: name } : { op: 'setNotNull', column: name });
    if (was.identity !== c.identity) {
      if (was.identity === 'none') ops.push({ op: 'addIdentity', column: name, identity: { generation: c.identity as 'always' | 'byDefault' } });
      else if (c.identity === 'none') ops.push({ op: 'dropIdentity', column: name });
      else ops.push({ op: 'alterIdentity', column: name, generation: c.identity });
    }
    if (was.generated.trim() !== c.generated.trim() || was.generatedStored !== c.generatedStored) {
      if (!was.generated.trim()) {
        errors.push(`Column ${name}: PostgreSQL cannot turn an existing column into a generated column. Add a new column instead.`);
      } else if (!c.generated.trim()) {
        ops.push({ op: 'dropExpression', column: name });
      } else if (was.generatedStored !== c.generatedStored) {
        errors.push(`Column ${name}: PostgreSQL cannot switch a generated column between stored and virtual. Add a new column instead.`);
      } else {
        ops.push({ op: 'setExpression', column: name, expression: c.generated.trim() });
      }
    }
    if (was.comment !== c.comment) {
      comments.push(
        pg({
          object: 'comment',
          target: { kind: 'column', database: null, schema: draft.schema, parent: draft.table, name, signature: null },
          comment: c.comment === '' ? null : c.comment,
        }),
      );
    }
  }
  if (draft.columns.every((c) => c.dropped)) errors.push('A table needs at least one column; keep one or add one.');

  // Constraints: drops first (also for redefined ones), then renames, then adds.
  const adds: PgTableOperation[] = [];
  for (const k of draft.constraints) {
    if (k.origName !== null) {
      const was = original.constraints.find((o) => o.id === k.id);
      if (k.locked && (k.dropped || k.changed || (was && was.name !== k.name))) {
        errors.push(`Constraint ${k.origName}: ${k.locked}`);
        continue;
      }
      if (k.dropped || k.changed) ops.push({ op: 'dropConstraint', name: k.origName });
      if (k.dropped) continue;
      if (!k.changed && was && was.name !== k.name.trim()) ops.push({ op: 'renameConstraint', name: k.origName, newName: k.name.trim() });
      if (!k.changed) continue;
    } else if (k.dropped) continue;
    errors.push(...constraintErrors('postgres', k));
    try {
      const op: PgTableOperation = { op: 'addConstraint', constraint: pgConstraintDef(draft, k) };
      if (k.notValid && (k.type === 'check' || k.type === 'foreignKey')) op.notValid = true;
      adds.push(op);
    } catch (e: unknown) {
      errors.push(e instanceof Error ? e.message : String(e));
    }
  }
  ops.push(...adds);
  comments.push(...pgConstraintComments(draft, original, errors));

  // Indexes: renames and creates after the table changes.
  for (const i of draft.indexes) {
    if (i.dropped) continue;
    if (i.origName !== null && i.changed && i.locked) continue;
    if (i.origName !== null && !i.changed) {
      const was = original.indexes.find((o) => o.id === i.id);
      if (was && was.name !== i.name.trim()) {
        after.push(pg({ object: 'index', action: 'alter', name: pgName(draft, i.origName), operations: [{ op: 'rename', newName: i.name.trim() }] }));
      }
      continue;
    }
    errors.push(...indexErrors('postgres', i));
    let swap: string | null = null;
    if (swapsReplicaIdentity(i)) {
      const problems = replicaIdentityProblems(draft, i);
      if (problems.length > 0) {
        errors.push(
          `Index ${i.origName ?? i.name}: it is the table's replica identity, which needs a UNIQUE index without WHERE or expressions on NOT NULL columns (${problems.join('; ')}). Keep that, or change the table's replica identity first.`,
        );
        continue;
      }
      const leftovers = swapLeftovers(draft, i);
      const halfway = leftovers.find((l) => l.replicaIdentity);
      if (halfway) {
        errors.push(
          `Index ${i.origName ?? ''}: an earlier redefinition stopped halfway and ${halfway.origName ?? ''} is now the replica identity. Drop ${i.origName ?? ''} and rename ${halfway.origName ?? ''} first.`,
        );
        continue;
      }
      // Leftovers of an earlier attempt that failed (often INVALID after CONCURRENTLY) go first.
      for (const l of leftovers) {
        if (l.dropped || l.changed || l.origName === null) continue;
        after.push(pg({ object: 'index', action: 'drop', name: pgName(draft, l.origName), concurrently: i.concurrently }));
      }
      swap = swapName(i.name.trim(), swapToken());
    }
    after.push(
      pg({
        object: 'index',
        action: 'create',
        name: swap ?? (i.name.trim() || null),
        table: pgName(draft),
        unique: i.unique,
        nullsNotDistinct: i.unique && i.nullsNotDistinct,
        concurrently: i.concurrently,
        method: pgIndexMethod(i.method),
        elements: pgIndexElements(i),
        include: i.include,
        storageParams: pgIndexParams(i),
        tablespace: i.tablespace,
        predicate: i.predicate.trim() || null,
      }),
    );
    if (swap !== null && i.origName !== null) {
      // The table keeps a replica identity throughout: the new index takes over before the old one is dropped.
      after.push(...pgRestoreIndexState(draft, i, swap));
      after.push(pg({ object: 'index', action: 'drop', name: pgName(draft, i.origName), concurrently: i.concurrently }));
      after.push(pg({ object: 'index', action: 'alter', name: pgName(draft, swap), operations: [{ op: 'rename', newName: i.name.trim() }] }));
    } else if (i.origName !== null && i.name.trim()) {
      after.push(...pgRestoreIndexState(draft, i, i.name.trim()));
    }
  }

  const changes: EngineChange[] = [...before];
  if (ops.length > 0) changes.push(pg({ object: 'table', action: 'alter', name: pgName(draft), operations: ops }));
  changes.push(...after, ...comments);
  const props = pgPropsChanges(original, draft);
  changes.push(...props.changes);
  errors.push(...props.errors);
  return { changes, errors };
}

/**
 * `COMMENT ON CONSTRAINT ... ON table` for the constraints whose comment
 * changed, under their names after the table changes (renames, re-adds). A
 * constraint that is dropped and added again (redefined, or new) loses its
 * comment, so a non-empty one is set again. PostgreSQL names an unnamed new
 * constraint itself; it needs a name here to take a comment.
 */
function pgConstraintComments(draft: TableDraft, original: TableDraft | null, errors: string[]): EngineChange[] {
  const out: EngineChange[] = [];
  for (const k of draft.constraints) {
    if (k.dropped || k.type === 'notNull') continue;
    const was = k.origName === null || original === null ? undefined : original.constraints.find((o) => o.id === k.id);
    const readded = k.origName === null || k.changed || !was;
    const comment = k.comment === '' ? null : k.comment;
    if (readded) {
      if (comment === null) continue;
    } else if (was.comment === k.comment) continue;
    const name = k.name.trim();
    if (!name) {
      errors.push(`Constraint new ${k.type}: give it a name to set its comment.`);
      continue;
    }
    out.push({
      engine: 'postgres',
      change: { object: 'comment', target: { kind: 'constraint', database: null, schema: draft.schema, parent: draft.table.trim(), name, signature: null }, comment },
    });
  }
  return out;
}

/** The catalog reference of the designed table, for comments. */
function pgTableRef(draft: TableDraft) {
  return { kind: draft.kind, database: null, schema: draft.schema, parent: null, name: draft.table, signature: null };
}

/** Storage parameter problems (names are bare words such as `fillfactor` or `toast.autovacuum_enabled`). */
function storageParamErrors(params: readonly PgStorageParam[]): string[] {
  const errors: string[] = [];
  const seen = new Set<string>();
  for (const p of params) {
    const name = p.name.trim();
    if (!/^[A-Za-z_][A-Za-z0-9_]*(\.[A-Za-z_][A-Za-z0-9_]*)?$/.test(name)) errors.push(`Storage parameter "${p.name}": use a parameter name such as fillfactor or toast.autovacuum_enabled.`);
    else if (seen.has(name.toLowerCase())) errors.push(`Storage parameter ${name} is set twice.`);
    else if (p.value.trim() === '') errors.push(`Storage parameter ${name}: give a value.`);
    seen.add(name.toLowerCase());
  }
  return errors;
}

/** INHERITS parents as typed, parsed; unqualified names are in the table's schema. */
function pgParents(draft: TableDraft, texts: readonly string[], errors: string[]): PgName[] {
  const out: PgName[] = [];
  for (const t of texts) {
    if (!t.trim()) continue;
    const n = parsePgName(t);
    if (!n) {
      errors.push(`Parent table "${t}": write it as name or schema.name.`);
      continue;
    }
    out.push({ schema: n.schema ?? draft.schema, name: n.name });
  }
  return out;
}

/**
 * PostgreSQL table properties: owner, logging, storage parameters,
 * tablespace and inheritance in one ALTER TABLE, then the comment, then
 * the rename and SET SCHEMA last (every earlier change still names the
 * table as read).
 */
function pgPropsChanges(original: TableDraft, draft: TableDraft): DraftChanges {
  const errors: string[] = [];
  const changes: EngineChange[] = [];
  const changed = new Set(changedProps(original, draft));
  if (changed.size === 0) return { changes, errors };
  const pg = (change: PgChange): EngineChange => ({ engine: 'postgres', change });
  const a = original.props;
  const b = draft.props;
  const ops: PgTableOperation[] = [];
  if (changed.has('owner')) {
    if (!b.owner.trim()) errors.push('Choose the owner role.');
    else ops.push({ op: 'owner', owner: { kind: 'name', name: b.owner.trim() } });
  }
  if (changed.has('unlogged')) ops.push({ op: 'setLogged', logged: !b.unlogged });
  if (changed.has('storageParams')) {
    errors.push(...storageParamErrors(b.storageParams));
    const now = new Map(b.storageParams.map((p) => [p.name.trim().toLowerCase(), p] as const));
    const removed = a.storageParams.filter((p) => !now.has(p.name.toLowerCase())).map((p) => p.name);
    const set = b.storageParams
      .filter((p) => {
        const was = a.storageParams.find((o) => o.name.toLowerCase() === p.name.trim().toLowerCase());
        return !was || was.value !== p.value.trim();
      })
      .map((p) => ({ name: p.name.trim(), value: p.value.trim() }));
    if (removed.length > 0) ops.push({ op: 'resetStorageParams', names: removed });
    if (set.length > 0) ops.push({ op: 'setStorageParams', params: set });
  }
  // Emptied: back to the database's default tablespace (PostgreSQL then stores it as "the default").
  if (changed.has('tablespace')) ops.push({ op: 'setTablespace', tablespace: b.tablespace.trim() || b.databaseTablespace.trim() || 'pg_default' });
  if (changed.has('inherits')) {
    const before = pgParents(draft, a.inherits, []);
    const after = pgParents(draft, b.inherits, errors);
    const key = (n: PgName): string => `${n.schema ?? ''}.${n.name}`;
    for (const n of before) if (!after.some((m) => key(m) === key(n))) ops.push({ op: 'noInherit', parent: n });
    for (const n of after) if (!before.some((m) => key(m) === key(n))) ops.push({ op: 'inherit', parent: n });
  }
  if (ops.length > 0) changes.push(pg({ object: 'table', action: 'alter', name: pgName(draft), operations: ops }));
  if (changed.has('comment')) changes.push(pg({ object: 'comment', target: pgTableRef(draft), comment: b.comment === '' ? null : b.comment }));
  const last: PgTableOperation[] = [];
  if (changed.has('name')) {
    if (!b.name.trim()) errors.push('The table needs a name.');
    else last.push({ op: 'rename', newName: b.name.trim() });
  }
  if (changed.has('schema')) {
    if (!b.schema.trim()) errors.push('Choose the schema.');
    else last.push({ op: 'setSchema', schema: b.schema.trim() });
  }
  if (last.length > 0) changes.push(pg({ object: 'table', action: 'alter', name: pgName(draft), operations: last }));
  return { changes, errors };
}

const MY_ROW_FORMATS: readonly MyRowFormat[] = ['default', 'dynamic', 'fixed', 'compressed', 'redundant', 'compact', 'page'];

/** The MySQL / MariaDB table options of the draft: every field for a new table, only the changed ones otherwise. */
function myOptions(draft: TableDraft, only: Set<keyof TablePropsDraft> | null, errors: string[]): MyTableOptions {
  const b = draft.props;
  const want = (k: keyof TablePropsDraft): boolean => (only ? only.has(k) : b[k] !== '');
  const options: MyTableOptions = {};
  if (want('engine') && b.engine.trim()) options.engine = b.engine.trim();
  if (want('rowFormat') && b.rowFormat) {
    const f = b.rowFormat.toLowerCase() as MyRowFormat;
    if (!MY_ROW_FORMATS.includes(f)) errors.push(`Row format ${b.rowFormat} is not one the designer can set.`);
    else if (b.engine.trim() && rowFormatFor(b.engine, f) !== f) errors.push(`Row format ${f.toUpperCase()} does not work with ${b.engine.trim()}; choose another one.`);
    else options.rowFormat = f;
  }
  if (want('autoIncrement') && b.autoIncrement.trim()) {
    const n = Number(b.autoIncrement.trim());
    if (!/^\d+$/.test(b.autoIncrement.trim()) || !Number.isSafeInteger(n) || n < 1) errors.push('AUTO_INCREMENT must be a whole number from 1 up.');
    else options.autoIncrement = n;
  }
  if (want('charset') && b.charset.trim()) options.charset = b.charset.trim();
  if (want('collation') && b.collation.trim()) options.collation = b.collation.trim();
  if (only ? only.has('comment') : b.comment !== '') options.comment = b.comment;
  return options;
}

/** True when `collation` belongs to `charset` (`utf8mb4_bin` to `utf8mb4`; MariaDB's `uca1400_*` names fit every Unicode set). */
export function collationOf(charset: string, collation: string): boolean {
  const cs = charset.trim().toLowerCase();
  const coll = collation.trim().toLowerCase();
  if (coll === cs) return true;
  const prefixes = cs === 'utf8' || cs === 'utf8mb3' ? ['utf8_', 'utf8mb3_'] : [`${cs}_`];
  if (prefixes.some((p) => coll.startsWith(p))) return true;
  return /^uca1400_/.test(coll) && /^(utf8|utf8mb3|utf8mb4|ucs2|utf16|utf16le|utf32)$/.test(cs);
}

/** ROW_FORMAT values each engine accepts (lower case); engines not listed are not checked. */
const ENGINE_ROW_FORMATS: Record<string, readonly string[]> = {
  innodb: ['default', 'dynamic', 'compact', 'redundant', 'compressed'],
  aria: ['default', 'page', 'fixed', 'dynamic'],
  myisam: ['default', 'fixed', 'dynamic', 'compressed'],
  memory: ['default', 'fixed'],
};

/** The row formats `engine` accepts, or `null` when the designer does not know the engine. */
export function rowFormatsOf(engine: string): readonly string[] | null {
  return ENGINE_ROW_FORMATS[engine.trim().toLowerCase()] ?? null;
}

/** The row format a table keeps when it moves to `engine`: its own when the engine accepts it, else `default`. */
export function rowFormatFor(engine: string, rowFormat: string): string {
  const valid = rowFormatsOf(engine);
  return !rowFormat || !valid || valid.includes(rowFormat.toLowerCase()) ? rowFormat : 'default';
}

/** How moving rows to a MySQL / MariaDB engine loses them: at once (BLACKHOLE, engines reading elsewhere), at the next restart (MEMORY), or not. */
export function engineRowLoss(engine: string): 'discarded' | 'restart' | null {
  const e = engine.trim().toUpperCase();
  if (['BLACKHOLE', 'FEDERATED', 'FEDERATEDX', 'SPIDER', 'CONNECT'].includes(e)) return 'discarded';
  if (e === 'MEMORY' || e === 'HEAP') return 'restart';
  return null;
}

/** Most bytes one character takes in a MySQL / MariaDB character set; `null` when not known. */
function charsetBytes(charset: string): number | null {
  const sizes: Record<string, number> = { latin1: 1, ascii: 1, binary: 1, latin2: 1, cp1250: 1, cp1251: 1, cp1252: 1, utf8: 3, utf8mb3: 3, utf8mb4: 4, ucs2: 2, utf16: 4, utf16le: 4, utf32: 4, gbk: 2, big5: 2, sjis: 2, ujis: 3, euckr: 2, gb2312: 2 };
  return sizes[charset.trim().toLowerCase()] ?? null;
}

/**
 * Indexes that CONVERT TO `charset` would make too long: InnoDB allows 767
 * bytes per key part with ROW_FORMAT COMPACT or REDUNDANT, 3072 otherwise,
 * and 3072 bytes per index. Sizes come from the column types (and prefix
 * lengths); a warning, the server has the last word.
 */
export function convertIndexWarnings(table: MyTableInfo, charset: string): string[] {
  const bytes = charsetBytes(charset);
  if (bytes === null || (table.engine ?? '').toLowerCase() !== 'innodb') return [];
  const perPart = ['compact', 'redundant'].includes((table.rowFormat ?? '').toLowerCase()) ? 767 : 3072;
  const out: string[] = [];
  for (const index of table.indexes) {
    if (index.indexKind === 'fulltext' || index.indexKind === 'spatial') continue;
    let total = 0;
    for (const part of index.parts) {
      const column = table.columns.find((c) => c.name === part.column);
      if (!column) continue;
      const m = /^(?:var)?char\((\d+)\)/i.exec(column.columnType);
      if (!m?.[1]) continue;
      const size = (part.prefixLength ?? Number(m[1])) * bytes;
      total += size;
      if (size > perPart) out.push(`Index ${index.name}: column ${column.name} takes up to ${size} bytes in ${charset}, over the ${perPart}-byte key part limit; CONVERT TO fails unless the index uses a prefix.`);
    }
    if (total > 3072) out.push(`Index ${index.name} takes up to ${total} bytes in ${charset}, over InnoDB's 3072-byte index limit; CONVERT TO fails.`);
  }
  return [...new Set(out)];
}

/**
 * MySQL / MariaDB table properties as one ALTER TABLE of their own, after
 * the column and index changes: options (engine, row format,
 * AUTO_INCREMENT, character set, comment), CONVERT TO CHARACTER SET when
 * asked, and the rename last.
 */
function myPropsChanges(original: TableDraft, draft: TableDraft): DraftChanges {
  const errors: string[] = [];
  const changed = new Set(changedProps(original, draft));
  if (changed.size === 0) return { changes: [], errors };
  const b = draft.props;
  const ops: MyAlterOp[] = [];
  const optionKeys = new Set([...changed].filter((k) => k !== 'name'));
  const charsetChanged = optionKeys.has('charset') || optionKeys.has('collation');
  // A collation is sent only when the user changed it: the one read belongs to the old character set.
  const collation = optionKeys.has('collation') ? b.collation.trim() : '';
  const charset = b.charset.trim() || original.props.charset.trim();
  if (collation && charset && !collationOf(charset, collation)) {
    errors.push(`Collation ${collation} does not belong to character set ${charset}; choose one of ${charset}, or leave it empty for the character set's default.`);
  }
  if (charsetChanged && b.convertCharset) {
    optionKeys.delete('charset');
    optionKeys.delete('collation');
    if (!b.charset.trim()) errors.push('Choose the character set to convert to.');
    else ops.push({ op: 'convertCharset', charset: b.charset.trim(), collation: collation || null });
  }
  if (optionKeys.has('charset') && !collation) optionKeys.delete('collation');
  if (optionKeys.has('engine') && !b.engine.trim()) errors.push('Choose a storage engine.');
  if (optionKeys.has('autoIncrement') && !b.autoIncrement.trim()) optionKeys.delete('autoIncrement');
  if (optionKeys.has('rowFormat') && !b.rowFormat) optionKeys.delete('rowFormat');
  let effective = draft;
  if (optionKeys.has('engine')) {
    // The new engine must accept the row format the table keeps (Aria's PAGE fails on InnoDB).
    const kept = rowFormatFor(b.engine, b.rowFormat);
    if (kept !== b.rowFormat) {
      if (optionKeys.has('rowFormat')) errors.push(`Row format ${b.rowFormat.toUpperCase()} does not work with ${b.engine.trim()}; choose another one.`);
      else {
        optionKeys.add('rowFormat');
        effective = { ...draft, props: { ...b, rowFormat: kept } };
      }
    }
  }
  const options = myOptions(effective, optionKeys, errors);
  if (Object.keys(options).length > 0) ops.unshift({ op: 'setOptions', options });
  if (changed.has('name')) {
    if (!b.name.trim()) errors.push('The table needs a name.');
    else ops.push({ op: 'rename', name: b.name.trim() });
  }
  const changes: EngineChange[] =
    ops.length > 0 ? [{ engine: 'mysql', change: { change: 'alterTable', database: draft.database, name: draft.table, operations: ops, systemVersioned: draft.systemVersioned } }] : [];
  return { changes, errors };
}

/** A redefined replica identity index is built under another name and swapped in, so the table always has one. */
function swapsReplicaIdentity(i: IndexDraft): boolean {
  return i.origName !== null && i.changed && i.replicaIdentity && !i.dropped && i.name.trim() !== '';
}

/** The temporary name of a swapped index (PostgreSQL names are at most 63 bytes). */
export function swapName(name: string, token: string): string {
  const suffix = `${SWAP_MARK}_${token}`;
  const chars = [...name];
  while (new TextEncoder().encode(chars.join('') + suffix).length > 63) chars.pop();
  return chars.join('') + suffix;
}

/** Marks the temporary index of a replica identity swap. */
const SWAP_MARK = '_varve_swap';

/** A new token per attempt, so a leftover of a failed one never blocks the next. */
function swapToken(): string {
  return Math.random().toString(36).slice(2, 8).padEnd(6, '0');
}

/** Indexes left by earlier swap attempts for `i` (named after it with the swap mark). */
function swapLeftovers(draft: TableDraft, i: IndexDraft): IndexDraft[] {
  const base = i.origName ?? '';
  return draft.indexes.filter((o) => {
    const name = o.origName;
    if (o === i || name === null) return false;
    const at = name.lastIndexOf(SWAP_MARK);
    if (at < 0 || !/^_varve_swap(_[a-z0-9]+)?$/.test(name.slice(at))) return false;
    const prefix = name.slice(0, at);
    // The base name is cut when the whole name would pass 63 bytes.
    return prefix === base || (prefix !== '' && base.startsWith(prefix) && new TextEncoder().encode(name).length >= 56);
  });
}

/** Why `i` cannot stay the table's replica identity as redefined (empty when it can). */
function replicaIdentityProblems(draft: TableDraft, i: IndexDraft): string[] {
  const problems: string[] = [];
  if (!i.unique) problems.push('it is not UNIQUE');
  if (i.method !== 'btree') problems.push(`it uses ${i.method}, not btree`);
  if (i.predicate.trim()) problems.push('it has a WHERE clause');
  for (const p of i.parts) {
    if (p.expression) problems.push(`(${p.text.trim()}) is an expression`);
    else {
      const column = draft.columns.find((c) => !c.dropped && c.name === p.text);
      if (column?.nullable) problems.push(`${p.text} allows NULL`);
    }
  }
  return problems;
}

/**
 * What a redefined PostgreSQL index had besides its definition, set again
 * after the create: comment, column statistics targets, REPLICA IDENTITY
 * and CLUSTER ON (each its own change: CONCURRENTLY runs outside a transaction).
 */
function pgRestoreIndexState(draft: TableDraft, i: IndexDraft, name: string): EngineChange[] {
  const pg = (change: PgChange): EngineChange => ({ engine: 'postgres', change });
  const out: EngineChange[] = [];
  if (i.comment !== '') {
    out.push(pg({ object: 'comment', target: { kind: 'index', database: null, schema: draft.schema, parent: null, name, signature: null }, comment: i.comment }));
  }
  // A statistics target belongs to the column at its position, while the key there is unchanged.
  const statistics = i.parts.flatMap((p, n) => {
    const read = p.read;
    const text = p.expression ? p.text.trim() : p.text;
    return read && read.statisticsTarget !== null && read.text === text && read.expression === p.expression ? [{ op: 'setStatistics' as const, column: n + 1, target: read.statisticsTarget }] : [];
  });
  if (statistics.length > 0) out.push(pg({ object: 'index', action: 'alter', name: pgName(draft, name), operations: statistics }));
  const table: PgTableOperation[] = [];
  if (i.replicaIdentity) table.push({ op: 'replicaIdentityUsingIndex', index: name });
  if (i.clustered) table.push({ op: 'clusterOn', index: name });
  if (table.length > 0) out.push(pg({ object: 'table', action: 'alter', name: pgName(draft), operations: table }));
  return out;
}

function myIndexDef(i: IndexDraft): MyIndexDef {
  const kind = (['index', 'unique', 'fulltext', 'spatial'].includes(i.method) ? i.method : 'index') as MyIndexDef['kind'];
  const def: MyIndexDef = {
    name: i.name.trim() || null,
    kind,
    parts: i.parts.map((p): MyKeyPart =>
      p.expression
        ? { kind: 'expression', sql: p.text.trim(), descending: p.descending }
        : { kind: 'column', name: p.text.trim(), length: p.extra.trim() ? Number(p.extra.trim()) : null, descending: p.descending },
    ),
  };
  // The method as read (MEMORY tables default to HASH, so BTREE must be written) and the full-text parser.
  const method = i.indexType?.toLowerCase();
  if ((kind === 'index' || kind === 'unique') && (method === 'btree' || method === 'hash')) def.method = method;
  if (kind === 'fulltext' && i.parser) def.parser = i.parser;
  if (i.comment) def.comment = i.comment;
  if (i.invisible) def.invisible = true;
  return def;
}

function myConstraintDef(draft: TableDraft, k: ConstraintDraft): MyTableConstraint {
  switch (k.type) {
    case 'primaryKey':
      return { kind: 'primaryKey', parts: k.columns.map((name): MyKeyPart => ({ kind: 'column', name })) };
    case 'foreignKey':
      return {
        kind: 'foreignKey',
        name: k.name.trim() || null,
        columns: k.columns,
        referencesDatabase: k.refSchema && k.refSchema !== draft.database ? k.refSchema : null,
        referencesTable: k.refTable,
        referencesColumns: k.refColumns,
        onDelete: k.onDelete as MyReferenceAction,
        onUpdate: k.onUpdate as MyReferenceAction,
      };
    case 'check':
      return { kind: 'check', name: k.name.trim() || null, expression: k.expression.trim(), enforced: k.enforced ? null : false };
    default:
      throw new Error(`MySQL and MariaDB have no ${k.type} constraint here; use an index.`);
  }
}

/** MySQL and MariaDB refuse a table whose columns are all INVISIBLE. */
function visibleColumnError(draft: TableDraft): string | null {
  const live = draft.columns.filter((c) => !c.dropped);
  return draft.engine === 'mysql' && live.length > 0 && live.every((c) => c.invisible) ? 'A table needs at least one visible column.' : null;
}

function myDiff(original: TableDraft, draft: TableDraft, infos: readonly MyColumnInfo[]): DraftChanges {
  const errors: string[] = [];
  const visibility = visibleColumnError(draft);
  if (visibility) errors.push(visibility);
  const ops: MyAlterOp[] = [];
  const indexed = (name: string): boolean =>
    original.indexes.some((i) => i.parts.some((p) => !p.expression && p.text.toLowerCase() === name.toLowerCase()));

  // Index and constraint drops first, so a column can be dropped or changed after them.
  for (const i of draft.indexes) {
    if (i.origName === null || i.primary) continue;
    if (i.dropped || i.changed) ops.push({ op: 'dropIndex', name: i.origName });
  }
  for (const k of draft.constraints) {
    if (k.origName === null || !(k.dropped || k.changed)) continue;
    if (k.locked) {
      errors.push(`Constraint ${k.origName}: ${k.locked}`);
      continue;
    }
    if (k.type === 'primaryKey') ops.push({ op: 'dropPrimaryKey' });
    else if (k.type === 'foreignKey') ops.push({ op: 'dropForeignKey', name: k.origName });
    else if (k.type === 'check') ops.push({ op: 'dropCheck', name: k.origName });
  }

  // Columns.
  const names = new Set<string>();
  let previous: string | null = null;
  for (const c of draft.columns) {
    if (c.dropped) {
      if (c.origName !== null) ops.push({ op: 'dropColumn', name: c.origName, indexed: indexed(c.origName) });
      continue;
    }
    const name = c.name.trim();
    const position = previous === null ? { kind: 'first' as const } : { kind: 'after' as const, column: previous };
    previous = name;
    if (!name) errors.push('A column has no name.');
    if (names.has(name.toLowerCase())) errors.push(`Two columns are called ${name}.`);
    names.add(name.toLowerCase());
    if (c.defaultKind === 'expression') {
      const p = exprProblem('mysql', `Default of ${name}`, c.defaultText);
      if (p) errors.push(p);
    }
    if (c.generated.trim()) {
      const p = exprProblem('mysql', `Generation of ${name}`, c.generated);
      if (p) errors.push(p);
    }
    const info = c.origName === null ? undefined : infos.find((i) => i.name === c.origName);
    let column: MyColumnDef;
    try {
      column = myColumnDef({ ...c, name }, info);
    } catch (e: unknown) {
      errors.push(`Column ${name}: ${e instanceof Error ? e.message : String(e)}`);
      continue;
    }
    if (c.origName === null) {
      ops.push({ op: 'addColumn', column, position: c.moved || draft.columns.indexOf(c) < draft.columns.length - 1 ? position : null });
      continue;
    }
    const was = original.columns.find((o) => o.id === c.id);
    if (!was || !columnChanged(was, c)) continue;
    if (c.locked) {
      errors.push(`Column ${was.name}: ${c.locked}`);
      continue;
    }
    let prev: MyColumnDef;
    try {
      prev = myColumnDef(was, info);
    } catch (e: unknown) {
      errors.push(`Column ${was.name}: ${e instanceof Error ? e.message : String(e)}`);
      continue;
    }
    const op: MyAlterOp =
      was.name !== name
        ? { op: 'changeColumn', previous: prev, column, position: c.moved ? position : null, indexed: indexed(was.name) }
        : { op: 'modifyColumn', previous: prev, column, position: c.moved ? position : null, indexed: indexed(was.name) };
    ops.push(op);
  }
  if (draft.columns.every((c) => c.dropped)) errors.push('A table needs at least one column; keep one or add one.');

  // Indexes: renames, visibility, adds.
  for (const i of draft.indexes) {
    if (i.dropped || i.primary) continue;
    if (i.origName !== null && !i.changed) {
      const was = original.indexes.find((o) => o.id === i.id);
      if (was && was.name !== i.name.trim()) ops.push({ op: 'renameIndex', oldName: i.origName, newName: i.name.trim() });
      if (was && was.invisible !== i.invisible) ops.push({ op: 'setIndexVisibility', name: i.name.trim(), invisible: i.invisible });
      continue;
    }
    errors.push(...indexErrors('mysql', i));
    ops.push({ op: 'addIndex', index: myIndexDef(i) });
  }

  // Constraints: enforcement changes and adds.
  for (const k of draft.constraints) {
    if (k.dropped) continue;
    if (k.origName !== null && !k.changed) {
      const was = original.constraints.find((o) => o.id === k.id);
      if (was && was.name !== k.name.trim()) errors.push(`Constraint ${k.origName}: MySQL and MariaDB cannot rename a constraint; drop it and add it again.`);
      if (was && k.type === 'check' && was.enforced !== k.enforced) ops.push({ op: 'setCheckEnforced', name: k.origName, enforced: k.enforced });
      continue;
    }
    errors.push(...constraintErrors('mysql', k));
    try {
      ops.push({ op: 'addConstraint', constraint: myConstraintDef(draft, k) });
    } catch (e: unknown) {
      errors.push(e instanceof Error ? e.message : String(e));
    }
  }

  const changes: EngineChange[] =
    ops.length > 0
      ? [{ engine: 'mysql', change: { change: 'alterTable', database: draft.database, name: draft.table, operations: ops, systemVersioned: draft.systemVersioned } }]
      : [];
  const props = myPropsChanges(original, draft);
  changes.push(...props.changes);
  errors.push(...props.errors);
  return { changes, errors };
}

/**
 * The changes that turn the table as read (`original`) into the draft.
 * `myColumns` are the MySQL / MariaDB catalog columns (character sets and
 * SRIDs that the draft does not show); ignored for PostgreSQL.
 */
export function buildDraftChanges(original: TableDraft, draft: TableDraft, myColumns: readonly MyColumnInfo[] = [], serverMajor = Number.MAX_SAFE_INTEGER): DraftChanges {
  const result = draft.isNew ? createChanges(draft, serverMajor) : draft.engine === 'postgres' ? pgDiff(original, draft) : myDiff(original, draft, myColumns);
  return { changes: result.changes, errors: [...new Set(result.errors)] };
}

function newTableErrors(draft: TableDraft, serverMajor: number): string[] {
  const errors: string[] = [];
  if (!draft.table.trim()) errors.push('The table needs a name.');
  if (draft.engine === 'postgres' && new TextEncoder().encode(draft.table.trim()).length > 63) errors.push('PostgreSQL names are at most 63 bytes; shorten the table name.');
  const live = draft.columns.filter((c) => !c.dropped);
  if (live.length === 0) errors.push('A table needs at least one column.');
  const visibility = visibleColumnError(draft);
  if (visibility) errors.push(visibility);
  const names = new Set<string>();
  const dialect = draft.engine;
  for (const c of live) {
    const name = c.name.trim();
    if (!name) errors.push('A column has no name.');
    const folded = dialect === 'mysql' ? name.toLowerCase() : name;
    if (names.has(folded)) errors.push(`Two columns are called ${name}.`);
    names.add(folded);
    if (!c.dataType.trim()) errors.push(`Column ${name}: choose a type.`);
    if (['expression', 'literal'].includes(c.defaultKind) && dialect === 'postgres' && c.defaultText.trim()) {
      const p = exprProblem(dialect, `Default of ${name}`, c.defaultText);
      if (p) errors.push(p);
    }
    if (c.defaultKind === 'expression' && dialect === 'mysql') {
      const p = exprProblem(dialect, `Default of ${name}`, c.defaultText);
      if (p) errors.push(p);
    }
    if (c.generated.trim()) {
      const p = exprProblem(dialect, `Generation of ${name}`, c.generated);
      if (p) errors.push(p);
    }
  }
  for (const k of draft.constraints) if (!k.dropped) errors.push(...constraintErrors(dialect, k));
  for (const i of draft.indexes) if (!i.dropped) errors.push(...indexErrors(dialect, i));
  if (dialect === 'postgres') {
    errors.push(...storageParamErrors(draft.props.storageParams));
    errors.push(...partitionByErrors(draft, serverMajor));
  }
  return errors;
}

/** Problems of `PARTITION BY` on a new PostgreSQL table. */
function partitionByErrors(draft: TableDraft, serverMajor: number): string[] {
  const by = draft.partitionBy;
  if (!by.enabled) return [];
  const errors: string[] = [];
  const keys = by.keys.filter((k) => k.text.trim() !== '');
  if (keys.length === 0) errors.push('Choose at least one partition key column or expression.');
  if (by.strategy === 'list' && keys.length > 1) errors.push('List partitioning takes exactly one key.');
  if (by.strategy === 'hash' && by.defaultPartition) errors.push('A hash partitioned table cannot have a default partition.');
  if (draft.props.unlogged) errors.push('A partitioned table cannot be unlogged.');
  if (draft.props.inherits.some((t) => t.trim())) errors.push('A partitioned table cannot inherit from other tables.');
  if (draft.props.storageParams.length > 0) errors.push('A partitioned table takes no storage parameters; set them on its partitions.');
  const live = new Set(draft.columns.filter((c) => !c.dropped).map((c) => c.name.trim()));
  for (const k of keys) {
    if (k.expression) {
      const p = exprProblem('postgres', 'Partition key', k.text);
      if (p) errors.push(p);
    } else if (!live.has(k.text.trim())) errors.push(`Partition key ${k.text.trim()} is not a column of the table.`);
  }
  // PostgreSQL enforces unique keys per partition: they must contain every key column (expressions cannot be part of one).
  const columns = keys.filter((k) => !k.expression).map((k) => k.text.trim());
  for (const k of draft.constraints) {
    if (k.dropped || (k.type !== 'primaryKey' && k.type !== 'unique')) continue;
    const what = k.type === 'primaryKey' ? 'The primary key' : `Unique constraint ${k.name || '(unnamed)'}`;
    if (keys.some((x) => x.expression)) errors.push(`${what}: a partitioned table with an expression key cannot have primary key or unique constraints.`);
    else {
      const missing = columns.filter((c) => !k.columns.includes(c));
      if (missing.length > 0) errors.push(`${what} must include every partition key column (${missing.join(', ')}).`);
    }
  }
  // Unique indexes follow the same rule as unique constraints.
  for (const i of draft.indexes) {
    if (i.dropped || !i.unique) continue;
    const what = `Unique index ${i.name || '(unnamed)'}`;
    if (keys.some((x) => x.expression) || i.parts.some((p) => p.expression)) {
      errors.push(`${what}: a unique index on a partitioned table needs plain key columns, and an expression partition key allows none.`);
      continue;
    }
    const missing = columns.filter((c) => !i.parts.some((p) => p.text === c));
    if (missing.length > 0) errors.push(`${what} must include every partition key column (${missing.join(', ')}).`);
  }
  // Exclusion constraints: none before PostgreSQL 17; from 17 every key column compared with =.
  for (const k of draft.constraints) {
    if (k.dropped || k.type !== 'exclusion') continue;
    const what = `Exclusion constraint ${k.name || '(unnamed)'}`;
    if (serverMajor < 17) {
      errors.push(`${what}: PostgreSQL ${serverMajor} has no exclusion constraints on partitioned tables (17 and later do).`);
      continue;
    }
    const missing = columns.filter((c) => !k.exclusion.some((p) => !p.expression && p.text === c && p.operator.trim() === '='));
    if (keys.some((x) => x.expression) || missing.length > 0) errors.push(`${what} must compare every partition key column with = (${missing.join(', ') || 'expression keys are not allowed'}).`);
  }
  return errors;
}

/** The name of a new table's DEFAULT partition: `<table>_default`, the table part cut so the name fits PostgreSQL's 63 bytes. */
export function defaultPartitionName(table: string): string {
  const suffix = '_default';
  const chars = [...table];
  while (chars.length > 0 && new TextEncoder().encode(chars.join('') + suffix).length > 63) chars.pop();
  return chars.join('') + suffix;
}

/** The HASH moduli of partitions as `pg_get_expr` writes their bounds. */
export function hashModuli(bounds: readonly string[]): number[] {
  return bounds.flatMap((b) => {
    const m = /modulus\s+(\d+)/i.exec(b);
    return m?.[1] ? [Number(m[1])] : [];
  });
}

/**
 * Why a new partition bound would be refused, or `null`: NULL in a range
 * bound, a hash remainder outside the modulus, or a modulus that breaks
 * PostgreSQL's rule that every modulus is a factor of the next larger one.
 */
export function pgBoundProblem(bound: PgBoundDraft, existingModuli: readonly number[] = []): string | null {
  if (bound.kind === 'range') {
    const values = [...splitList(bound.from), ...splitList(bound.to)];
    if (values.some((v) => /^null$/i.test(v.trim()))) return 'A range bound cannot hold NULL; use MINVALUE or MAXVALUE, or a list partition for NULL.';
    if (splitList(bound.from).length !== splitList(bound.to).length) return 'FROM and TO need the same number of values.';
  }
  if (bound.kind === 'hash') {
    if (!Number.isInteger(bound.modulus) || bound.modulus < 1) return 'The modulus must be a whole number from 1 up.';
    if (!Number.isInteger(bound.remainder) || bound.remainder < 0 || bound.remainder >= bound.modulus) return 'The remainder must be from 0 to the modulus minus 1.';
    const all = [...new Set([...existingModuli, bound.modulus])].sort((a, b) => a - b);
    for (let n = 1; n < all.length; n += 1) {
      const smaller = all[n - 1] as number;
      const larger = all[n] as number;
      if (larger % smaller !== 0) return `Every hash modulus must be a factor of the next larger one: ${smaller} does not divide ${larger}. Use ${existingModuli.length > 0 ? 'a multiple or a factor of the existing moduli' : 'another modulus'}.`;
    }
  }
  return null;
}

/** `PARTITION BY` of the draft, or `null`. */
export function pgPartitionBy(by: PartitionByDraft): PgPartitionBy | null {
  if (!by.enabled) return null;
  return {
    strategy: by.strategy,
    keys: by.keys.filter((k) => k.text.trim() !== '').map((k) => ({ key: k.expression ? { expression: k.text.trim() } : { column: k.text.trim() } })),
  };
}

/** `CREATE TABLE` for a new table (PostgreSQL: then its indexes and column comments come with it). */
function createChanges(draft: TableDraft, serverMajor: number): DraftChanges {
  const errors = newTableErrors(draft, serverMajor);
  const name = draft.table.trim();
  const columns = draft.columns.filter((c) => !c.dropped);
  const constraints = draft.constraints.filter((k) => !k.dropped);
  const indexes = draft.indexes.filter((i) => !i.dropped);
  const changes: EngineChange[] = [];
  if (draft.engine === 'postgres') {
    const pgConstraints: PgTableConstraint[] = [];
    for (const k of constraints) {
      try {
        pgConstraints.push(pgConstraintDef(draft, k));
      } catch (e: unknown) {
        errors.push(e instanceof Error ? e.message : String(e));
      }
    }
    const props = draft.props;
    const inherits = pgParents(draft, props.inherits, errors);
    const partitionBy = pgPartitionBy(draft.partitionBy);
    changes.push({
      engine: 'postgres',
      change: {
        object: 'table',
        action: 'create',
        name: pgName(draft, name),
        columns: columns.map(pgColumnDef),
        constraints: pgConstraints,
        partitionBy,
        unlogged: props.unlogged,
        inherits,
        storageParams: props.storageParams.map((p) => ({ name: p.name.trim(), value: p.value.trim() })),
        tablespace: props.tablespace.trim() || null,
        comment: props.comment === '' ? null : props.comment,
      },
    });
    if (props.owner.trim()) {
      changes.push({ engine: 'postgres', change: { object: 'table', action: 'alter', name: pgName(draft, name), operations: [{ op: 'owner', owner: { kind: 'name', name: props.owner.trim() } }] } });
    }
    if (partitionBy && draft.partitionBy.defaultPartition && draft.partitionBy.strategy !== 'hash') {
      changes.push(pgCreatePartition(draft.schema, name, defaultPartitionName(name), { kind: 'default' }, null));
    }
    changes.push(...pgConstraintComments(draft, null, errors));
    for (const i of indexes) {
      changes.push({
        engine: 'postgres',
        change: {
          object: 'index',
          action: 'create',
          name: i.name.trim() || null,
          table: pgName(draft, name),
          unique: i.unique,
          nullsNotDistinct: i.unique && i.nullsNotDistinct,
          concurrently: false,
          method: pgIndexMethod(i.method),
          elements: pgIndexElements(i),
          include: i.include,
          storageParams: pgIndexParams(i),
          tablespace: i.tablespace,
          predicate: i.predicate.trim() || null,
        },
      });
    }
    return { changes, errors };
  }
  const myColumns: MyColumnDef[] = [];
  for (const c of columns) {
    try {
      myColumns.push(myColumnDef({ ...c, name: c.name.trim() }));
    } catch (e: unknown) {
      errors.push(`Column ${c.name}: ${e instanceof Error ? e.message : String(e)}`);
    }
  }
  const myConstraints: MyTableConstraint[] = [];
  for (const k of constraints) {
    try {
      myConstraints.push(myConstraintDef(draft, k));
    } catch (e: unknown) {
      errors.push(e instanceof Error ? e.message : String(e));
    }
  }
  changes.push({
    engine: 'mysql',
    change: {
      change: 'createTable',
      database: draft.database,
      name,
      columns: myColumns,
      constraints: myConstraints,
      indexes: indexes.map(myIndexDef),
      options: myOptions(draft, null, errors),
    },
  });
  return { changes, errors };
}

/** Moves a column up (-1) or down (+1) in a MySQL / MariaDB draft. PostgreSQL cannot reorder columns. */
export function moveColumn(draft: TableDraft, id: string, by: -1 | 1): void {
  const at = draft.columns.findIndex((c) => c.id === id);
  const to = at + by;
  if (at < 0 || to < 0 || to >= draft.columns.length) return;
  const [c] = draft.columns.splice(at, 1);
  if (!c) return;
  c.moved = true;
  draft.columns.splice(to, 0, c);
}

// ---------------------------------------------------------------------------
// Single actions: maintenance and partitions
// ---------------------------------------------------------------------------

/** `ALTER TABLE ... VALIDATE CONSTRAINT`. */
export function pgValidateConstraint(schema: string, table: string, name: string): EngineChange {
  return { engine: 'postgres', change: { object: 'table', action: 'alter', name: { schema, name: table }, operations: [{ op: 'validateConstraint', name }] } };
}

/** `REINDEX INDEX [CONCURRENTLY]`. */
export function pgReindex(schema: string, index: string, concurrently: boolean): EngineChange {
  return { engine: 'postgres', change: { object: 'index', action: 'reindex', target: { kind: 'index', name: { schema, name: index } }, concurrently } };
}

/** One value of a PostgreSQL partition bound as typed: MINVALUE, MAXVALUE, NULL, or a literal. */
export function pgBoundValue(text: string): PgBoundValue {
  const t = text.trim();
  if (/^minvalue$/i.test(t)) return { kind: 'minValue' };
  if (/^maxvalue$/i.test(t)) return { kind: 'maxValue' };
  if (/^null$/i.test(t)) return { kind: 'null' };
  const quoted = /^'([\s\S]*)'$/.exec(t);
  return { kind: 'literal', value: quoted?.[1] !== undefined ? quoted[1].replace(/''/g, "'") : t };
}

/** Splits a list typed as `a, 'b, c', d` at the top-level commas. */
export function splitList(text: string): string[] {
  const out: string[] = [];
  let cur = '';
  let quote = false;
  let depth = 0;
  for (let i = 0; i < text.length; i += 1) {
    const c = text[i] as string;
    if (c === "'") quote = !quote;
    if (!quote && c === '(') depth += 1;
    if (!quote && c === ')') depth -= 1;
    if (c === ',' && !quote && depth === 0) {
      out.push(cur.trim());
      cur = '';
    } else cur += c;
  }
  if (cur.trim() !== '') out.push(cur.trim());
  return out;
}

/** What a new PostgreSQL partition holds. */
export type PgBoundDraft =
  | { kind: 'range'; from: string; to: string }
  | { kind: 'list'; values: string }
  | { kind: 'hash'; modulus: number; remainder: number }
  | { kind: 'default' };

function pgBound(bound: PgBoundDraft) {
  switch (bound.kind) {
    case 'range':
      return { kind: 'range' as const, from: splitList(bound.from).map(pgBoundValue), to: splitList(bound.to).map(pgBoundValue) };
    case 'list':
      return { kind: 'list' as const, values: splitList(bound.values).map(pgBoundValue) };
    case 'hash':
      return { kind: 'hash' as const, modulus: bound.modulus, remainder: bound.remainder };
    case 'default':
      return { kind: 'default' as const };
  }
}

/** `CREATE TABLE <name> PARTITION OF <parent> <bound>`. */
export function pgCreatePartition(schema: string, parent: string, name: string, bound: PgBoundDraft, tablespace: string | null): EngineChange {
  return {
    engine: 'postgres',
    change: {
      object: 'table',
      action: 'create',
      name: { schema, name },
      partitionOf: { parent: { schema, name: parent }, bound: pgBound(bound) },
      tablespace,
    },
  };
}

/** `ALTER TABLE <parent> ATTACH PARTITION <table> <bound>`. */
export function pgAttachPartition(schema: string, parent: string, partition: PgName, bound: PgBoundDraft): EngineChange {
  return {
    engine: 'postgres',
    change: { object: 'table', action: 'alter', name: { schema, name: parent }, operations: [{ op: 'attachPartition', partition, bound: pgBound(bound) }] },
  };
}

/** `ALTER TABLE <parent> DETACH PARTITION <partition> [CONCURRENTLY]`, optionally followed by dropping it. */
export function pgDetachPartition(schema: string, parent: string, partition: PgName, concurrently: boolean, drop: boolean): EngineChange[] {
  const out: EngineChange[] = [
    {
      engine: 'postgres',
      change: {
        object: 'table',
        action: 'alter',
        name: { schema, name: parent },
        operations: [{ op: 'detachPartition', partition, mode: concurrently ? 'concurrently' : 'normal' }],
      },
    },
  ];
  if (drop) out.push({ engine: 'postgres', change: { object: 'table', action: 'drop', name: partition } });
  return out;
}

/** A parsed single-column range bound: `FOR VALUES FROM ('2026-10-01') TO ('2026-11-01')`. */
export interface DateRange {
  from: string;
  to: string;
  /** What follows the date inside the quotes (` 00:00:00+00`), kept for new bounds. */
  suffix: string;
}

/** Reads a single-column date range bound as `pg_get_expr` writes it; `null` for anything else. */
export function parseDateRange(bound: string): DateRange | null {
  const m = /^FOR VALUES FROM \('(\d{4}-\d{2}-\d{2})([^']*)'\) TO \('(\d{4}-\d{2}-\d{2})([^']*)'\)$/.exec(bound.trim());
  if (!m?.[1] || !m[3]) return null;
  return { from: m[1], to: m[3], suffix: m[2] ?? '' };
}

/** Partition interval of "Create next partitions". */
export type PartitionInterval = 'month' | 'week' | 'day';

function addInterval(date: string, interval: PartitionInterval): string {
  const [y, m, d] = date.split('-').map(Number) as [number, number, number];
  const at = new Date(Date.UTC(y, m - 1, d));
  if (interval === 'month') at.setUTCMonth(at.getUTCMonth() + 1);
  else at.setUTCDate(at.getUTCDate() + (interval === 'week' ? 7 : 1));
  return at.toISOString().slice(0, 10);
}

/** A partition "Create next partitions" proposes. */
export interface NextPartition {
  name: string;
  from: string;
  to: string;
}

/**
 * The next `count` range partitions after the last one: each starts where
 * the previous ends and spans `interval`. `pattern` names them with
 * `{YYYY}`, `{MM}` and `{DD}` of the start date.
 */
export function nextRangePartitions(bounds: readonly string[], count: number, interval: PartitionInterval, pattern: string): NextPartition[] {
  const ranges = bounds.map(parseDateRange).filter((r): r is DateRange => r !== null);
  if (ranges.length === 0 || count < 1) return [];
  const last = ranges.reduce((a, b) => (b.to > a.to ? b : a));
  const out: NextPartition[] = [];
  let from = last.to;
  for (let n = 0; n < count; n += 1) {
    const to = addInterval(from, interval);
    const [yyyy, mm, dd] = from.split('-') as [string, string, string];
    out.push({
      name: pattern.replaceAll('{YYYY}', yyyy).replaceAll('{MM}', mm).replaceAll('{DD}', dd),
      from: `'${from}${last.suffix}'`,
      to: `'${to}${last.suffix}'`,
    });
    from = to;
  }
  return out;
}

/** One value of a MySQL partition bound as typed: MAXVALUE, a number, a quoted string, or an expression. */
export function myBoundValue(text: string): MyPartitionBound {
  const t = text.trim();
  if (/^maxvalue$/i.test(t)) return { kind: 'maxValue' };
  if (/^[+-]?\d+$/.test(t)) {
    const n = Number(t);
    return { kind: 'value', value: Number.isSafeInteger(n) ? { t: 'int', v: n } : { t: 'decimal', v: t.replace(/^\+/, '') } };
  }
  const quoted = /^'([\s\S]*)'$/.exec(t);
  if (quoted?.[1] !== undefined) return { kind: 'value', value: { t: 'text', v: quoted[1].replace(/''/g, "'") } };
  if (/^null$/i.test(t)) return { kind: 'value', value: { t: 'null' } };
  return { kind: 'expression', sql: t };
}

/**
 * The values of a MySQL / MariaDB range or list partition as typed: a
 * comma-separated list for `VALUES LESS THAN (...)` (range) or
 * `VALUES IN (...)` (list, one value per row). `null` for hash and key
 * partitioning, which take no values.
 */
export function myPartitionValues(strategy: string, text: string): MyPartitionValues | null {
  const s = strategy.toLowerCase();
  const values = splitList(text).filter((v) => v !== '').map(myBoundValue);
  if (s.startsWith('range')) return { kind: 'lessThan', bounds: values };
  if (s.startsWith('list')) return { kind: 'in', rows: values.map((v) => [v]) };
  return null;
}

/** One new partition of a REORGANIZE PARTITION, as the Partitions section edits it. */
export interface MyReorganizeTarget {
  name: string;
  /** Range: the LESS THAN bounds; list: the IN values (comma-separated). */
  values: string;
}

/**
 * One LIST value as a comparable key: integers without sign or leading
 * zeros, quoted text with its quotes unescaped and written one way, NULL in
 * capitals, tuples (LIST COLUMNS) element by element, anything else as
 * typed without spaces. `1`, `+01` and `001` are the same value; `'a'` and
 * `a` are not.
 */
export function myListValueKey(text: string): string {
  const t = text.trim();
  const tuple = /^\(([\s\S]*)\)$/.exec(t);
  if (tuple?.[1] !== undefined) return `(${splitList(tuple[1]).map(myListValueKey).join(',')})`;
  if (/^[+-]?\d+$/.test(t)) return BigInt(t).toString();
  const quoted = /^'([\s\S]*)'$/.exec(t);
  if (quoted?.[1] !== undefined) return `'${quoted[1].replace(/''/g, "'").replace(/'/g, "''")}'`;
  if (/^null$/i.test(t)) return 'NULL';
  return t.replace(/\s+/g, '');
}

/** A partition REORGANIZE replaces, as the catalog describes it (`description`: its values, `1,2,3` or `MAXVALUE`). */
export interface MyReorganizeSource {
  name: string;
  description: string | null;
}

/**
 * `REORGANIZE PARTITION names INTO (...)` of a range or list partitioned
 * table (split one partition, merge adjacent ones, or move a bound), or
 * the reasons it cannot be planned. `old` are the partitions it replaces.
 *
 * RANGE: the server refuses new partitions that do not cover the old range.
 * LIST: the server silently deletes the rows whose value no new partition
 * lists, so every value of the old partitions must be in a new one; the
 * values that would be lost are named. A MariaDB DEFAULT partition holds
 * values no list names, so it is not reorganized here.
 */
export function myReorganize(
  strategy: string,
  names: readonly string[],
  into: readonly MyReorganizeTarget[],
  old: readonly MyReorganizeSource[],
): { op: MyAlterOp | null; errors: string[] } {
  const errors: string[] = [];
  const s = strategy.toLowerCase();
  const list = s.startsWith('list');
  if (!s.startsWith('range') && !list) errors.push('Reorganize splits or merges range and list partitions; hash and key partitions are added or coalesced.');
  if (names.length === 0) errors.push('Choose the partitions to reorganize.');
  if (into.length === 0) errors.push('Give at least one new partition.');
  const seen = new Set<string>();
  const defs: MyPartitionDef[] = [];
  for (const [i, t] of into.entries()) {
    const name = t.name.trim();
    if (!name) errors.push(`New partition ${i + 1}: give it a name.`);
    else if (seen.has(name.toLowerCase())) errors.push(`Two new partitions are called ${name}.`);
    seen.add(name.toLowerCase());
    if (!t.values.trim()) errors.push(`New partition ${name || i + 1}: give its ${s.startsWith('range') ? 'LESS THAN bound' : 'IN values'}.`);
    defs.push({ name, values: myPartitionValues(s, t.values) });
  }
  if (list) {
    const replaced = names.map((n) => old.find((o) => o.name === n));
    const unknown = names.filter((_, i) => (replaced[i]?.description ?? null) === null);
    const defaults = replaced.filter((o) => o?.description?.trim().toUpperCase() === 'DEFAULT').map((o) => o?.name ?? '');
    if (defaults.length > 0) errors.push(`Partition ${defaults.join(', ')} is the DEFAULT partition; reorganize it in a query.`);
    else if (unknown.length > 0) errors.push(`The values of partition ${unknown.join(', ')} are not known; reload the table.`);
    else {
      const kept = new Set(into.flatMap((t) => splitList(t.values).filter((v) => v !== '').map(myListValueKey)));
      const lost: string[] = [];
      for (const o of replaced) {
        for (const v of splitList(o?.description ?? '').filter((x) => x !== '')) {
          if (!kept.has(myListValueKey(v)) && !lost.includes(v.trim())) lost.push(v.trim());
        }
      }
      if (lost.length > 0) {
        errors.push(`Rows with the values ${lost.join(', ')} would be deleted: no new partition lists them. Add them to a new partition.`);
      }
    }
  }
  return errors.length > 0 ? { op: null, errors } : { op: { op: 'reorganizePartitions', names: [...names], into: defs }, errors };
}

/** `SUBPARTITION BY` as the partition-by form edits it: none, or HASH (expression) / KEY (columns) with a count. */
export function mySubpartitioning(kind: 'none' | 'hash' | 'key', text: string, count: number): MySubPartitioning | null {
  if (kind === 'none') return null;
  const method = kind === 'hash' ? { kind, expression: text.trim() } : { kind, columns: splitList(text).filter((c) => c !== '') };
  return { method, count: Number.isInteger(count) && count > 0 ? count : null };
}

/** A MySQL / MariaDB ALTER TABLE with one partition operation. */
export function myPartitionChange(database: string, table: string, op: MyAlterOp, systemVersioned = false): EngineChange {
  return { engine: 'mysql', change: { change: 'alterTable', database, name: table, operations: [op], systemVersioned } };
}
