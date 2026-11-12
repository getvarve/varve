import { computed } from 'vue';
import type { Engine } from '../../api/records';
import type { ServerInfo, SessionInfo } from '../../api/sessions';
import { useTabs } from '../../composables/useTabs';
import type { WorkspaceTab } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import { ENGINE_INFO } from '../../lib/display';
import type { TreeNode } from '../../lib/tree';
import { useObjectBrowser } from './objectBrowser';

/** Whether a connection has a live session, and what it has open. */
export interface ConnectionState {
  /** At least one session is open: a tab's or the object browser's. */
  connected: boolean;
  /** Tabs of the connection, connected or not. */
  tabs: number;
  /** Open sessions: one per connected tab, plus the object browser's. */
  sessions: number;
  /** When the oldest open session was opened (ms), `null` without one. */
  since: number | null;
  /** The server as the newest open session reported it, `null` without one. */
  server: ServerInfo | null;
}

export const NOT_CONNECTED: ConnectionState = { connected: false, tabs: 0, sessions: 0, since: null, server: null };

/** The state of one connection from its tabs and the object browser's sessions. */
export function connectionState(tabs: readonly Pick<WorkspaceTab, 'session'>[], browseSessions: readonly SessionInfo[]): ConnectionState {
  const sessions = [...tabs.map((t) => t.session).filter((s): s is SessionInfo => s !== null), ...browseSessions];
  if (sessions.length === 0) return { ...NOT_CONNECTED, tabs: tabs.length };
  const oldest = sessions.reduce((a, b) => (b.openedAt < a.openedAt ? b : a));
  const newest = sessions.reduce((a, b) => (b.openedAt > a.openedAt ? b : a));
  return { connected: true, tabs: tabs.length, sessions: sessions.length, since: oldest.openedAt, server: newest.server };
}

/** `HH:MM` in local time. */
function clock(ms: number): string {
  const d = new Date(ms);
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
}

function plural(n: number, one: string, many: string): string {
  return `${n} ${n === 1 ? one : many}`;
}

/**
 * The tooltip of a connection's engine tile: the engine (with the server
 * version when connected) and whether it is connected, then what it has open.
 */
export function engineTitle(engine: Engine, state: ConnectionState): string {
  const name = ENGINE_INFO[engine].name;
  if (!state.connected) {
    const first = `${name} · Not connected`;
    return state.tabs > 0 ? `${first}\n${plural(state.tabs, 'tab', 'tabs')} open` : first;
  }
  const first = `${state.server ? `${name} ${state.server.version}` : name} · Connected`;
  const since = state.since === null ? '' : ` since ${clock(state.since)}`;
  const second = state.tabs > 0 ? `${plural(state.tabs, 'tab', 'tabs')} open${since}` : `Object browser only${since}`;
  return `${first}\n${second}`;
}

/** The tooltip of a connected row's Close button. */
export function closeTitle(state: ConnectionState): string {
  const ends = `Ends ${plural(state.sessions, 'session', 'sessions')}.`;
  return `Close connection\n${state.tabs > 0 ? `${ends} ${state.tabs === 1 ? 'Its tab stays' : 'Its tabs stay'} open.` : ends}`;
}

/** Connections in a folder (through every subfolder), and how many of them are connected. */
export function folderCounts(node: TreeNode, connected: (connectionId: string) => boolean): { connections: number; connected: number } {
  const counts = { connections: 0, connected: 0 };
  const walk = (n: TreeNode): void => {
    if (n.kind === 'connection') {
      counts.connections += 1;
      if (connected(n.id)) counts.connected += 1;
    } else if (n.kind === 'folder') n.children.forEach(walk);
  };
  node.children.forEach(walk);
  return counts;
}

/** What a collapsed folder shows: "3 · 2 connected", "1", or nothing for a folder without connections. */
export function folderCountText(counts: { connections: number; connected: number }): string | null {
  if (counts.connections === 0) return null;
  return counts.connected > 0 ? `${counts.connections} · ${counts.connected} connected` : String(counts.connections);
}

/** The same, spelled out for screen readers and the tooltip. */
export function folderCountLabel(counts: { connections: number; connected: number }): string | null {
  if (counts.connections === 0) return null;
  const held = plural(counts.connections, 'connection', 'connections');
  return counts.connected > 0 ? `${held}, ${counts.connected} connected` : held;
}

/**
 * The state of every connection, kept up to date as tabs connect and close
 * and the object browser opens and closes its sessions.
 */
export function useConnectionStates() {
  const tabs = useTabs();
  const objects = useObjectBrowser();
  const workspace = useWorkspace();
  const states = computed(() => {
    const byConnection = new Map<string, WorkspaceTab[]>();
    for (const tab of tabs.tabs) {
      const list = byConnection.get(tab.connectionId);
      if (list) list.push(tab);
      else byConnection.set(tab.connectionId, [tab]);
    }
    const out = new Map<string, ConnectionState>();
    for (const profile of workspace.records.value.connections) {
      out.set(profile.id, connectionState(byConnection.get(profile.id) ?? [], objects.sessions(profile.id)));
    }
    return out;
  });
  return {
    states,
    state: (connectionId: string): ConnectionState => states.value.get(connectionId) ?? NOT_CONNECTED,
  };
}
