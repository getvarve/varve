import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { Router } from 'vue-router';
import { isMenuAction, livePanelFor, MENU_ACTIONS, menuEnabled, runMenuAction } from './useAppMenu';
import { registerTabCommands, tabCommands } from './useTabCommands';
import { useSidebar } from './useSidebar';

const calls: string[] = [];
const state = { activeTab: null as { id: string; connectionId: string } | null, connections: [{ id: 'pg', engine: 'postgres' }] };

vi.mock('./useTabs', () => ({
  useTabs: () => ({
    active: { get value() { return state.activeTab; } },
    openQueryTab: (p: { id: string }) => calls.push(`query:${p.id}`),
    openLiveTab: (p: { id: string }, panel: string) => calls.push(`live:${p.id}:${panel}`),
    closeTab: (id: string) => { calls.push(`close:${id}`); return Promise.resolve(); },
    reconnect: (id: string) => { calls.push(`reconnect:${id}`); return Promise.resolve(); },
  }),
}));
vi.mock('./useWorkspace', () => ({
  useWorkspace: () => ({
    records: { get value() { return { connections: state.connections }; } },
    connection: (id: string) => state.connections.find((c) => c.id === id) ?? null,
  }),
}));
vi.mock('./useWorkspaceActions', () => ({
  useWorkspaceActions: () => ({
    newConnection: () => { calls.push('new-connection'); return Promise.resolve(null); },
    newFolder: (parent: string | null) => { calls.push(`new-folder:${String(parent)}`); return Promise.resolve(null); },
    editConnection: (p: { id: string }) => { calls.push(`edit:${p.id}`); return Promise.resolve(); },
    duplicate: (p: { id: string }) => { calls.push(`duplicate:${p.id}`); return Promise.resolve(); },
  }),
}));
vi.mock('./useDialogs', () => ({
  useDialogs: () => ({ pickConnection: () => { calls.push('pick'); return Promise.resolve({ id: 'rd', engine: 'redis' }); } }),
}));
vi.mock('../components/tree/openConnection', () => ({ openConnectionOverview: (p: { id: string }) => calls.push(`overview:${p.id}`) }));
vi.mock('../backend', () => ({ setMenuEnabled: () => Promise.resolve(true) }));
vi.mock('./useToasts', () => ({ useToasts: () => ({ push: (_: string, title: string) => calls.push(`toast:${title}`), pushError: vi.fn() }) }));

function router(name = 'home') {
  const pushed: unknown[] = [];
  return { router: { currentRoute: { value: { name } }, push: (to: unknown) => { pushed.push(to); return Promise.resolve(); } } as unknown as Router, pushed };
}

describe('menu bar actions', () => {
  beforeEach(() => {
    calls.length = 0;
    state.activeTab = null;
    state.connections = [{ id: 'pg', engine: 'postgres' }];
    localStorage.clear();
    useSidebar().reload();
  });

  it('knows its own actions and nothing else', () => {
    for (const action of MENU_ACTIONS) expect(isMenuAction(action)).toBe(true);
    expect(isMenuAction('quit')).toBe(false);
    expect(isMenuAction(3)).toBe(false);
  });

  it('opens the panel that answers the question on each engine', () => {
    expect(livePanelFor('live:status', 'postgres')).toBe('serverStatus');
    expect(livePanelFor('live:status', 'redis')).toBe('redisInfo');
    expect(livePanelFor('live:processes', 'mysql')).toBe('processes');
    expect(livePanelFor('live:processes', 'redis')).toBe('redisClients');
    expect(livePanelFor('live:variables', 'mariadb')).toBe('variables');
    expect(livePanelFor('live:variables', 'redis')).toBe('redisConfig');
  });

  it('uses the connection of the tab in front, and asks when no tab is', async () => {
    state.activeTab = { id: 't1', connectionId: 'pg' };
    await runMenuAction('new-query', router().router);
    expect(calls).toEqual(['query:pg']);

    calls.length = 0;
    state.activeTab = null;
    const away = router('activity');
    await runMenuAction('live:status', away.router);
    expect(calls).toEqual(['pick', 'live:rd:redisInfo']);
    // The tab opened on the workspace page, so that page is shown.
    expect(away.pushed).toEqual([{ name: 'home' }]);
  });

  it('says so instead of asking when there is no connection at all', async () => {
    state.connections = [];
    await runMenuAction('new-query', router().router);
    expect(calls).toEqual(['toast:No connection yet']);
  });

  it('closes the tab in front only on the workspace page', async () => {
    state.activeTab = { id: 't1', connectionId: 'pg' };
    await runMenuAction('close-tab', router('settings').router);
    expect(calls).toEqual([]);
    await runMenuAction('close-tab', router().router);
    expect(calls).toEqual(['close:t1']);
  });

  it('goes to pages, makes things and moves the sidebar', async () => {
    const r = router();
    await runMenuAction('show-activity', r.router);
    await runMenuAction('show-settings', r.router);
    expect(r.pushed).toEqual([{ name: 'activity' }, { name: 'settings' }]);
    await runMenuAction('new-connection', r.router);
    await runMenuAction('new-folder', r.router);
    expect(calls).toEqual(['new-connection', 'new-folder:null']);

    await runMenuAction('toggle-sidebar', r.router);
    expect(useSidebar().state.collapsed).toBe(true);
    const before = useSidebar().fitRequests.value;
    await runMenuAction('fit-sidebar', r.router);
    // A hidden sidebar is shown first: there is nothing to fit otherwise.
    expect(useSidebar().state.collapsed).toBe(false);
    expect(useSidebar().fitRequests.value).toBe(before + 1);
  });

  it('runs the query tab\'s own actions, only on the workspace page', async () => {
    state.activeTab = { id: 't1', connectionId: 'pg' };
    const done: string[] = [];
    const remove = registerTabCommands('t1', {
      run: () => done.push('run'),
      cancel: () => done.push('cancel'),
      save: () => done.push('save'),
      saveAs: () => done.push('saveAs'),
      toggleHistory: () => done.push('history'),
    });
    for (const action of ['query-run', 'query-cancel', 'query-save', 'query-save-as', 'query-history'] as const) {
      await runMenuAction(action, router().router);
    }
    expect(done).toEqual(['run', 'cancel', 'save', 'saveAs', 'history']);
    await runMenuAction('query-run', router('activity').router);
    expect(done).toHaveLength(5);
    remove();
    expect(tabCommands('t1')).toBeNull();
    await runMenuAction('query-run', router().router);
    expect(done).toHaveLength(5);
  });

  it('acts on the connection of the tab in front', async () => {
    state.activeTab = { id: 't1', connectionId: 'pg' };
    const r = router();
    await runMenuAction('connection-overview', r.router);
    await runMenuAction('connection-edit', r.router);
    await runMenuAction('connection-duplicate', r.router);
    await runMenuAction('tab-reconnect', r.router);
    expect(calls).toEqual(['overview:pg', 'edit:pg', 'duplicate:pg', 'reconnect:t1']);
  });

  it('greys out what cannot be done now', () => {
    const nothing = menuEnabled({ onWorkspace: true, connections: 0, tab: null });
    expect(Object.keys(nothing).sort()).toEqual([...MENU_ACTIONS].sort());
    expect(nothing['new-connection']).toBe(true);
    expect(nothing['new-query']).toBe(false);
    expect(nothing['close-tab']).toBe(false);
    expect(nothing['query-run']).toBe(false);
    expect(nothing['live:status']).toBe(false);

    const query = menuEnabled({ onWorkspace: true, connections: 2, tab: { kind: 'query', running: false, canRun: true, hasCommands: true } });
    expect(query['query-run']).toBe(true);
    expect(query['query-cancel']).toBe(false);
    expect(query['query-save']).toBe(true);
    expect(query['close-tab']).toBe(true);

    const running = menuEnabled({ onWorkspace: true, connections: 2, tab: { kind: 'query', running: true, canRun: false, hasCommands: true } });
    expect(running['query-run']).toBe(false);
    expect(running['query-cancel']).toBe(true);

    // A tab that is connecting cannot start a run yet; its other query actions stay.
    const connecting = menuEnabled({ onWorkspace: true, connections: 2, tab: { kind: 'query', running: false, canRun: false, hasCommands: true } });
    expect(connecting['query-run']).toBe(false);
    expect(connecting['query-save']).toBe(true);

    // A table tab has no query actions; on another page no tab is in front.
    const table = menuEnabled({ onWorkspace: true, connections: 2, tab: { kind: 'tableData', running: false, canRun: false, hasCommands: false } });
    expect(table['query-save']).toBe(false);
    expect(table['tab-reconnect']).toBe(true);
    const away = menuEnabled({ onWorkspace: false, connections: 2, tab: { kind: 'query', running: false, canRun: true, hasCommands: true } });
    expect(away['query-run']).toBe(false);
    expect(away['close-tab']).toBe(false);
    expect(away['new-query']).toBe(true);
  });
});
