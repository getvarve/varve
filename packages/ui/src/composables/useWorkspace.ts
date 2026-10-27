import { computed, reactive, shallowRef } from 'vue';
import { fetchTree } from '../api/records';
import type { ConnectionProfile, SavedQuery, WorkspaceRecords } from '../api/records';
import { isStringArray, readStored, writeStored } from '../lib/storage';
import { buildTree } from '../lib/tree';
import { useToasts } from './useToasts';

const EXPANDED_KEY = 'varve.tree.expanded';

const records = shallowRef<WorkspaceRecords>({ folders: [], connections: [], queries: [] });
const status = reactive<{ loaded: boolean; loading: boolean; error: string | null }>({
  loaded: false,
  loading: false,
  error: null,
});
const expanded = reactive(new Set<string>(readStored<string[]>(EXPANDED_KEY, [], isStringArray)));

const tree = computed(() => buildTree(records.value));
const connectionsById = computed(() => new Map(records.value.connections.map((c) => [c.id, c])));
const isEmpty = computed(
  () => status.loaded && records.value.folders.length === 0 && records.value.connections.length === 0 && records.value.queries.length === 0,
);

let inflight: Promise<void> | null = null;
let queued: Promise<void> | null = null;

function fetchOnce(): Promise<void> {
  status.loading = true;
  return fetchTree()
    .then((value) => {
      records.value = value;
      status.loaded = true;
      status.error = null;
    })
    .catch((error: unknown) => {
      status.error = error instanceof Error ? error.message : String(error);
      useToasts().pushError(error, 'Could not load connections');
    })
    .finally(() => {
      status.loading = false;
    });
}

/**
 * Reloads every record from the backend. A refresh asked for while a fetch
 * runs never joins that fetch (it may have started before the write the
 * caller wants to see): one more fetch is queued after it, shared by every
 * caller that asks in the meantime.
 */
function refresh(): Promise<void> {
  if (!inflight) {
    inflight = fetchOnce().finally(() => {
      inflight = null;
    });
    return inflight;
  }
  if (!queued) {
    queued = inflight.then(() => {
      queued = null;
      return refresh();
    });
  }
  return queued;
}

function persistExpanded(): void {
  writeStored(EXPANDED_KEY, [...expanded]);
}

function setExpanded(key: string, open: boolean): void {
  if (open) expanded.add(key);
  else expanded.delete(key);
  persistExpanded();
}

function connection(id: string | null): ConnectionProfile | null {
  return id === null ? null : (connectionsById.value.get(id) ?? null);
}

function query(id: string): SavedQuery | null {
  return records.value.queries.find((q) => q.id === id) ?? null;
}

/** Folders, connections and saved queries of the personal workspace, shared by every component. */
export function useWorkspace() {
  return { records, tree, status, expanded, isEmpty, refresh, setExpanded, connection, query };
}
