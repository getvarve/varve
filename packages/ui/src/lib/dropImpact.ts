/**
 * What the drop flows of the object editors say about a drop's impact
 * (`catalog_drop_impact`, app/src-tauri/src/m2/impact.rs): dependents,
 * contents, and for a database the sessions connected to it, per engine.
 */
import type { ContentCount, DropImpact, ObjectKind } from '../api/ddl';
import type { Engine } from '../api/records';

/** Engines with a catalog (Redis has none). */
type SqlEngine = Exclude<Engine, 'redis'>;

/** `3 tables, 1 view`: the contents that go with the object. */
export function contentsText(contents: readonly ContentCount[]): string {
  return contents.map((c) => `${c.count} ${c.count === '1' ? singular(c.label) : c.label}`).join(', ');
}

const SINGULAR: Record<string, string> = {
  indexes: 'index',
  policies: 'policy',
  'materialized views': 'materialized view',
  'foreign tables': 'foreign table',
};

function singular(label: string): string {
  return SINGULAR[label] ?? label.replace(/s$/, '');
}

function count(n: number, one: string, many: string): string {
  return `${n} ${n === 1 ? one : many}`;
}

/** What the dependents mean for this drop, or `null` when there are none. */
export function dependentsNote(engine: SqlEngine, impact: DropImpact, cascade: boolean): string | null {
  const blocking = impact.dependents.filter((d) => d.effect === 'blocks').length;
  const breaking = impact.dependents.filter((d) => d.effect === 'breaks').length;
  const more = impact.dependentsTruncated ? ' (and more)' : '';
  const parts: string[] = [];
  if (blocking > 0) {
    if (engine === 'postgres') {
      parts.push(
        cascade
          ? `CASCADE also drops ${count(blocking, 'object', 'objects')} that depend on it${more}.`
          : `${count(blocking, 'object depends', 'objects depend')} on it${more}: the drop fails unless you choose CASCADE, which drops them too.`,
      );
    } else {
      parts.push(`${count(blocking, 'foreign key references', 'foreign keys reference')} it${more}: the server refuses the drop until they are dropped.`);
    }
  }
  if (breaking > 0) parts.push(`${count(breaking, 'view uses', 'views use')} it${more}: they stay but fail until it exists again.`);
  const maybe = impact.dependents.filter((d) => d.effect === 'mayBreak').length;
  if (maybe > 0) {
    parts.push(
      `${count(maybe, 'routine, trigger or event names', 'routines, triggers or events name')} it${more}: they may fail once it is gone (the server does not track what they use; found by name).`,
    );
  }
  return parts.length > 0 ? parts.join(' ') : null;
}

/** How the engine treats the other sessions of a database being dropped. */
export function sessionsNote(engine: SqlEngine, impact: DropImpact, force: boolean): string | null {
  const n = impact.sessions.length;
  const varve = impact.sessions.filter((s) => s.varve).length;
  const own =
    varve > 0
      ? ` ${count(varve, 'of them is a Varve session', 'of them are Varve sessions')}: the object browser's own connections close by themselves before the drop; close tabs of this connection that use the database.`
      : '';
  const partial = impact.sessionsComplete ? '' : ' Your account may not see every connection (the PROCESS privilege shows all).';
  if (engine === 'postgres') {
    if (n === 0) return 'No other session is connected to this database.';
    if (force) {
      return `WITH (FORCE) terminates the ${count(n, 'other session', 'other sessions')} first; their open transactions are rolled back.${own}`;
    }
    return `PostgreSQL refuses to drop a database while other sessions are connected: ${count(n, 'session is', 'sessions are')} connected now. Ask their users to disconnect (or close those tabs) and retry, or choose to terminate them.${own}`;
  }
  if (n === 0) return `No other connection uses this database as its default database.${partial}`;
  return `MySQL and MariaDB do not refuse: ${count(n, 'connection uses', 'connections use')} this database as default database and keep running without one. A connection holding a lock on one of its tables (an open transaction) makes the drop wait, up to lock_wait_timeout.${partial}`;
}

/** Whether a plain drop is expected to fail with these dependents (the user can still try). */
export function restrictFails(engine: SqlEngine, impact: DropImpact, cascade: boolean): boolean {
  const blocking = impact.dependents.some((d) => d.effect === 'blocks');
  return blocking && (engine !== 'postgres' || !cascade);
}

/** The heading of the impact panel for a kind. */
export function impactHeading(kind: ObjectKind): string {
  switch (kind) {
    case 'database':
      return 'What the drop affects';
    case 'schema':
      return 'Objects in the schema and what depends on them';
    default:
      return 'What depends on it';
  }
}

/** One of this app's tabs on the database being dropped. */
export interface AppTab {
  title: string;
  busy: boolean;
  openTransaction: boolean | null;
  /** Known to be on the same server; `false`: only may be (its server could not be identified). */
  certain: boolean;
  /** On a PostgreSQL streaming standby of the server. */
  standby: boolean;
}

function tabText(tab: AppTab): string {
  if (tab.busy) return `"${tab.title}" (running a statement)`;
  if (tab.openTransaction === true) return `"${tab.title}" (open transaction)`;
  return `"${tab.title}"`;
}

/** The tabs Varve could not tie to the server for sure, as a sentence, or blank. */
function possibleTabs(tabs: readonly AppTab[]): string {
  const maybe = tabs.filter((t) => !t.certain && !t.standby);
  if (maybe.length === 0) return '';
  return ` May also include ${maybe.map(tabText).join(', ')}: ${maybe.length === 1 ? 'its server' : 'their server'} could not be identified, but the connection points at the same address.`;
}

/**
 * Tabs on a PostgreSQL streaming standby, as a sentence, or blank: the drop
 * replays there and ends them whether or not FORCE is chosen, and they never
 * make the primary refuse the drop.
 */
function standbyTabs(tabs: readonly AppTab[]): string {
  const standby = tabs.filter((t) => t.standby);
  if (standby.length === 0) return '';
  return ` On a standby of this server: ${standby.map(tabText).join(', ')}. ${standby.length === 1 ? 'It is' : 'They are'} ended when the drop replays on the standby, with or without FORCE (open transactions there are rolled back); ${standby.length === 1 ? 'it does' : 'they do'} not stop the drop.`;
}

/**
 * What a database drop does to this app's tabs on it, or `null` when there
 * are none: PostgreSQL refuses while they are connected, WITH (FORCE) ends
 * them (open transactions are rolled back); MySQL and MariaDB leave them
 * without their default database. Tabs whose server could not be
 * identified are named as "may also include", never asserted.
 */
export function appTabsNote(engine: SqlEngine, tabs: readonly AppTab[], force: boolean): string | null {
  if (tabs.length === 0) return null;
  return appTabsText(engine, tabs, force) + COPY_LIMIT;
}

/**
 * Known limit of matching tabs by the server's identity: a server and a
 * copy of it (a cold copy of its data directory, a restored snapshot)
 * report the same identity, so "same server or a copy of it" cannot be told
 * apart.
 */
export const COPY_LIMIT = ' Tabs are matched by the identity the server reports, so the same server and a copy of it (for example a cold copy of its data directory) cannot be told apart.';

function appTabsText(engine: SqlEngine, tabs: readonly AppTab[], force: boolean): string {
  const sure = tabs.filter((t) => t.certain && !t.standby);
  const maybe = possibleTabs(tabs) + standbyTabs(tabs);
  if (sure.length === 0) return `No tab in this app is known to use this database on this server.${maybe}`;
  const list = sure.map(tabText).join(', ');
  const open = sure.some((t) => t.openTransaction === true || t.busy);
  if (engine === 'postgres') {
    if (force) {
      return `WITH (FORCE) also ends ${count(sure.length, 'tab', 'tabs')} in this app on the database: ${list}.${open ? ' Their open transactions and running statements are rolled back.' : ''} Reconnecting them fails once the database is gone.${maybe}`;
    }
    return `${count(sure.length, 'tab', 'tabs')} in this app ${sure.length === 1 ? 'uses' : 'use'} the database, so PostgreSQL refuses the drop until they are closed: ${list}.${maybe}`;
  }
  return `${count(sure.length, 'tab', 'tabs')} in this app ${sure.length === 1 ? 'uses' : 'use'} it as default database and lose it: ${list}.${open ? ' A tab whose open transaction used its tables makes the drop wait until that transaction ends.' : ''}${maybe}`;
}
