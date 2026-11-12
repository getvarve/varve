import type { ConnectionProfile } from '../../api/records';
import { busySessions, openTransactions } from '../../api/sessions';
import { useDialogs } from '../../composables/useDialogs';
import { hasUnsavedEdits } from '../../composables/useQuerySave';
import { useTabs } from '../../composables/useTabs';
import type { WorkspaceTab } from '../../composables/useTabs';
import { useObjectBrowser } from './objectBrowser';

/** What closing a connection would interrupt, to ask about first. */
export interface CloseRisks {
  /** Titles of tabs with text or changes that are not saved or applied. */
  unsaved: string[];
  /** Titles of tabs with a statement still running. */
  running: string[];
  /**
   * Sessions of the connection that are working on something other than a
   * query tab's statement (a designer or DDL apply, a grid save); `null`
   * when that could not be checked.
   */
  working: number | null;
  /** Sessions of the connection with a transaction open; `null` when that could not be checked. */
  transactions: number | null;
}

/** Whether a tab holds work that is not saved (query text) or not applied (editors). */
export function tabHasUnsavedWork(tab: WorkspaceTab): boolean {
  return tab.kind === 'query' ? hasUnsavedEdits(tab) : tab.unsaved;
}

function quoted(titles: readonly string[]): string {
  return titles.map((t) => `“${t}”`).join(', ');
}

/**
 * The one question asked before closing a connection, about every tab at
 * once; `null` when nothing would be interrupted and the connection closes
 * without asking.
 */
export function closeQuestion(name: string, risks: CloseRisks): { title: string; message: string; danger: boolean } | null {
  const { transactions, working } = risks;
  if (risks.unsaved.length === 0 && risks.running.length === 0 && transactions === 0 && working === 0) return null;
  const parts: string[] = [];
  if (working === null) parts.push('Varve could not check whether a session is still working. Work in progress is cancelled.');
  else if (working === 1) parts.push('A session is still working. Its work is cancelled.');
  else if (working > 1) parts.push(`${working} sessions are still working. Their work is cancelled.`);
  if (transactions === null) parts.push('Varve could not check for open transactions. Any open transaction is rolled back.');
  else if (transactions === 1) parts.push('A transaction is open. It is rolled back.');
  else if (transactions > 1) parts.push(`${transactions} transactions are open. They are rolled back.`);
  if (risks.running.length > 0) {
    parts.push(
      risks.running.length === 1
        ? `A statement is still running in ${quoted(risks.running)}. It is cancelled.`
        : `Statements are still running in ${quoted(risks.running)}. They are cancelled.`,
    );
  }
  if (risks.unsaved.length > 0) {
    parts.push(
      risks.unsaved.length === 1
        ? `${quoted(risks.unsaved)} has changes that are not saved or applied. The tab stays open and keeps them.`
        : `${quoted(risks.unsaved)} have changes that are not saved or applied. The tabs stay open and keep them.`,
    );
  }
  return {
    title: `Close connection “${name}”?`,
    message: parts.join(' '),
    // Rolling back or cancelling loses work in the database; unsaved tab work stays in its tab.
    danger: transactions !== 0 || working !== 0 || risks.running.length > 0,
  };
}

/**
 * Sessions of a connection that are working now, apart from `known` (the
 * sessions of query tabs with a running statement, which the question names
 * by tab); `null` when the backend cannot say.
 */
async function workingSessions(connectionId: string, known: ReadonlySet<string>): Promise<number | null> {
  try {
    return (await busySessions(connectionId)).filter((id) => !known.has(id)).length;
  } catch {
    return null;
  }
}

/** Sessions of a connection with an open transaction, from the backend; `null` when it cannot say. */
async function transactionsOf(connectionId: string): Promise<number | null> {
  try {
    return (await openTransactions()).filter((id) => id === connectionId).length;
  } catch {
    return null;
  }
}

/** Connections being closed now: a second request for one of them (a double click) does nothing. */
const closing = new Set<string>();

/**
 * Closes a connection: ends the sessions of all its tabs and of the object
 * browser. The tabs stay open, not connected, and connect again when used;
 * the backend closes the pooled sessions and the SSH tunnel with the last
 * user session. Tabs with unsaved work, a running statement, a session that
 * is working on something else or an open transaction are asked about
 * first, in one question. Resolves `true` when the connection was closed,
 * after the backend has closed every session.
 */
export async function closeConnection(profile: ConnectionProfile): Promise<boolean> {
  if (closing.has(profile.id)) return false;
  closing.add(profile.id);
  try {
    const tabs = useTabs();
    const own = tabs.tabs.filter((t) => t.connectionId === profile.id);
    const running = own.filter((t) => t.kind === 'query' && t.running && t.session !== null);
    // One after the other: the transaction check uses each idle session for a moment, which would look like work.
    const working = await workingSessions(profile.id, new Set(running.map((t) => t.session?.id ?? '')));
    const risks: CloseRisks = {
      unsaved: own.filter(tabHasUnsavedWork).map((t) => t.title),
      running: running.map((t) => t.title),
      working,
      transactions: await transactionsOf(profile.id),
    };
    const question = closeQuestion(profile.name, risks);
    if (question) {
      const ok = await useDialogs().askConfirm({ ...question, confirmLabel: 'Close connection' });
      if (!ok) return false;
    }
    await Promise.all([useObjectBrowser().disconnectAndWait(profile.id), tabs.disconnectConnection(profile.id)]);
    return true;
  } finally {
    closing.delete(profile.id);
  }
}
