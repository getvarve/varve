import { presentError, secretLabel } from '../../api/errors';
import type { CmdError } from '../../api/errors';
import type { PendingDeletion, PendingRestore } from '../../api/notices';
import type { ConnectionProfile, Engine } from '../../api/records';
import type { ReloadReport, SessionInfo } from '../../api/sessions';

/**
 * The engine a pending restore's server identity names, or `null` for entries
 * of very old versions (`host:port`), which can only be dismissed.
 */
export function restoreEngine(entry: Pick<PendingRestore, 'server'>): 'mysql' | 'mariadb' | null {
  if (entry.server.startsWith('mysql:')) return 'mysql';
  if (entry.server.startsWith('mariadb:')) return 'mariadb';
  return null;
}

/** MySQL and MariaDB profiles can reach either server; the backend checks the identity. */
function sameFamily(engine: Engine): boolean {
  return engine === 'mysql' || engine === 'mariadb';
}

/** An open session a restore could run on, with the name to show for it. */
export interface RestoreCandidate {
  session: SessionInfo;
  /** The tab or connection name the user recognizes. */
  label: string;
  /** True for a session of the connection that recorded the entry. */
  recording: boolean;
}

/**
 * Open user sessions that can run a restore: those of the recording connection
 * first (the backend then needs no confirmation when the route matches), then
 * every other MySQL or MariaDB session. One entry per session.
 */
export function restoreCandidates(
  entry: Pick<PendingRestore, 'connectionId' | 'server'>,
  open: readonly { session: SessionInfo | null; title: string }[],
): RestoreCandidate[] {
  if (restoreEngine(entry) === null) return [];
  const seen = new Set<string>();
  const out: RestoreCandidate[] = [];
  for (const tab of open) {
    const session = tab.session;
    if (!session || seen.has(session.id) || !sameFamily(session.engine)) continue;
    seen.add(session.id);
    out.push({ session, label: tab.title, recording: session.connectionId === entry.connectionId });
  }
  return out.sort((a, b) => Number(b.recording) - Number(a.recording));
}

/** `recordedAt` in the user's time zone; older entries have no time. */
export function formatRecordedAt(ms: number, locale?: string): string {
  if (!Number.isFinite(ms) || ms <= 0) return 'at an unknown time';
  return new Date(ms).toLocaleString(locale, { dateStyle: 'medium', timeStyle: 'short' });
}

/** The headline of a pending restore banner. */
export function restoreHeadline(entry: Pick<PendingRestore, 'database' | 'collation'>): string {
  return `The default collation of database ${entry.database} was left changed (it should be ${entry.collation}).`;
}

/** What the banner shows after a failed restore, and whether it offers Dismiss. */
export interface RestoreProblem {
  message: string;
  offerDismiss: boolean;
}

/** Turns a failed `ddl_restore_collation` into the banner's message. */
export function restoreProblem(error: CmdError): RestoreProblem {
  if (error.kind === 'notFound') {
    return {
      message:
        'Nothing is pending for this database on the server this tab is connected to: the tab is on another server. Pick a tab of the server the restore was recorded on.',
      offerDismiss: false,
    };
  }
  if (error.kind === 'invalid') {
    // Stale: another server with the same identity, a recreated database, or a change by someone else.
    return { message: `${error.message} If the database is fine as it is, dismiss this notice.`, offerDismiss: true };
  }
  const presented = presentError(error);
  const extra = presented.detail ? ` ${presented.detail}` : '';
  return { message: `${presented.message}${extra}`, offerDismiss: false };
}

/** One line per pending keychain removal: the connection name (or that it was deleted) and the secret. */
export function keychainLines(
  pending: readonly PendingDeletion[],
  connection: (id: string) => Pick<ConnectionProfile, 'name'> | null,
): string[] {
  return pending.map((p) => {
    const name = connection(p.connectionId)?.name;
    const owner = name ? `connection ${name}` : 'a deleted connection';
    return `The ${secretLabel(p.kind)} of ${owner}`;
  });
}

/**
 * The notice after a reload of the window: how many sessions the reload
 * closed and where it rolled back open transactions or cancelled running
 * statements. `null` when it closed nothing (the first load).
 */
export function reloadNoticeText(report: ReloadReport, nameOf: (connectionId: string) => string): string | null {
  const closed = report.sessions.length + report.opening.length;
  if (closed === 0) return null;
  const names = (pick: (s: ReloadReport['sessions'][number]) => boolean): string[] => [
    ...new Set(report.sessions.filter(pick).map((s) => nameOf(s.connectionId))),
  ];
  const rolledBack = names((s) => s.inTransaction === true);
  const running = names((s) => s.inTransaction === null);
  let text = `The window was reloaded; ${closed} ${closed === 1 ? 'session was' : 'sessions were'} closed.`;
  if (rolledBack.length > 0) text += ` Open transactions in ${rolledBack.join(', ')} were rolled back.`;
  if (running.length > 0) text += ` Statements still running in ${running.join(', ')} were cancelled and rolled back.`;
  return text;
}

