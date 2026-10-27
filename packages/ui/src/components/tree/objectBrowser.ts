import { reactive, watch } from 'vue';
import { catalogCapabilities, catalogDatabases, catalogKinds, catalogList, catalogSchemas, sameObject } from '../../api/catalog';
import type { Capabilities, Container, ObjectRef } from '../../api/catalog';
import { CommandFailure, presentError, toCmdError } from '../../api/errors';
import type { CmdError } from '../../api/errors';
import type { ConnectionProfile } from '../../api/records';
import { hasModules, redisKeyspace, redisModules } from '../../api/redis';
import { closeSession, openDatabaseSession, openSession } from '../../api/sessions';
import type { SessionInfo } from '../../api/sessions';
import { withConnectPrompts } from '../../composables/useConnect';
import { rememberDefaultDatabase } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import { isLostSession } from '../../lib/connectFlow';
import { isStringArray, readStored, writeStored } from '../../lib/storage';
import {
  databaseChildren,
  groupChildren,
  nodeDatabase,
  objectChildren,
  objectExpandable,
  objectRootKey,
  redisRootNodes,
  schemaChildren,
  sqlRootNodes,
} from '../../lib/tree';
import type { ChildState, ObjNode, ObjectSource } from '../../lib/tree';

const EXPANDED_KEY = 'varve.objects.expanded';
/** Expanded object nodes kept across restarts; older entries are dropped beyond this. */
const MAX_REMEMBERED = 2000;

/** A user session the object browser opened for itself (catalog calls name it to reach the metadata session). */
interface BrowseSession {
  session: SessionInfo | null;
  /** The open in progress, shared by every load that needs this session meanwhile. */
  pending: Promise<SessionInfo | null> | null;
}

/** The object tree state of one connection. */
interface ConnectionBrowser {
  connectionId: string;
  /** Bumped by disconnect and reconnect: answers of an older generation are dropped. */
  generation: number;
  root: ChildState;
  /** The session on the profile's database. */
  base: BrowseSession;
  /** PostgreSQL: sessions on other databases, by name. */
  databases: Map<string, BrowseSession>;
  capabilities: Capabilities | null;
  /** Show system objects; `null` follows the connection option. */
  includeSystem: boolean | null;
}

const browsers = reactive(new Map<string, ConnectionBrowser>());
const loads = reactive(new Map<string, ChildState>());
/** Identity of the newest load per key; an older answer for the key is dropped. */
const loadTokens = new Map<string, number>();
let tokens = 0;
let generations = 0;

const objectExpanded = reactive(new Set<string>(readStored<string[]>(EXPANDED_KEY, [], isStringArray)));

function persistExpanded(): void {
  const keys = [...objectExpanded];
  writeStored(EXPANDED_KEY, keys.slice(Math.max(0, keys.length - MAX_REMEMBERED)));
}

function setExpanded(key: string, open: boolean): void {
  if (open) {
    // Re-adding moves the key to the end, so the newest survive the cap.
    objectExpanded.delete(key);
    objectExpanded.add(key);
  } else objectExpanded.delete(key);
  persistExpanded();
}

function profileOf(connectionId: string): ConnectionProfile | null {
  return useWorkspace().connection(connectionId);
}

function includeSystemFor(connectionId: string): boolean {
  const browser = browsers.get(connectionId);
  if (browser && browser.includeSystem !== null) return browser.includeSystem;
  return profileOf(connectionId)?.options.showSystemObjects ?? false;
}

function errorState(error: CmdError): ChildState {
  const presented = presentError(error);
  const extra = [presented.code, presented.detail].filter(Boolean).join(' · ');
  return {
    status: 'error',
    title: presented.title,
    message: extra ? `${presented.message} (${extra})` : presented.message,
    reconnect: error.kind === 'reconnectRequired',
  };
}

/** Drops loaded children of `key` and everything below it. */
function forgetBelow(key: string, includeSelf: boolean): void {
  for (const k of [...loads.keys()]) {
    if (k.startsWith(`${key}/`) || (includeSelf && k === key)) {
      loads.delete(k);
      loadTokens.delete(k);
    }
  }
}

function closeAll(browser: ConnectionBrowser): void {
  const sessions = [browser.base.session, ...[...browser.databases.values()].map((b) => b.session)];
  browser.base = { session: null, pending: null };
  browser.databases = new Map();
  for (const session of sessions) if (session) void closeSession(session.id).catch(() => undefined);
}

/**
 * Opens (or reuses) the browse session for a database. `null` when the user
 * cancelled a password prompt.
 */
async function sessionFor(browser: ConnectionBrowser, database: string | null): Promise<SessionInfo | null> {
  const profile = profileOf(browser.connectionId);
  if (!profile) throw new CommandFailure({ kind: 'notFound', message: 'This connection no longer exists.' });
  const baseDb = browser.base.session?.database ?? null;
  const other = profile.engine === 'postgres' && database !== null && baseDb !== null && database !== baseDb;
  let slot = browser.base;
  if (other) {
    const existing = browser.databases.get(database);
    if (existing) slot = existing;
    else {
      slot = { session: null, pending: null };
      browser.databases.set(database, slot);
    }
  }
  if (slot.session) return slot.session;
  if (slot.pending) return slot.pending;
  const generation = browser.generation;
  const target = slot;
  target.pending = withConnectPrompts(profile, (secrets) =>
    other ? openDatabaseSession(profile.id, database, secrets) : openSession(profile.id, secrets),
  )
    .then((info) => {
      const current = browsers.get(browser.connectionId);
      if (!info) return null;
      if (current !== browser || browser.generation !== generation) {
        // Disconnected meanwhile: this session belongs to nobody.
        void closeSession(info.id).catch(() => undefined);
        return null;
      }
      target.session = info;
      if (!other) rememberDefaultDatabase(profile, info.database);
      return info;
    })
    .finally(() => {
      target.pending = null;
    });
  return target.pending;
}

/**
 * Closes the browse sessions connected to a PostgreSQL database, before a
 * change PostgreSQL refuses while any session uses it (drop, rename). The
 * tree opens a new session when it needs that database again.
 */
async function releaseDatabase(connectionId: string, database: string): Promise<void> {
  const browser = browsers.get(connectionId);
  if (!browser) return;
  const slots: BrowseSession[] = [];
  const slot = browser.databases.get(database);
  if (slot) {
    browser.databases.delete(database);
    slots.push(slot);
  }
  if (browser.base.session?.database === database) {
    slots.push(browser.base);
    browser.base = { session: null, pending: null };
  }
  await Promise.all(
    slots.map(async (s) => {
      const info = s.session ?? (s.pending ? await s.pending.catch(() => null) : null);
      if (info) await closeSession(info.id).catch(() => undefined);
    }),
  );
}

/** Forgets a browse session the backend no longer knows, so the next load opens a new one. */
function dropSession(browser: ConnectionBrowser, sessionId: string): void {
  if (browser.base.session?.id === sessionId) browser.base = { session: null, pending: null };
  for (const [name, slot] of browser.databases) {
    if (slot.session?.id === sessionId) browser.databases.delete(name);
  }
}

async function loadRoot(browser: ConnectionBrowser): Promise<void> {
  const generation = browser.generation;
  const current = (): boolean => browsers.get(browser.connectionId) === browser && browser.generation === generation;
  browser.root = { status: 'loading', message: browser.base.session ? 'Loading…' : 'Connecting…' };
  let sessionId: string | null = null;
  try {
    const session = await sessionFor(browser, null);
    if (!current()) return;
    if (!session) {
      browser.root = { status: 'idle' };
      return;
    }
    sessionId = session.id;
    browser.root = { status: 'loading', message: 'Loading…' };
    if (session.engine === 'redis') {
      const keyspace = await redisKeyspace(session.id);
      if (!current()) return;
      // A report that cannot be read, or cannot tell (`hasModules`), keeps the panel: it says itself what it cannot tell.
      const modules = await redisModules(session.id).then(hasModules, () => true);
      if (!current()) return;
      browser.root = { status: 'loaded', nodes: redisRootNodes(browser.connectionId, keyspace, session.server.major, modules) };
      return;
    }
    const includeSystem = includeSystemFor(browser.connectionId);
    const [capabilities, databases, serverKinds] = await Promise.all([
      catalogCapabilities(session.id),
      catalogDatabases(session.id),
      catalogKinds(session.id, { database: null, schema: null, parent: null }, includeSystem),
    ]);
    if (!current()) return;
    browser.capabilities = capabilities;
    browser.root = {
      status: 'loaded',
      nodes: sqlRootNodes({
        connectionId: browser.connectionId,
        engine: session.engine,
        capabilities,
        databases,
        serverKinds,
        currentDatabase: session.database,
        includeSystem,
      }),
    };
  } catch (error: unknown) {
    if (!current()) return;
    const cmd = toCmdError(error);
    if (sessionId !== null && isLostSession(cmd)) dropSession(browser, sessionId);
    browser.root = errorState(cmd);
  }
}

async function fetchChildren(node: ObjNode, sessionId: string, engine: SessionInfo['engine'], includeSystem: boolean): Promise<ObjNode[]> {
  switch (node.type) {
    case 'database': {
      const container = { database: node.name, schema: null, parent: null };
      const [kinds, schemas] = await Promise.all([
        catalogKinds(sessionId, container, includeSystem),
        engine === 'postgres' ? catalogSchemas(sessionId, node.name) : Promise.resolve([]),
      ]);
      return databaseChildren(node, engine, kinds, schemas, includeSystem);
    }
    case 'schema':
      return schemaChildren(node, await catalogKinds(sessionId, { database: node.database, schema: node.name, parent: null }, includeSystem));
    case 'group':
      return groupChildren(node, await catalogList(sessionId, node.container, node.kind, includeSystem));
    case 'object': {
      const ref = node.summary.object;
      return objectChildren(node, await catalogKinds(sessionId, { database: ref.database, schema: ref.schema, parent: ref }, includeSystem));
    }
    case 'section':
      return node.children;
    default:
      return [];
  }
}

/** Loads the children of an expandable node unless they are loaded or loading. */
async function loadChildren(node: ObjNode): Promise<void> {
  if (!objectExpandable(node) || node.type === 'section') return;
  const existing = loads.get(node.key);
  if (existing && existing.status !== 'error') return;
  const browser = browsers.get(node.connectionId);
  if (!browser || browser.root.status !== 'loaded') return;
  tokens += 1;
  const token = tokens;
  loadTokens.set(node.key, token);
  loads.set(node.key, { status: 'loading', message: 'Loading…' });
  const generation = browser.generation;
  const current = (): boolean =>
    browsers.get(node.connectionId) === browser && browser.generation === generation && loadTokens.get(node.key) === token;
  let sessionId: string | null = null;
  try {
    const session = await sessionFor(browser, nodeDatabase(node));
    if (!current()) return;
    if (!session) {
      loads.set(node.key, { status: 'error', title: 'Not connected', message: 'The password prompt was cancelled.', reconnect: false });
      return;
    }
    sessionId = session.id;
    const children = await fetchChildren(node, session.id, session.engine, includeSystemFor(node.connectionId));
    if (!current()) return;
    loads.set(node.key, { status: 'loaded', nodes: children });
  } catch (error: unknown) {
    if (!current()) return;
    const cmd = toCmdError(error);
    if (sessionId !== null && isLostSession(cmd)) dropSession(browser, sessionId);
    loads.set(node.key, errorState(cmd));
  }
}

/**
 * Starts browsing a connection (opens its browse session, asking for
 * secrets). Does nothing while it loads or is loaded. `includeSystem` is
 * the system object choice of a new browser (`null`: the connection option).
 */
function browse(connectionId: string, includeSystem: boolean | null = null): Promise<void> {
  const existing = browsers.get(connectionId);
  if (existing && (existing.root.status === 'loading' || existing.root.status === 'loaded')) return Promise.resolve();
  generations += 1;
  const browser: ConnectionBrowser = existing ?? {
    connectionId,
    generation: generations,
    root: { status: 'idle' },
    base: { session: null, pending: null },
    databases: new Map(),
    capabilities: null,
    includeSystem,
  };
  browser.generation = generations;
  if (!existing) browsers.set(connectionId, browser);
  // Read back through the reactive map, so state changes reach the views.
  const tracked = browsers.get(connectionId);
  return tracked ? loadRoot(tracked) : Promise.resolve();
}

/** Closes the browse sessions of a connection and forgets its loaded objects. */
function disconnect(connectionId: string): void {
  const browser = browsers.get(connectionId);
  if (!browser) return;
  generations += 1;
  browser.generation = generations;
  closeAll(browser);
  browsers.delete(connectionId);
  forgetBelow(objectRootKey(connectionId), true);
}

/** Opens new browse sessions (after `reconnectRequired` or a lost connection) and reloads. */
function reconnect(connectionId: string): Promise<void> {
  const includeSystem = browsers.get(connectionId)?.includeSystem ?? null;
  disconnect(connectionId);
  return browse(connectionId, includeSystem);
}

/**
 * Reloads a node's children, and below it whatever stays expanded (the
 * view asks for those again). Without a node, reloads the whole tree of
 * the connection on the sessions it has.
 */
function refresh(connectionId: string, node: ObjNode | null = null): Promise<void> {
  const browser = browsers.get(connectionId);
  if (!browser) return browse(connectionId);
  if (node === null || node.type === 'section') {
    forgetBelow(objectRootKey(connectionId), false);
    if (browser.root.status === 'loading') return Promise.resolve();
    return loadRoot(browser);
  }
  forgetBelow(node.key, true);
  return loadChildren(node);
}

/** Every node loaded so far under a connection (root nodes, section tools and loaded children). */
function loadedNodes(browser: ConnectionBrowser): ObjNode[] {
  const out: ObjNode[] = [];
  const add = (nodes: readonly ObjNode[]): void => {
    for (const node of nodes) {
      out.push(node);
      if (node.type === 'section') add(node.children);
    }
  };
  if (browser.root.status === 'loaded') add(browser.root.nodes);
  const prefix = `${objectRootKey(browser.connectionId)}/`;
  for (const [key, state] of loads) if (key.startsWith(prefix) && state.status === 'loaded') add(state.nodes);
  return out;
}

/** The loaded node that lists the objects of `container`, if any. */
function containerNode(browser: ConnectionBrowser, container: Container): ObjNode | null {
  const nodes = loadedNodes(browser);
  const parent = container.parent;
  if (parent !== null) return nodes.find((n) => n.type === 'object' && sameObject(n.summary.object, parent)) ?? null;
  if (container.schema !== null) {
    return nodes.find((n) => n.type === 'schema' && n.database === container.database && n.name === container.schema) ?? null;
  }
  if (container.database !== null) return nodes.find((n) => n.type === 'database' && n.name === container.database) ?? null;
  return null;
}

/**
 * Reloads what the tree shows of one container after an editor changed it
 * (created, altered or dropped an object there): the schema (PostgreSQL) or
 * database (MySQL, MariaDB) node, or the parent object for child objects,
 * with the counts and every expanded group below it. A server-wide
 * container reloads the whole tree. Nothing happens when the connection is
 * not browsed or the container is not loaded (it loads fresh when opened).
 */
function refreshContainer(connectionId: string, container: Container): Promise<void> {
  const browser = browsers.get(connectionId);
  if (!browser || browser.root.status !== 'loaded') return Promise.resolve();
  if (container.database === null && container.parent === null) return refresh(connectionId);
  const node = containerNode(browser, container);
  if (!node) return Promise.resolve();
  return refresh(connectionId, node);
}

/**
 * Reloads the tree around one object after it changed: its schema or
 * database container (so new, renamed and dropped objects and the counts
 * are right, and its own child groups reload when expanded).
 */
function refreshObject(connectionId: string, object: ObjectRef): Promise<void> {
  if (object.kind === 'database') return refresh(connectionId);
  if (object.kind === 'schema') return refreshContainer(connectionId, { database: object.database, schema: null, parent: null });
  return refreshContainer(connectionId, { database: object.database, schema: object.schema, parent: null });
}

/** Shows or hides system objects of a connection (for this app run) and reloads its tree. */
function setIncludeSystem(connectionId: string, include: boolean): Promise<void> {
  const browser = browsers.get(connectionId);
  if (!browser) return Promise.resolve();
  browser.includeSystem = include;
  return refresh(connectionId);
}

/** Retries a failed load: the connection's tree for its root key, otherwise the node's children. */
function retry(connectionId: string, node: ObjNode | null): Promise<void> {
  const browser = browsers.get(connectionId);
  if (browser?.root.status === 'error' && browser.root.reconnect) return reconnect(connectionId);
  if (node === null) {
    if (!browser || browser.root.status !== 'loading') {
      if (browser) return loadRoot(browser);
      return browse(connectionId);
    }
    return Promise.resolve();
  }
  const state = loads.get(node.key);
  if (state?.status === 'error' && state.reconnect) return reconnect(connectionId);
  forgetBelow(node.key, true);
  return loadChildren(node);
}

const source: ObjectSource = {
  root: (connectionId) => browsers.get(connectionId)?.root ?? { status: 'idle' },
  children: (key) => loads.get(key),
};

// A deleted connection takes its browse sessions with it.
watch(
  () => useWorkspace().records.value.connections.map((c) => c.id),
  (ids) => {
    const present = new Set(ids);
    for (const id of [...browsers.keys()]) if (!present.has(id)) disconnect(id);
  },
);

/** The object trees under connections in the sidebar, shared by the whole app. */
export function useObjectBrowser() {
  return {
    source,
    objectExpanded,
    setExpanded,
    browse,
    disconnect,
    reconnect,
    refresh,
    refreshContainer,
    refreshObject,
    releaseDatabase,
    retry,
    loadChildren,
    setIncludeSystem,
    includeSystem: includeSystemFor,
    /** True while a connection's objects are browsed (loading, loaded or failed). */
    browsing: (connectionId: string): boolean => (browsers.get(connectionId)?.root.status ?? 'idle') !== 'idle',
    capabilities: (connectionId: string): Capabilities | null => browsers.get(connectionId)?.capabilities ?? null,
  };
}
