import { isTauri } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { readonly, ref } from 'vue';
import { RECORDS_CHANGED, SETTINGS_CHANGED } from '../api/mcp';
import { useUpdates } from './useUpdates';
import { useWorkspace } from './useWorkspace';

/** Goes up each time settings changed outside the settings page; the page reloads them. */
const settingsRevision = ref(0);

/**
 * Follows changes an MCP client made through the app: the tree reloads, and
 * the settings page and the update settings read their values again.
 * Outside Tauri it does nothing. Resolves to a cleanup function.
 */
export async function installExternalChanges(): Promise<() => void> {
  if (!isTauri()) return () => undefined;
  const cleanups: (() => void)[] = [];
  try {
    cleanups.push(
      await listen(RECORDS_CHANGED, () => {
        void useWorkspace().refresh();
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
