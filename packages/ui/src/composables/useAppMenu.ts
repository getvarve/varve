import { isTauri } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { watch } from 'vue';
import type { Router } from 'vue-router';
import type { ConnectionProfile } from '../api/records';
import { setMenuEnabled } from '../backend';
import { canRun } from '../components/query/runOnTab';
import { openConnectionOverview } from '../components/tree/openConnection';
import { showWorkspace } from '../components/tree/showWorkspace';
import type { LivePanel } from '../lib/tree';
import { useDialogs } from './useDialogs';
import { useSidebar } from './useSidebar';
import { tabCommands } from './useTabCommands';
import { useTabs } from './useTabs';
import { useToasts } from './useToasts';
import { useWorkspace } from './useWorkspace';
import { useWorkspaceActions } from './useWorkspaceActions';

/** Event the backend sends for a menu bar item the window carries out (app/src-tauri/src/menu.rs). */
export const MENU_ACTION = 'menu://action';

/** Every action of the menu bar. A test on the backend checks that each of its items is listed here. */
export const MENU_ACTIONS = [
  'new-connection',
  'new-folder',
  'new-query',
  'close-tab',
  'show-workspace',
  'show-activity',
  'show-settings',
  'toggle-sidebar',
  'fit-sidebar',
  'live:status',
  'live:processes',
  'live:variables',
  'query-run',
  'query-cancel',
  'query-save',
  'query-save-as',
  'query-history',
  'connection-overview',
  'connection-edit',
  'connection-duplicate',
  'tab-reconnect',
] as const;

export type MenuAction = (typeof MENU_ACTIONS)[number];

export function isMenuAction(value: unknown): value is MenuAction {
  return typeof value === 'string' && (MENU_ACTIONS as readonly string[]).includes(value);
}

/** The live panel a Tools item opens on this engine; Redis has its own panels for the same questions. */
export function livePanelFor(action: 'live:status' | 'live:processes' | 'live:variables', engine: ConnectionProfile['engine']): LivePanel {
  const redis = engine === 'redis';
  if (action === 'live:status') return redis ? 'redisInfo' : 'serverStatus';
  if (action === 'live:processes') return redis ? 'redisClients' : 'processes';
  return redis ? 'redisConfig' : 'variables';
}

/** The connection of the tab in front, or the one the user picks when no tab is. Null when they cancel. */
async function connectionFor(title: string): Promise<ConnectionProfile | null> {
  const tab = useTabs().active.value;
  const current = tab ? useWorkspace().connection(tab.connectionId) : null;
  if (current) return current;
  if (useWorkspace().records.value.connections.length === 0) {
    useToasts().push('info', 'No connection yet', 'Make a connection first: File, New Connection.');
    return null;
  }
  return useDialogs().pickConnection({ title });
}

/** What decides which menu items can be used now. */
export interface MenuState {
  /** The workspace page is on screen (the tabs are). */
  onWorkspace: boolean;
  /** How many connections exist. */
  connections: number;
  /** The tab in front, if any. */
  tab: {
    kind: string;
    running: boolean;
    /** Run can start (a query tab that is not connected connects first; see `canRun`). */
    canRun: boolean;
    hasCommands: boolean;
  } | null;
}

/** Which menu bar actions can be carried out in this state. Every action is listed. */
export function menuEnabled(state: MenuState): Record<MenuAction, boolean> {
  const tab = state.onWorkspace ? state.tab : null;
  const query = tab !== null && tab.kind === 'query' && tab.hasCommands;
  const anyConnection = state.connections > 0;
  return {
    'new-connection': true,
    'new-folder': true,
    'new-query': anyConnection,
    'close-tab': tab !== null,
    'show-workspace': true,
    'show-activity': true,
    'show-settings': true,
    'toggle-sidebar': true,
    'fit-sidebar': true,
    'live:status': anyConnection,
    'live:processes': anyConnection,
    'live:variables': anyConnection,
    'query-run': query && tab.canRun,
    'query-cancel': query && tab.running,
    'query-save': query,
    'query-save-as': query,
    'query-history': query,
    'connection-overview': anyConnection,
    'connection-edit': anyConnection,
    'connection-duplicate': anyConnection,
    'tab-reconnect': tab !== null,
  };
}

/** Carries out one menu bar action. */
export async function runMenuAction(action: MenuAction, router: Router): Promise<void> {
  const tabs = useTabs();
  switch (action) {
    case 'new-connection':
      await useWorkspaceActions().newConnection();
      break;
    case 'new-folder':
      await useWorkspaceActions().newFolder(null);
      break;
    case 'new-query': {
      const profile = await connectionFor('New query on');
      if (!profile) return;
      tabs.openQueryTab(profile);
      showWorkspace(router);
      break;
    }
    case 'close-tab': {
      // Only on the workspace page: elsewhere no tab is in front to close.
      const tab = tabs.active.value;
      if (tab && router.currentRoute.value.name === 'home') await tabs.closeTab(tab.id);
      break;
    }
    case 'show-workspace':
      showWorkspace(router);
      break;
    case 'show-activity':
      await router.push({ name: 'activity' });
      break;
    case 'show-settings':
      await router.push({ name: 'settings' });
      break;
    case 'toggle-sidebar':
      useSidebar().toggle();
      break;
    case 'fit-sidebar':
      useSidebar().requestFit();
      break;
    case 'query-run':
    case 'query-cancel':
    case 'query-save':
    case 'query-save-as':
    case 'query-history': {
      const tab = tabs.active.value;
      const commands = tab && router.currentRoute.value.name === 'home' ? tabCommands(tab.id) : null;
      if (!commands) return;
      if (action === 'query-run') commands.run();
      else if (action === 'query-cancel') commands.cancel();
      else if (action === 'query-save') commands.save();
      else if (action === 'query-save-as') commands.saveAs();
      else commands.toggleHistory();
      break;
    }
    case 'connection-overview': {
      const profile = await connectionFor('Open the overview of');
      if (!profile) return;
      openConnectionOverview(profile);
      showWorkspace(router);
      break;
    }
    case 'connection-edit': {
      const profile = await connectionFor('Edit');
      if (profile) await useWorkspaceActions().editConnection(profile);
      break;
    }
    case 'connection-duplicate': {
      const profile = await connectionFor('Duplicate');
      if (profile) await useWorkspaceActions().duplicate(profile);
      break;
    }
    case 'tab-reconnect': {
      const tab = tabs.active.value;
      if (tab && router.currentRoute.value.name === 'home') await tabs.reconnect(tab.id);
      break;
    }
    case 'live:status':
    case 'live:processes':
    case 'live:variables': {
      const profile = await connectionFor('Open on');
      if (!profile) return;
      tabs.openLiveTab(profile, livePanelFor(action, profile.engine));
      showWorkspace(router);
      break;
    }
  }
}

/**
 * Follows the menu bar: runs each action the backend sends. Returns the function that stops it. Outside the
 * desktop app (tests, a browser preview) there is no menu bar and nothing to follow.
 */
export async function installAppMenu(router: Router): Promise<() => void> {
  if (!isTauri()) return () => undefined;
  const stopListening = await listen<string>(MENU_ACTION, (event) => {
    if (!isMenuAction(event.payload)) return;
    runMenuAction(event.payload, router).catch((error: unknown) => {
      useToasts().pushError(error, 'Could not carry out the menu action');
    });
  });
  // Items that cannot work now are greyed out, and follow the tab in front, the page and the connections.
  const stopWatching = watch(
    () => {
      const tab = useTabs().active.value;
      return menuEnabled({
        onWorkspace: router.currentRoute.value.name === 'home',
        connections: useWorkspace().records.value.connections.length,
        tab: tab
          ? {
              kind: tab.kind,
              running: tab.kind === 'query' && tab.running,
              canRun: tab.kind === 'query' && canRun(tab, useWorkspace().connection(tab.connectionId) !== null),
              hasCommands: tabCommands(tab.id) !== null,
            }
          : null,
      });
    },
    (enabled, before) => {
      // Only what changed: the first time, everything.
      const changed = Object.fromEntries(Object.entries(enabled).filter(([action, on]) => before?.[action as MenuAction] !== on));
      if (Object.keys(changed).length === 0) return;
      setMenuEnabled(changed).catch((error: unknown) => {
        console.error('Could not update the menu bar', error);
      });
    },
    { immediate: true },
  );
  return () => {
    stopListening();
    stopWatching();
  };
}
