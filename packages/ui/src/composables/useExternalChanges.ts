import { isTauri } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { readonly, ref } from 'vue';
import type { CmdError } from '../api/errors';
import { RECORDS_CHANGED, SETTINGS_CHANGED } from '../api/mcp';
import { followExternalEdit, forgetAllTypedSecrets, forgetTypedSecrets, typedSecretIds } from './useConnect';
import { useQuerySave } from './useQuerySave';
import { useTabs } from './useTabs';
import { useToasts } from './useToasts';
import { useUpdates } from './useUpdates';
import { useWorkspace } from './useWorkspace';

/** Goes up each time settings changed outside the settings page; the page reloads them. */
const settingsRevision = ref(0);

/** What a tab of a deleted connection shows once the user chose to keep it. */
const GONE: CmdError = { kind: 'notFound', message: 'This connection no longer exists.' };

/** The last reload of the records after an outside change; the next one starts after it. */
let reloading: Promise<void> = Promise.resolve();

/** The last round of closing tabs; the next one starts after it. */
let closing: Promise<void> = Promise.resolve();

/**
 * Reloads the records after an outside change and applies it to what the
 * window holds for each connection: typed secrets are kept only while the
 * connection points at the same server and route, and whatever is held for
 * a connection that is no longer in the records is forgotten. Resolves to
 * the names of the connections seen to disappear (for the message), or
 * `null` when the records could not be read.
 */
async function reloadRecords(): Promise<ReadonlyMap<string, string> | null> {
  const workspace = useWorkspace();
  const before = workspace.records.value.connections;
  await workspace.refresh();
  if (workspace.status.error !== null) {
    // The new settings are unknown, so no typed secret can be checked
    // against them.
    forgetAllTypedSecrets();
    return null;
  }
  const after = new Map(workspace.records.value.connections.map((c) => [c.id, c]));
  const names = new Map<string, string>();
  for (const old of before) {
    const now = after.get(old.id);
    if (now) followExternalEdit(old, now);
    else names.set(old.id, old.name);
  }
  // Another reload may have run between the delete and this one, so
  // `before` may not hold the deleted connection any more: go by what the
  // window still holds, not by the difference.
  const held = new Set([...names.keys(), ...typedSecretIds(), ...useTabs().tabs.map((t) => t.connectionId)]);
  for (const id of held) {
    if (!after.has(id)) forgetTypedSecrets(id);
  }
  return names;
}

/**
 * Closes the tabs of connections that are not in the records any more
 * (deleted outside the window). The backend has closed their sessions
 * already; unsaved editor text is asked about first, and a tab the user
 * keeps stays open without a session, saying that the connection no longer
 * exists. Such a tab is not asked about again.
 */
async function closeGoneTabs(names: ReadonlyMap<string, string>): Promise<void> {
  const workspace = useWorkspace();
  const tabs = useTabs();
  const exists = (connectionId: string): boolean => workspace.records.value.connections.some((c) => c.id === connectionId);
  const affected = tabs.tabs.filter(
    (t) => !exists(t.connectionId) && !(t.session === null && t.status === 'error' && t.connectError?.kind === 'notFound'),
  );
  if (affected.length === 0) return;
  const ids = affected.map((t) => t.id);
  const connections = [...new Set(affected.map((t) => t.connectionId))];
  const [only] = connections;
  if (connections.length === 1 && only !== undefined) {
    // The name is known only when this reload saw the connection disappear.
    useToasts().push('info', `${names.get(only) ?? 'A connection'} was deleted`, 'An MCP client deleted it, so its tabs close.');
  } else {
    useToasts().push('info', `${connections.length} connections were deleted`, 'An MCP client deleted them, so their tabs close.');
  }
  for (const id of ids) {
    const tab = tabs.tabs.find((t) => t.id === id);
    // Closed meanwhile, or the connection is back (an import, for example).
    if (!tab || exists(tab.connectionId)) continue;
    if (await useQuerySave().confirmUnsavedTabs([id])) await tabs.closeTab(id);
    else tabs.markLost(id, GONE);
  }
}

/**
 * Follows one outside change of the records. Changes are followed one after
 * the other: each reload starts when the one before it has finished, and
 * the tabs of deleted connections are closed in rounds that never overlap
 * (a round waits for the user's answers about unsaved text, the reloads do
 * not wait for that).
 */
function followRecordsChange(): Promise<void> {
  const reloaded = reloading.then(reloadRecords);
  reloading = reloaded.then(
    () => undefined,
    () => undefined,
  );
  const closed = closing.then(async () => {
    const names = await reloaded;
    if (names) await closeGoneTabs(names);
  });
  closing = closed.catch(() => undefined);
  return closed;
}

/**
 * Follows changes an MCP client made through the app: the tree reloads (and
 * typed secrets and tabs follow the changed connections), and the settings
 * page and the update settings read their values again. Outside Tauri it
 * does nothing. Resolves to a cleanup function.
 */
export async function installExternalChanges(): Promise<() => void> {
  if (!isTauri()) return () => undefined;
  const cleanups: (() => void)[] = [];
  try {
    cleanups.push(
      await listen(RECORDS_CHANGED, () => {
        followRecordsChange().catch((error: unknown) => {
          useToasts().pushError(error, 'Could not follow a change to the connections');
        });
      }),
    );
    cleanups.push(
      await listen(SETTINGS_CHANGED, () => {
        settingsRevision.value += 1;
        void useUpdates().refreshInfo();
      }),
    );
  } catch (error: unknown) {
    for (const cleanup of cleanups.splice(0)) cleanup();
    throw error;
  }
  return () => {
    for (const cleanup of cleanups.splice(0)) cleanup();
  };
}

export function useExternalChanges() {
  return { settingsRevision: readonly(settingsRevision) };
}
