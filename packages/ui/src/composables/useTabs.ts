import { computed, markRaw, reactive, ref, watch } from 'vue';
import { displayName, kindLabel, objectIdentity } from '../api/catalog';
import type { Container, ObjectKind, ObjectRef } from '../api/catalog';
import { driverErrorOf, toCmdError } from '../api/errors';
import type { CmdError, DriverError } from '../api/errors';
import type { ConnectionProfile, SavedQuery } from '../api/records';
import { redisSelectDb } from '../api/redis';
import { cancel, closeSession, execute, openDatabaseSession, openSession } from '../api/sessions';
import type { ExecOutput, SessionInfo } from '../api/sessions';
import { isLostSession } from '../lib/connectFlow';
import { partialAsOutput } from '../lib/partialRun';
import { KeyIndex } from '../lib/keyIndex';
import type { KeyIndexApi } from '../lib/keyIndex';
import { runWithConfirmation } from '../lib/destructive';
import { selectedDbAfterRun } from '../lib/redisMode';
import { LIVE_PANEL_LABELS } from '../lib/tree';
import type { LivePanel } from '../lib/tree';
import { withConnectPrompts } from './useConnect';
import { useDialogs } from './useDialogs';
import { useToasts } from './useToasts';
import { useWorkspace } from './useWorkspace';

export type TabStatus = 'connecting' | 'ready' | 'cancelled' | 'error';

/** The last run of a query tab. Stored with `markRaw`: result rows are never deeply reactive. */
export interface RunResult {
  /** The results; for a failed script, those of the statements that completed before the failure. */
  output: ExecOutput | null;
  error: DriverError | CmdError | null;
  /** UTF-16 offset of the submitted text in the editor, to place error positions. */
  offset: number;
  /** The submitted text; error positions count characters in it. */
  text: string;
  /** The whole editor text when the run started, for line and column of an error. */
  doc: string;
  /** Identity of this run. */
  id: number;
  startedAt: number;
}

interface TabBase {
  id: string;
  connectionId: string;
  title: string;
  status: TabStatus;
  session: SessionInfo | null;
  connectError: CmdError | null;
  /** Redis logical database of the session; `null` for SQL engines. */
  db: number | null;
  /**
   * PostgreSQL database the tab's session is opened on (`open_database_session`);
   * `null` for the profile's own database and for the other engines.
   */
  database: string | null;
  /**
   * Set when a call answered `reconnectRequired`: the connection was edited
   * since the tab connected. The tab offers "Reconnect"; its session keeps
   * working for editors, grid and EXPLAIN until then.
   */
  reconnectNeeded: CmdError | null;
  /**
   * Editors (object, designer, DDL, key editors) set this through
   * `setUnsaved` while they hold changes that are not applied; the tab shows
   * the unsaved dot. Query tabs use their text instead (`useQuerySave`).
   */
  unsaved: boolean;
}

export interface QueryTab extends TabBase {
  kind: 'query';
  sql: string;
  savedQuery: SavedQuery | null;
  running: boolean;
  run: RunResult | null;
  /** Bumped when the editor text is replaced from outside (history, saved query). */
  revision: number;
}

export interface KeysTab extends TabBase {
  kind: 'keys';
  /** The submitted scan: "load more" continues with these, not with edited form fields. */
  pattern: string;
  typeFilter: string;
  count: number;
  delimiter: string;
  /** Keys found so far, grouped incrementally (`markRaw`; `keysVersion` signals changes). */
  index: KeyIndexApi;
  keysVersion: number;
  /** True when the scan stopped at the key cap. */
  capped: boolean;
  /** SCAN cursor as decimal text; `null` before the first page, `"0"` when complete. */
  cursor: string | null;
  scanned: boolean;
  selected: string | null;
  /** `sessionId:db` the keys, cursor and selection belong to; anything else must not use them. */
  scanFor: string | null;
  /** Bumped for every SCAN request; only the answer with the current value may write the keys. */
  scanToken: number;
}

/** What an object tab is for. */
export type ObjectIntent = 'view' | 'edit' | 'create' | 'drop';

/**
 * Details and editor of one object (U2, components/objects). `object` is
 * `null` while creating: then `createKind` and `container` say what and where.
 */
export interface ObjectTab extends TabBase {
  kind: 'object';
  object: ObjectRef | null;
  createKind: ObjectKind | null;
  container: Container;
  intent: ObjectIntent;
}

/** The DDL of one object, viewed, edited and run (U2, components/objects). */
export interface DdlTab extends TabBase {
  kind: 'ddl';
  object: ObjectRef;
}

/** The rows of a table or view, editable where it has a key (U3, components/table). */
export interface TableDataTab extends TabBase {
  kind: 'tableData';
  object: ObjectRef;
}

/** The table designer (U3, components/table); `object` is `null` for a new table in `container`. */
export interface TableDesignerTab extends TabBase {
  kind: 'tableDesigner';
  object: ObjectRef | null;
  container: Container;
}

/** EXPLAIN of a statement on the tab's own session (U4, components/explain). */
export interface ExplainTab extends TabBase {
  kind: 'explain';
  statement: string;
}

/** A live server panel, run on the profile's admin session (U4, components/live). */
export interface LiveTab extends TabBase {
  kind: 'live';
  panel: LivePanel;
}

/** The editor of one Redis key in database `db` (U5, components/redis). */
export interface RedisKeyTab extends TabBase {
  kind: 'redisKey';
  key: string;
}

/** PostgreSQL LISTEN / NOTIFY panel of the tab's own session (B2, components/notify). */
export interface NotifyTab extends TabBase {
  kind: 'notify';
}

/** Redis Pub/Sub panel: subscribe and publish (U5, components/redis). */
export interface PubSubTab extends TabBase {
  kind: 'pubsub';
}

/**
 * The objects of one container as a table (components/overview, U1).
 * `scope` is what the pickers show: the schema for PostgreSQL (the database
 * is the tab session's), the database for MySQL and MariaDB; `null` until
 * the view picks the default.
 */
export interface OverviewTab extends TabBase {
  kind: 'overview';
  scope: { database: string | null; schema: string | null };
}

export type WorkspaceTab =
  | OverviewTab
  | QueryTab
  | KeysTab
  | ObjectTab
  | DdlTab
  | TableDataTab
  | TableDesignerTab
  | ExplainTab
  | LiveTab
  | RedisKeyTab
  | NotifyTab
  | PubSubTab;

export type TabKind = WorkspaceTab['kind'];

/** Every tab kind; `WorkspaceView` has a view (or a placeholder) for each. */
export const TAB_KINDS: readonly TabKind[] = [
  'overview',
  'query',
  'keys',
  'object',
  'ddl',
  'tableData',
  'tableDesigner',
  'explain',
  'live',
  'redisKey',
  'notify',
  'pubsub',
];

/**
 * What makes two tabs the same, so opening it again activates the open one;
 * `null` for tabs that are always opened anew (new queries, new objects).
 */
export function tabIdentity(tab: WorkspaceTab): string | null {
  const scope = `${tab.kind}|${tab.connectionId}`;
  switch (tab.kind) {
    case 'overview':
      // PostgreSQL: one per database (each has its own session); MySQL and MariaDB: one per connection.
      return `${scope}|${tab.database ?? ''}`;
    case 'query':
      return tab.savedQuery ? `${scope}|${tab.savedQuery.id}` : null;
    case 'keys':
    case 'notify':
    case 'pubsub':
      return scope;
    case 'object':
      return tab.object ? `${scope}|${objectIdentity(tab.object)}` : null;
    case 'ddl':
    case 'tableData':
      return `${scope}|${objectIdentity(tab.object)}`;
    case 'tableDesigner':
      return tab.object ? `${scope}|${objectIdentity(tab.object)}` : null;
    case 'explain':
      return null;
    case 'live':
      return `${scope}|${tab.panel}`;
    case 'redisKey':
      return `${scope}|${tab.db ?? 0}|${tab.key}`;
  }
}

const tabs = reactive<WorkspaceTab[]>([]);
const activeId = ref<string | null>(null);
let nextId = 1;

const active = computed<WorkspaceTab | null>(() => tabs.find((t) => t.id === activeId.value) ?? null);

function makeId(): string {
  const id = `tab-${nextId}`;
  nextId += 1;
  return id;
}

function find(id: string): WorkspaceTab | undefined {
  return tabs.find((t) => t.id === id);
}

let runIds = 0;

/** The newest connect attempt per tab; an older attempt that finishes later is discarded. */
const connecting = new Map<string, number>();

/** Opens the session of a tab (one session per tab), asking for secrets as needed. */
async function connect(id: string): Promise<void> {
  const tab = find(id);
  if (!tab) return;
  const profile = useWorkspace().connection(tab.connectionId);
  if (!profile) {
    tab.status = 'error';
    tab.connectError = { kind: 'notFound', message: 'This connection no longer exists.' };
    return;
  }
  tab.status = 'connecting';
  tab.connectError = null;
  const generation = (connecting.get(id) ?? 0) + 1;
  connecting.set(id, generation);
  try {
    const database = tab.database;
    const info = await withConnectPrompts(profile, (secrets) =>
      database !== null ? openDatabaseSession(profile.id, database, secrets) : openSession(profile.id, secrets),
    );
    const stale = connecting.get(id) !== generation;
    const current = stale ? undefined : find(id);
    if (!info) {
      if (current) current.status = 'cancelled';
      return;
    }
    if (!current) {
      // The tab closed (or reconnected) while this attempt was running.
      void closeSession(info.id).catch(() => undefined);
      return;
    }
    // A new session starts on the profile's database. Restore the database the
    // user had switched to, before anything else uses the session.
    let db: number | null = profile.engine === 'redis' ? profile.options.redisDb : null;
    const wanted = current.db;
    if (db !== null && wanted !== null && wanted !== db) {
      try {
        await redisSelectDb(info.id, wanted);
        db = wanted;
      } catch {
        // Fall back to the profile's database (the keys below are reset either way) and say so.
        useToasts().push('warning', `Could not switch back to db${wanted}`, `This tab now uses db${db}, the database in the connection settings.`);
      }
      if (connecting.get(id) !== generation || !find(id)) {
        void closeSession(info.id).catch(() => undefined);
        return;
      }
    }
    current.session = info;
    current.db = db;
    current.reconnectNeeded = null;
    if (current.kind === 'keys') resetKeys(current);
    current.status = 'ready';
    if (current.database === null) rememberDefaultDatabase(profile, info.database);
    if (current.kind === 'overview' && info.engine === 'postgres') mergeOverview(current);
  } catch (error: unknown) {
    const current = connecting.get(id) === generation ? find(id) : undefined;
    if (current) {
      current.status = 'error';
      current.connectError = toCmdError(error);
    }
  }
}

function add(tab: WorkspaceTab): WorkspaceTab {
  const identity = tabIdentity(tab);
  const open = identity === null ? undefined : tabs.find((t) => tabIdentity(t) === identity);
  if (open) {
    activeId.value = open.id;
    return open;
  }
  tabs.push(tab);
  activeId.value = tab.id;
  const added = tabs[tabs.length - 1] as WorkspaceTab;
  void connect(added.id);
  return added;
}

/** The default database of each connection version, learnt from sessions opened without naming one. */
const defaultDatabases = new Map<string, string>();

function defaultKey(profile: Pick<ConnectionProfile, 'id' | 'version'>): string {
  return `${profile.id}@${profile.version}`;
}

/**
 * Records the database a session opened without naming one landed on
 * (PostgreSQL `current_database()`), so "the default database" and that
 * database by name are known to be the same. Tabs and the object tree call this.
 */
export function rememberDefaultDatabase(profile: Pick<ConnectionProfile, 'id' | 'version'>, database: string | null): void {
  if (database !== null) defaultDatabases.set(defaultKey(profile), database);
}

/** The default database remembered for this version of a connection, if any (not the profile's own). */
export function knownDefaultDatabase(profile: Pick<ConnectionProfile, 'id' | 'version'>): string | null {
  return defaultDatabases.get(defaultKey(profile)) ?? null;
}

/** The database a session of the profile opened without naming one lands on, when known. */
export function defaultDatabaseOf(profile: ConnectionProfile): string | null {
  return profile.database ?? defaultDatabases.get(defaultKey(profile)) ?? null;
}

/** The PostgreSQL database an overview tab shows: the one it names, else where its session landed, else the default. */
function overviewOn(tab: OverviewTab, profile: ConnectionProfile): string | null {
  return tab.database ?? tab.session?.database ?? defaultDatabaseOf(profile);
}

/**
 * After a PostgreSQL overview connected: when another overview of the same
 * connection already shows the database it landed on (one opened from the
 * connection, the other from the tree's database node, before either knew
 * the default database), only the other one stays.
 */
function mergeOverview(tab: OverviewTab): void {
  const profile = useWorkspace().connection(tab.connectionId);
  if (!profile) return;
  const database = overviewOn(tab, profile);
  if (database === null) return;
  const other = tabs.find(
    (t): t is OverviewTab => t.kind === 'overview' && t.id !== tab.id && t.connectionId === tab.connectionId && overviewOn(t, profile) === database,
  );
  if (!other) return;
  // Keep the tab that works best: ready before connecting before failed or cancelled; on a tie
  // the one that was there first (the other) stays.
  const rank = (t: OverviewTab): number => (t.status === 'ready' ? 2 : t.status === 'connecting' ? 1 : 0);
  const keepOther = rank(other) >= rank(tab);
  const keep = keepOther ? other : tab;
  const drop = keepOther ? tab : other;
  if (activeId.value === drop.id) activeId.value = keep.id;
  if (drop.scope.schema !== null && keep.scope.schema === null) keep.scope.schema = drop.scope.schema;
  void closeTab(drop.id);
}

/** The database a tab's session must be opened on: only PostgreSQL browses other databases through their own sessions. */
function sessionDatabase(profile: ConnectionProfile, database: string | null): string | null {
  return profile.engine === 'postgres' ? database : null;
}

/** Fields every tab starts with. */
function base(profile: ConnectionProfile, title: string, database: string | null, db: number | null = null): TabBase {
  return {
    id: makeId(),
    connectionId: profile.id,
    title,
    status: 'connecting',
    session: null,
    connectError: null,
    db: profile.engine === 'redis' ? (db ?? profile.options.redisDb) : null,
    database: sessionDatabase(profile, database),
    reconnectNeeded: null,
    unsaved: false,
  };
}

/**
 * Opens a query (or Redis console) tab on a connection. `database` opens a
 * PostgreSQL tab on another database than the profile's.
 */
function openQueryTab(profile: ConnectionProfile, saved: SavedQuery | null = null, sql = '', database: string | null = null): WorkspaceTab {
  // An open tab of the same saved query is activated by `add` (`tabIdentity`).
  const sameConnection = tabs.filter((t) => t.connectionId === profile.id && t.kind === 'query').length;
  const onDatabase = sessionDatabase(profile, database);
  const label = profile.engine === 'redis' ? `Console · ${profile.name}` : onDatabase !== null ? `${profile.name} · ${onDatabase}` : profile.name;
  return add({
    ...base(profile, saved ? saved.name : sameConnection > 0 ? `${label} (${sameConnection + 1})` : label, database),
    kind: 'query',
    sql: saved ? saved.sql : sql,
    savedQuery: saved,
    running: false,
    run: null,
    revision: 0,
  });
}

/**
 * Opens the key browser of a Redis connection. With `db`, the browser shows
 * that database (an open browser switches to it).
 */
function openKeysTab(profile: ConnectionProfile, db: number | null = null): WorkspaceTab {
  const open = tabs.find((t) => t.kind === 'keys' && t.connectionId === profile.id);
  if (open) {
    activeId.value = open.id;
    if (db !== null && open.db !== db) {
      if (open.status === 'ready') {
        selectDb(open.id, db).catch((error: unknown) => {
          useToasts().pushError(error, `Could not switch to db${db}`);
        });
      } else {
        // The connect in progress (or the next one) switches its new session to this database.
        open.db = db;
        if (open.kind === 'keys') resetKeys(open);
      }
    }
    return open;
  }
  const startDb = db ?? profile.options.redisDb;
  return add({
    kind: 'keys',
    id: makeId(),
    connectionId: profile.id,
    title: `Keys · ${profile.name} db${startDb}`,
    status: 'connecting',
    session: null,
    connectError: null,
    db: startDb,
    database: null,
    reconnectNeeded: null,
    unsaved: false,
    pattern: '*',
    typeFilter: '',
    delimiter: ':',
    count: 1000,
    index: markRaw(new KeyIndex(':')),
    keysVersion: 0,
    capped: false,
    cursor: null,
    scanned: false,
    selected: null,
    scanFor: null,
    scanToken: 0,
  });
}

/** Opens the details (or editor) of an object, or the editor for a new one. */
function openObjectTab(
  profile: ConnectionProfile,
  options: { object: ObjectRef; intent?: Exclude<ObjectIntent, 'create'> } | { createKind: ObjectKind; container: Container },
): WorkspaceTab {
  if ('object' in options) {
    const object = options.object;
    const tab = add({
      // A database is edited from a session on the profile's own database: PostgreSQL cannot
      // drop or rename the database a session is connected to (the editor offers another one
      // when the profile's database is the one being changed).
      ...base(profile, displayName(object), object.kind === 'database' ? null : object.database),
      kind: 'object',
      object,
      createKind: null,
      container: { database: object.database, schema: object.schema, parent: null },
      intent: options.intent ?? 'view',
    });
    // An open tab of the object switches to what was asked for now.
    if (tab.kind === 'object') tab.intent = options.intent ?? tab.intent;
    return tab;
  }
  return add({
    ...base(profile, `New ${kindLabel(options.createKind).toLocaleLowerCase()}`, options.container.database),
    kind: 'object',
    object: null,
    createKind: options.createKind,
    container: options.container,
    intent: 'create',
  });
}

/**
 * Opens the overview of a connection's objects. `database` (PostgreSQL:
 * opens on that database; MySQL and MariaDB: shows it) and `schema`
 * (PostgreSQL) choose what it shows; an open overview switches to them.
 */
function openOverviewTab(profile: ConnectionProfile, where: { database?: string | null; schema?: string | null } = {}): WorkspaceTab {
  const database = where.database ?? null;
  const pg = profile.engine === 'postgres';
  if (pg) {
    // The overview of the default database (opened from the connection, `database` null) and of
    // the same database named from the tree are one tab: compare the database each one is on.
    // While the default database is not known yet, a match is found after connecting (`mergeOverview`).
    const wanted = database ?? defaultDatabaseOf(profile);
    const open = tabs.find(
      (t): t is OverviewTab =>
        t.kind === 'overview' &&
        t.connectionId === profile.id &&
        (wanted === null ? t.database === null : overviewOn(t, profile) === wanted),
    );
    if (open) {
      activeId.value = open.id;
      if (where.schema) open.scope.schema = where.schema;
      return open;
    }
  }
  const scope = { database: pg ? null : database, schema: pg ? (where.schema ?? null) : null };
  const title = pg && database !== null ? `${database} · ${profile.name}` : profile.name;
  const tab = add({ ...base(profile, title, database), kind: 'overview', scope });
  if (tab.kind === 'overview') {
    if (!pg && database !== null) tab.scope.database = database;
    if (pg && where.schema) tab.scope.schema = where.schema;
  }
  return tab;
}

function openDdlTab(profile: ConnectionProfile, object: ObjectRef): WorkspaceTab {
  return add({ ...base(profile, `${displayName(object)} · DDL`, object.database), kind: 'ddl', object });
}

function openTableDataTab(profile: ConnectionProfile, object: ObjectRef): WorkspaceTab {
  return add({ ...base(profile, displayName(object), object.database), kind: 'tableData', object });
}

/** Opens the table designer for a table, or for a new table in `container`. */
function openTableDesigner(profile: ConnectionProfile, object: ObjectRef | null, container: Container): WorkspaceTab {
  const title = object ? `${displayName(object)} · Design` : 'New table';
  return add({ ...base(profile, title, object?.database ?? container.database), kind: 'tableDesigner', object, container });
}

function openExplainTab(profile: ConnectionProfile, statement: string, database: string | null = null): WorkspaceTab {
  return add({ ...base(profile, `Explain · ${profile.name}`, database), kind: 'explain', statement });
}

function openLiveTab(profile: ConnectionProfile, panel: LivePanel): WorkspaceTab {
  return add({ ...base(profile, `${LIVE_PANEL_LABELS[panel]} · ${profile.name}`, null), kind: 'live', panel });
}

function openRedisKeyTab(profile: ConnectionProfile, key: string, db: number): WorkspaceTab {
  return add({ ...base(profile, key === '' ? '(empty key name)' : key, null, db), kind: 'redisKey', key });
}

function openNotifyTab(profile: ConnectionProfile, database: string | null = null): WorkspaceTab {
  return add({ ...base(profile, `Notifications · ${profile.name}`, database), kind: 'notify' });
}

function openPubSubTab(profile: ConnectionProfile): WorkspaceTab {
  return add({ ...base(profile, `Pub/Sub · ${profile.name}`, null), kind: 'pubsub' });
}

/**
 * Handles an error from a call made on a tab's session and returns it parsed
 * for display: a lost session marks the tab lost (it offers "Connect again"),
 * `reconnectRequired` makes it offer "Reconnect". Views call this from every
 * catch so all tabs react the same way.
 */
function reportTabError(id: string, error: unknown): CmdError {
  const cmd = toCmdError(error);
  if (isLostSession(cmd)) markLost(id, cmd);
  else if (cmd.kind === 'reconnectRequired') {
    const tab = find(id);
    if (tab) tab.reconnectNeeded = cmd;
  }
  return cmd;
}

/**
 * Points an object or table designer tab at the object it just created (it
 * was opened for a new object), so its title and identity follow.
 */
function setTabObject(id: string, object: ObjectRef): void {
  const tab = find(id);
  if (!tab || (tab.kind !== 'object' && tab.kind !== 'tableDesigner')) return;
  tab.object = object;
  if (tab.kind === 'object') {
    tab.createKind = null;
    tab.intent = 'view';
    tab.title = displayName(object);
  } else tab.title = `${displayName(object)} · Design`;
}

/** Changes what an overview tab shows (its picker): the schema (PostgreSQL) or database (MySQL, MariaDB). */
function setOverviewScope(id: string, scope: Partial<OverviewTab['scope']>): void {
  const tab = find(id);
  if (tab?.kind !== 'overview') return;
  if (scope.database !== undefined) tab.scope.database = scope.database;
  if (scope.schema !== undefined) tab.scope.schema = scope.schema;
}

/** Marks an editor tab as holding (or no longer holding) changes that are not applied. */
function setUnsaved(id: string, unsaved: boolean): void {
  const tab = find(id);
  if (tab) tab.unsaved = unsaved;
}

/** Closes a tab and its session. */
async function closeTab(id: string): Promise<void> {
  const at = tabs.findIndex((t) => t.id === id);
  if (at < 0) return;
  const [tab] = tabs.splice(at, 1);
  // A connect still running for this tab sees it is gone and closes its session.
  connecting.delete(id);
  if (activeId.value === id) {
    const next = tabs[Math.min(at, tabs.length - 1)];
    activeId.value = next ? next.id : null;
  }
  if (tab?.session) {
    const sessionId = tab.session.id;
    if (tab.kind === 'query' && tab.running) await cancel(sessionId).catch(() => undefined);
    await closeSession(sessionId).catch(() => undefined);
  }
}

/** Closes every tab of a connection, for example after it was deleted. */
async function closeConnectionTabs(connectionId: string): Promise<void> {
  for (const tab of tabs.filter((t) => t.connectionId === connectionId)) await closeTab(tab.id);
}

/** Opens a new session for a tab whose session failed or closed. */
async function reconnect(id: string): Promise<void> {
  const tab = find(id);
  if (!tab) return;
  if (tab.session) {
    const old = tab.session.id;
    tab.session = null;
    await closeSession(old).catch(() => undefined);
  }
  await connect(id);
}

/**
 * Moves a PostgreSQL tab's session to another database (`null`: the
 * profile's own) and connects again. The database editor uses it when the
 * tab is connected to the database it drops or renames.
 */
async function switchTabDatabase(id: string, database: string | null): Promise<void> {
  const tab = find(id);
  if (!tab) return;
  const profile = useWorkspace().connection(tab.connectionId);
  if (!profile || profile.engine !== 'postgres') return;
  tab.database = database;
  await reconnect(id);
}

/**
 * Runs `text` in a query tab. `offset` is where the text starts in the
 * editor, so error positions land on the right character.
 */
async function runQuery(id: string, text: string, offset: number): Promise<void> {
  const tab = find(id);
  if (!tab || tab.kind !== 'query' || !tab.session || tab.running || text.trim() === '') return;
  const profile = useWorkspace().connection(tab.connectionId);
  const sessionId = tab.session.id;
  tab.running = true;
  const startedAt = Date.now();
  const doc = tab.sql;
  runIds += 1;
  const runId = runIds;
  try {
    const output = await runWithConfirmation(
      (confirmed) => execute(sessionId, text, confirmed),
      (request) =>
        useDialogs().askDestructive({
          connectionName: profile?.name ?? tab.title,
          environment: request.environment,
          statements: request.statements,
        }),
    );
    const current = find(id);
    // Results of a session that was replaced meanwhile (reconnect) are dropped.
    if (current?.kind === 'query' && output && current.session?.id === sessionId) {
      current.run = markRaw({ output, error: null, offset, text, doc, id: runId, startedAt });
      followSelect(current, text, null);
    }
  } catch (error: unknown) {
    const current = find(id);
    if (current?.kind === 'query' && current.session?.id === sessionId) {
      const cmd = toCmdError(error);
      const driverError = driverErrorOf(cmd);
      // The statements before the failing one ran: their results are shown with the error.
      const output = partialAsOutput(driverError?.partial);
      current.run = markRaw({ output, error: driverError ?? cmd, offset, text, doc, id: runId, startedAt });
      // Commands before the failing one did run: a SELECT among them still switched the database.
      if (driverError) followSelect(current, text, driverError);
      if (isLostSession(cmd)) markLost(id, cmd);
    }
  } finally {
    const current = find(id);
    if (current?.kind === 'query') current.running = false;
  }
}

/** Keeps the Redis database switcher in step with a `SELECT n` that ran in the console. */
function followSelect(tab: WorkspaceTab, ranText: string, error: DriverError | null): void {
  if (tab.db === null) return;
  const db = selectedDbAfterRun(ranText, error);
  if (db !== null) tab.db = db;
}

/**
 * Marks a tab whose session is gone (the connection dropped, or the backend
 * no longer knows the session). The tab then offers "Connect again".
 */
function markLost(id: string, error: CmdError): void {
  const tab = find(id);
  if (!tab) return;
  const old = tab.session;
  tab.session = null;
  tab.status = 'error';
  tab.connectError = error;
  if (old) void closeSession(old.id).catch(() => undefined);
}

/** Switches the Redis database of a tab's session; failures are rethrown for the caller to show. */
async function selectDb(id: string, db: number): Promise<void> {
  const tab = find(id);
  if (!tab?.session) return;
  const sessionId = tab.session.id;
  try {
    await redisSelectDb(sessionId, db);
  } catch (error: unknown) {
    const cmd = toCmdError(error);
    if (isLostSession(cmd)) markLost(id, cmd);
    throw error;
  }
  const current = find(id);
  // The answer belongs to the session it was sent on; a reconnect meanwhile wins.
  if (!current || current.session?.id !== sessionId) return;
  current.db = db;
  if (current.kind === 'keys') resetKeys(current);
}

/**
 * Clears the scan of a key browser tab and names its database in the title.
 * Called whenever the session or database changes, so a cursor or selected
 * key from one session or database is never used with another.
 */
function resetKeys(tab: KeysTab): void {
  tab.index = markRaw(new KeyIndex(tab.delimiter));
  tab.keysVersion += 1;
  tab.capped = false;
  tab.cursor = null;
  tab.scanned = false;
  tab.selected = null;
  tab.scanFor = null;
  tab.scanToken += 1;
  const profile = useWorkspace().connection(tab.connectionId);
  tab.title = `Keys · ${profile?.name ?? 'Redis'} db${tab.db ?? profile?.options.redisDb ?? 0}`;
}

async function cancelQuery(id: string): Promise<void> {
  const tab = find(id);
  if (tab?.kind === 'query' && tab.session && tab.running) await cancel(tab.session.id);
}

/** Replaces the editor text of a tab (from history or a saved query). */
function loadText(id: string, sql: string): void {
  const tab = find(id);
  if (tab?.kind === 'query') {
    tab.sql = sql;
    tab.revision += 1;
  }
}

function activate(id: string): void {
  if (find(id)) activeId.value = id;
}

// A remembered default database belongs to one version of a connection: an edit or delete drops it.
watch(
  () => useWorkspace().records.value.connections.map((c) => defaultKey(c)),
  (current) => {
    const live = new Set(current);
    for (const key of [...defaultDatabases.keys()]) if (!live.has(key)) defaultDatabases.delete(key);
  },
);

// Keep the saved query of each tab in step with the tree, so a rename or move
// made elsewhere is not reverted by the next Cmd+S, and a deleted query
// becomes an unsaved tab.
watch(
  () => useWorkspace().records.value.queries,
  (queries) => {
    const byId = new Map(queries.map((q) => [q.id, q]));
    for (const tab of tabs) {
      if (tab.kind !== 'query' || !tab.savedQuery) continue;
      const latest = byId.get(tab.savedQuery.id);
      if (!latest) {
        tab.savedQuery = null;
        continue;
      }
      if (tab.title === tab.savedQuery.name) tab.title = latest.name;
      tab.savedQuery = latest;
    }
  },
);

/** Editor and key browser tabs, one database session each, shared by the whole app. */
export function useTabs() {
  return {
    tabs,
    activeId,
    active,
    openQueryTab,
    openKeysTab,
    openObjectTab,
    switchTabDatabase,
    openOverviewTab,
    openDdlTab,
    openTableDataTab,
    openTableDesigner,
    openExplainTab,
    openLiveTab,
    openRedisKeyTab,
    openNotifyTab,
    openPubSubTab,
    reportTabError,
    setUnsaved,
    setTabObject,
    setOverviewScope,
    closeTab,
    closeConnectionTabs,
    reconnect,
    markLost,
    selectDb,
    runQuery,
    cancelQuery,
    loadText,
    activate,
  };
}

/** The `sessionId:db` a key browser's scan state must match to be used. */
export function scanBinding(tab: Pick<WorkspaceTab, 'session' | 'db'>): string | null {
  return tab.session ? `${tab.session.id}:${tab.db ?? 0}` : null;
}
