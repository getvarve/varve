import type { DropImpact } from '../../api/ddl';
import { useTabs } from '../../composables/useTabs';
import type { AppTab } from '../../lib/dropImpact';

/**
 * This app's tabs behind the impact's `appSessions` (matched by session
 * id), with their titles. A session no tab holds (the object browser's)
 * is left out: the app closes those itself before the drop.
 */
export function appTabsOf(impact: DropImpact | null): AppTab[] {
  if (!impact) return [];
  const tabs = useTabs().tabs;
  const out: AppTab[] = [];
  for (const s of impact.appSessions) {
    const tab = tabs.find((t) => t.session?.id === s.sessionId);
    if (tab) out.push({ title: tab.title, busy: s.busy, openTransaction: s.openTransaction, certain: s.certain, standby: s.standby });
  }
  return out;
}
