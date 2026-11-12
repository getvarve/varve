import { isTauri } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import type { Router } from 'vue-router';
import type { ConnectionProfile } from '../api/records';
import { showWorkspace } from '../components/tree/showWorkspace';
import type { LivePanel } from '../lib/tree';
import { useDialogs } from './useDialogs';
import { useSidebar } from './useSidebar';
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
  return listen<string>(MENU_ACTION, (event) => {
    if (!isMenuAction(event.payload)) return;
    runMenuAction(event.payload, router).catch((error: unknown) => {
      useToasts().pushError(error, 'Could not carry out the menu action');
    });
  });
}
