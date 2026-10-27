import { markRaw, reactive } from 'vue';
import { catalogCapabilities, catalogDatabases, catalogKinds, catalogList, catalogSchemas } from '../../api/catalog';
import type { Capabilities, Container, ObjectSummary } from '../../api/catalog';
import { presentError } from '../../api/errors';
import { useTabs } from '../../composables/useTabs';
import type { OverviewTab } from '../../composables/useTabs';
import { useObjectBrowser } from '../tree/objectBrowser';
import { defaultDatabase, defaultSchema, overviewKinds, overviewRows } from './overview';
import type { OverviewRow } from './overview';

export interface OverviewState {
  capabilities: Capabilities | null;
  /** Reading the capabilities failed: edit actions are missing from the menus until a refresh. */
  capabilitiesError: string | null;
  /** The session and container (`containerKey`) `rows` belong to; `null` while none. */
  rowsFor: string | null;
  /** What the picker offers: schemas (PostgreSQL) or databases (MySQL, MariaDB). */
  choices: ObjectSummary[];
  rows: OverviewRow[];
  preparing: boolean;
  loading: boolean;
  /** Reading the picker choices failed; stays until the next prepare. */
  prepareError: string | null;
  /** Listing the objects failed; stays until the next list. */
  loadError: string | null;
}

function containerKey(sessionId: string, where: Container): string {
  return JSON.stringify([sessionId, where]);
}

/** The PostgreSQL database a tab's overview lists, or `null` when the session does not say. */
function pgDatabase(tab: OverviewTab): string | null {
  return tab.session?.database ?? tab.database;
}

/** The container an overview tab lists now, or `null` while its scope is not chosen yet. */
export function overviewContainer(tab: OverviewTab): Container | null {
  const session = tab.session;
  if (!session) return null;
  if (session.engine === 'postgres') {
    const database = pgDatabase(tab);
    return database === null || tab.scope.schema === null ? null : { database, schema: tab.scope.schema, parent: null };
  }
  return tab.scope.database === null ? null : { database: tab.scope.database, schema: null, parent: null };
}

/**
 * Loading state of one overview tab. Reading the picker choices (`prepare`)
 * and listing a container (`load`) each keep their own request counter, so
 * a list never discards the picker answer or the reverse; only a newer
 * request of the same kind does. A list for a container already loaded or
 * on its way is not sent again (`refresh` forces it).
 */
export function createOverviewLoader(tabId: string) {
  const tabs = useTabs();
  const state = reactive<OverviewState>({
    capabilities: null,
    capabilitiesError: null,
    rowsFor: null,
    choices: [],
    rows: [],
    preparing: false,
    loading: false,
    prepareError: null,
    loadError: null,
  });
  let prepareToken = 0;
  let loadToken = 0;
  /** The container of the newest list request, and of the last one that finished. */
  let requested: string | null = null;
  let loaded: string | null = null;

  const tab = (): OverviewTab | null => {
    const found = tabs.tabs.find((t) => t.id === tabId);
    return found?.kind === 'overview' ? found : null;
  };

  const message = (error: unknown): string => presentError(tabs.reportTabError(tabId, error)).message;

  async function prepare(): Promise<void> {
    const current = tab();
    const session = current?.session;
    if (!current || !session) return;
    const mine = (prepareToken += 1);
    state.preparing = true;
    state.prepareError = null;
    const pg = session.engine === 'postgres';
    const database = pgDatabase(current);
    // The capabilities (menu entries) do not depend on the picker list: a failing list keeps them.
    const capabilitiesRead = catalogCapabilities(session.id)
      .then((capabilities) => {
        if (mine !== prepareToken) return;
        state.capabilities = markRaw(capabilities);
        state.capabilitiesError = null;
      })
      .catch((error: unknown) => {
        if (mine === prepareToken) state.capabilitiesError = message(error);
      });
    try {
      if (pg && database === null) {
        throw new Error('This session did not report its database, so its schemas cannot be listed. Reconnect the tab.');
      }
      const list = await (pg && database !== null ? catalogSchemas(session.id, database) : catalogDatabases(session.id));
      if (mine !== prepareToken) return;
      const include = useObjectBrowser().includeSystem(current.connectionId);
      const visible = list.filter((c) => include || !c.system);
      // The default comes from what the picker shows; with nothing visible, from everything, shown too.
      const pool = visible.length > 0 ? visible : list;
      state.choices = markRaw(pool);
      const latest = tab();
      if (!latest) return;
      if (pg) {
        if (latest.scope.schema === null) tabs.setOverviewScope(tabId, { schema: defaultSchema(pool) });
      } else if (latest.scope.database === null) {
        tabs.setOverviewScope(tabId, { database: defaultDatabase(pool, session.database) });
      }
    } catch (error: unknown) {
      if (mine === prepareToken) state.prepareError = message(error);
    } finally {
      await capabilitiesRead;
      if (mine === prepareToken) state.preparing = false;
    }
  }

  async function load(force = false): Promise<void> {
    const current = tab();
    const session = current?.session;
    const where = current ? overviewContainer(current) : null;
    if (!current || !session || !where) return;
    const key = containerKey(session.id, where);
    // A list of this container on its way is never asked twice; a finished one only when forced.
    if (key === requested && state.loading) return;
    if (!force && key === loaded) {
      state.loadError = null;
      return;
    }
    requested = key;
    const mine = (loadToken += 1);
    state.loading = true;
    state.loadError = null;
    // Rows of another container never show under this one's label.
    if (key !== state.rowsFor) {
      state.rows = [];
      state.rowsFor = null;
    }
    try {
      const include = useObjectBrowser().includeSystem(current.connectionId);
      const kinds = overviewKinds(await catalogKinds(session.id, where, include));
      const lists = await Promise.all(kinds.map((kind) => catalogList(session.id, where, kind, include)));
      if (mine !== loadToken) return;
      state.rows = markRaw(overviewRows(lists.flat()));
      loaded = key;
      state.rowsFor = key;
    } catch (error: unknown) {
      if (mine === loadToken) {
        state.loadError = message(error);
        state.rows = [];
        state.rowsFor = null;
        loaded = null;
      }
    } finally {
      if (mine === loadToken) state.loading = false;
    }
  }

  /** Reads the picker choices and the objects again. */
  async function refresh(): Promise<void> {
    // Forgetting what was loaded makes the next list read again; a list already on its way
    // (started after this point) counts, so the container is never asked twice.
    loaded = null;
    await prepare();
    await load();
  }

  function container(): Container | null {
    const current = tab();
    return current ? overviewContainer(current) : null;
  }

  /** The key of what the tab shows now (session and container), to match against `state.rowsFor`. */
  function currentKey(): string | null {
    const current = tab();
    const where = current ? overviewContainer(current) : null;
    return current?.session && where ? containerKey(current.session.id, where) : null;
  }

  /** The rows of the container the tab shows now; rows read for any other container are never returned. */
  function rows(): OverviewRow[] {
    const key = currentKey();
    return key !== null && state.rowsFor === key ? state.rows : [];
  }

  return { state, prepare, load, refresh, container, currentKey, rows };
}

export type OverviewLoader = ReturnType<typeof createOverviewLoader>;
