import { computed, shallowRef } from 'vue';
import { reloadReport } from '../../api/sessions';
import type { ReloadReport } from '../../api/sessions';
import { useWorkspace } from '../../composables/useWorkspace';
import { reloadNoticeText } from './notices';

/**
 * What the last reload of the window closed. The backend empties the report
 * when it is read, so it is read once per page load and kept here (not in
 * the notice component) until dismissed: showing another page and coming
 * back keeps the notice.
 */
const report = shallowRef<ReloadReport | null>(null);
let requested = false;

/** Reads the report the first time it is called on this page; later calls do nothing. */
async function load(): Promise<void> {
  if (requested) return;
  requested = true;
  try {
    report.value = await reloadReport();
  } catch {
    // Nothing to tell when the report cannot be read.
  }
}

function dismiss(): void {
  report.value = null;
}

/**
 * The notice text, or `null`. Connection names are looked up when the text
 * is shown, and only once the connections were loaded (or failed to load):
 * a report that arrives before the tree does not name every connection "a
 * removed connection".
 */
const text = computed(() => {
  const current = report.value;
  if (!current) return null;
  const workspace = useWorkspace();
  if (!workspace.status.loaded && workspace.status.error === null) return null;
  return reloadNoticeText(current, (id) => workspace.connection(id)?.name ?? 'a removed connection');
});

/** The reload notice's state, shared by every mount of the notice. */
export function useReloadNotice() {
  return { text, load, dismiss };
}
