import { watch } from 'vue';
import type { ConnectionProfile } from '../../api/records';
import { useTabs } from '../../composables/useTabs';
import type { WorkspaceTab } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import { useObjectBrowser } from './objectBrowser';

/**
 * Opens a connection the way a double click (or Enter) on it does: the
 * overview of its objects (Redis: the key browser), and the object tree
 * under it expanded. The tree starts browsing only after the tab connected,
 * so a password is asked for once; when the tab fails or is cancelled the
 * tree stays as it was.
 */
export function openConnectionOverview(profile: ConnectionProfile): WorkspaceTab {
  const tabs = useTabs();
  const objects = useObjectBrowser();
  const tab = profile.engine === 'redis' ? tabs.openKeysTab(profile) : tabs.openOverviewTab(profile);
  useWorkspace().setExpanded(`connection:${profile.id}`, true);
  if (objects.browsing(profile.id)) return tab;
  let stop: (() => void) | null = null;
  let done = false;
  const settle = (status: WorkspaceTab['status'] | undefined): void => {
    if (done || status === 'connecting') return;
    done = true;
    stop?.();
    if (status === 'ready' && !objects.browsing(profile.id)) void objects.browse(profile.id);
  };
  stop = watch(() => tabs.tabs.find((t) => t.id === tab.id)?.status, settle);
  settle(tab.status);
  return tab;
}
