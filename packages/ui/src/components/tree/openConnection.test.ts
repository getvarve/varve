import type { ConnectionProfile } from '../../api/records';
import type { WorkspaceTab } from '../../composables/useTabs';
import { openConnectionOverview } from './openConnection';

const h = vi.hoisted(() => ({
  tabList: [] as Partial<WorkspaceTab>[],
  browse: vi.fn(async (_id: string) => undefined),
  browsing: { value: false },
  expanded: [] as [string, boolean][],
}));
const { tabList, browse, expanded } = h;

vi.mock('../../composables/useTabs', async () => {
  const { reactive } = await import('vue');
  const tabs = reactive(h.tabList);
  const open = (kind: WorkspaceTab['kind']): Partial<WorkspaceTab> | undefined => {
    tabs.push({ id: `t${tabs.length + 1}`, kind, status: 'connecting' } as Partial<WorkspaceTab>);
    return tabs[tabs.length - 1];
  };
  return { useTabs: () => ({ tabs, openOverviewTab: () => open('overview'), openKeysTab: () => open('keys') }) };
});
vi.mock('../../composables/useWorkspace', () => ({
  useWorkspace: () => ({ setExpanded: (key: string, open: boolean) => h.expanded.push([key, open]) }),
}));
vi.mock('./objectBrowser', () => ({
  useObjectBrowser: () => ({ browse: (id: string) => h.browse(id), browsing: () => h.browsing.value }),
}));

const pg = { id: 'pg', engine: 'postgres' } as unknown as ConnectionProfile;
const redis = { id: 'rd', engine: 'redis' } as unknown as ConnectionProfile;
const tick = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));

describe('double click on a connection', () => {
  beforeEach(() => {
    tabList.length = 0;
    expanded.length = 0;
    browse.mockClear();
    h.browsing.value = false;
  });

  it('opens the overview, expands the connection and browses once the tab connected', async () => {
    const { useTabs } = await import('../../composables/useTabs');
    const tab = openConnectionOverview(pg);
    expect(tab.kind).toBe('overview');
    expect(expanded).toEqual([['connection:pg', true]]);
    expect(browse).not.toHaveBeenCalled();
    const live = useTabs().tabs.find((t) => t.id === tab.id);
    if (live) live.status = 'ready';
    await tick();
    expect(browse).toHaveBeenCalledWith('pg');
  });

  it('opens the key browser for Redis', () => {
    expect(openConnectionOverview(redis).kind).toBe('keys');
  });

  it('leaves the tree alone when the tab could not connect', async () => {
    const { useTabs } = await import('../../composables/useTabs');
    const tab = openConnectionOverview(pg);
    const live = useTabs().tabs.find((t) => t.id === tab.id);
    if (live) live.status = 'error';
    await tick();
    expect(browse).not.toHaveBeenCalled();
  });

  it('does not browse again when the tree already browses', () => {
    h.browsing.value = true;
    openConnectionOverview(pg);
    expect(browse).not.toHaveBeenCalled();
  });
});
