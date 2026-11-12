import type { Folder } from '../../api/records';
import type { SessionInfo } from '../../api/sessions';
import type { TreeNode } from '../../lib/tree';
import { closeTitle, connectionState, engineTitle, folderCountLabel, folderCounts, folderCountText, NOT_CONNECTED } from './connectionState';

function session(id: string, openedAt: number, version = '16.2'): SessionInfo {
  return {
    id,
    connectionId: 'c1',
    engine: 'postgres',
    server: { engine: 'postgres', version, major: 16, minor: 2, patch: null },
    environment: 'production',
    database: 'shop',
    readOnly: false,
    confirmDestructive: true,
    purpose: 'user',
    openedAt,
    encryption: { tls: true, mode: 'prefer', passwordUnencrypted: false },
  };
}

/** Local time today at hh:mm, in ms. */
function at(hours: number, minutes: number): number {
  const d = new Date();
  d.setHours(hours, minutes, 0, 0);
  return d.getTime();
}

describe('connectionState', () => {
  it('is not connected without sessions, and still counts the tabs', () => {
    expect(connectionState([], [])).toEqual(NOT_CONNECTED);
    expect(connectionState([{ session: null }, { session: null }], [])).toEqual({ ...NOT_CONNECTED, tabs: 2 });
  });

  it('counts the sessions of tabs and of the object browser, from the oldest one', () => {
    const state = connectionState([{ session: session('t1', at(10, 0)) }, { session: null }], [session('b1', at(9, 14), '16.3')]);
    expect(state).toEqual({ connected: true, tabs: 2, sessions: 2, since: at(9, 14), server: session('t1', 0).server });
  });

  it('is connected through the object browser alone', () => {
    const state = connectionState([], [session('b1', at(8, 5))]);
    expect(state.connected).toBe(true);
    expect(state.tabs).toBe(0);
    expect(state.sessions).toBe(1);
  });
});

describe('engineTitle', () => {
  it('names the engine and says it is not connected', () => {
    expect(engineTitle('postgres', NOT_CONNECTED)).toBe('PostgreSQL · Not connected');
    expect(engineTitle('redis', { ...NOT_CONNECTED, tabs: 1 })).toBe('Redis · Not connected\n1 tab open');
  });

  it('says what is open and since when on a second line', () => {
    const one = connectionState([{ session: session('t1', at(9, 14), '7.4.1') }], []);
    expect(engineTitle('redis', one)).toBe('Redis 7.4.1 · Connected\n1 tab open since 09:14');
    const three = connectionState([{ session: session('a', at(13, 2)) }, { session: session('b', at(14, 0)) }, { session: null }], []);
    expect(engineTitle('postgres', three)).toBe('PostgreSQL 16.2 · Connected\n3 tabs open since 13:02');
    const browser = connectionState([], [session('b', at(7, 30))]);
    expect(engineTitle('postgres', browser)).toBe('PostgreSQL 16.2 · Connected\nObject browser only since 07:30');
  });
});

describe('closeTitle', () => {
  it('says how many sessions end and that the tabs stay', () => {
    expect(closeTitle({ ...NOT_CONNECTED, connected: true, tabs: 2, sessions: 3 })).toBe('Close connection\nEnds 3 sessions. Its tabs stay open.');
    expect(closeTitle({ ...NOT_CONNECTED, connected: true, tabs: 1, sessions: 1 })).toBe('Close connection\nEnds 1 session. Its tab stays open.');
    expect(closeTitle({ ...NOT_CONNECTED, connected: true, tabs: 0, sessions: 1 })).toBe('Close connection\nEnds 1 session.');
  });
});

describe('folder counts', () => {
  const folder = (id: string, children: TreeNode[]): TreeNode => ({
    kind: 'folder',
    key: `folder:${id}`,
    id,
    name: id,
    parentId: null,
    folder: { id } as Folder,
    children,
  });
  const connection = (id: string): TreeNode =>
    ({ kind: 'connection', key: `connection:${id}`, id, name: id, parentId: null, children: [] }) as unknown as TreeNode;
  const query = (id: string): TreeNode => ({ kind: 'query', key: `query:${id}`, id, name: id, parentId: null, children: [] }) as unknown as TreeNode;

  it('counts connections through every subfolder, and the connected ones', () => {
    const webshop = folder('webshop', [connection('a'), folder('inner', [connection('b'), query('q')]), connection('c')]);
    const counts = folderCounts(webshop, (id) => id !== 'b');
    expect(counts).toEqual({ connections: 3, connected: 2 });
    expect(folderCountText(counts)).toBe('3 · 2 connected');
    expect(folderCountLabel(counts)).toBe('3 connections, 2 connected');
  });

  it('shows only the count when none is connected, and nothing without connections', () => {
    const legacy = folderCounts(folder('legacy', [connection('a')]), () => false);
    expect(folderCountText(legacy)).toBe('1');
    expect(folderCountLabel(legacy)).toBe('1 connection');
    const empty = folderCounts(folder('scratch', [query('q')]), () => true);
    expect(folderCountText(empty)).toBeNull();
    expect(folderCountLabel(empty)).toBeNull();
  });
});
