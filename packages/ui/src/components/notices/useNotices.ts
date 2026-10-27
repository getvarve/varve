import { reactive, watch } from 'vue';
import { toCmdError } from '../../api/errors';
import {
  ddlDismissPendingRestore,
  ddlPendingRestores,
  ddlRestoreCollation,
  pendingKeychainDeletions,
  retryKeychainCleanup,
} from '../../api/notices';
import type { PendingDeletion, PendingRestore } from '../../api/notices';
import { closeSession, openSession } from '../../api/sessions';
import { withConnectPrompts } from '../../composables/useConnect';
import { useDialogs } from '../../composables/useDialogs';
import { useTabs } from '../../composables/useTabs';
import { useToasts } from '../../composables/useToasts';
import { useWorkspace } from '../../composables/useWorkspace';
import { runWithConfirmation } from '../../lib/destructive';
import { restoreProblem } from './notices';
import type { RestoreProblem } from './notices';

/** A stable key per pending restore (at most one entry per server and database). */
export function restoreKey(entry: Pick<PendingRestore, 'server' | 'database'>): string {
  return `${entry.server}\u0000${entry.database}`;
}

interface NoticeState {
  restores: PendingRestore[];
  /** Per restore key: a run in progress. */
  busy: Set<string>;
  /** Per restore key: the problem of the last attempt. */
  problems: Map<string, RestoreProblem>;
  restoreError: string | null;
  keychain: PendingDeletion[];
  /** Messages `retry_keychain_cleanup` returned for entries that still fail. */
  keychainMessages: string[];
  keychainError: string | null;
  retrying: boolean;
}

const state = reactive<NoticeState>({
  restores: [],
  busy: new Set(),
  problems: new Map(),
  restoreError: null,
  keychain: [],
  keychainMessages: [],
  keychainError: null,
  retrying: false,
});

let restoreRequest = 0;
let keychainRequest = 0;

/** Reloads the pending collation restores. Call at start and after a DDL run that warned or failed. */
async function refreshRestores(): Promise<void> {
  restoreRequest += 1;
  const request = restoreRequest;
  try {
    const list = await ddlPendingRestores();
    if (request !== restoreRequest) return;
    state.restores = list;
    state.restoreError = null;
    const live = new Set(list.map(restoreKey));
    for (const key of [...state.problems.keys()]) if (!live.has(key)) state.problems.delete(key);
  } catch (e: unknown) {
    if (request === restoreRequest) state.restoreError = `Could not read the pending collation restores: ${toCmdError(e).kind === 'internal' ? 'the app backend failed' : 'the request was refused'}.`;
  }
}

/** Reloads the keychain entries still to be removed. */
async function refreshKeychain(): Promise<void> {
  keychainRequest += 1;
  const request = keychainRequest;
  try {
    const list = await pendingKeychainDeletions();
    if (request !== keychainRequest) return;
    state.keychain = list;
    if (list.length === 0) state.keychainMessages = [];
    state.keychainError = null;
  } catch {
    if (request === keychainRequest) state.keychainError = 'Could not read the keychain cleanup list.';
  }
}

/** Retries the keychain cleanup now and shows what still fails. */
async function retryKeychain(): Promise<void> {
  if (state.retrying) return;
  state.retrying = true;
  try {
    state.keychainMessages = await retryKeychainCleanup();
    state.keychainError = null;
  } catch (e: unknown) {
    const cmd = toCmdError(e);
    state.keychainError = 'message' in cmd ? cmd.message : 'The keychain cleanup failed.';
  } finally {
    state.retrying = false;
    await refreshKeychain();
  }
}

/** Runs `work` with a session on `connectionId`: an open one, or a new one closed afterwards. */
async function withSession(
  pick: { sessionId: string } | { connectionId: string },
  work: (sessionId: string) => Promise<void>,
): Promise<boolean> {
  if ('sessionId' in pick) {
    await work(pick.sessionId);
    return true;
  }
  const profile = useWorkspace().connection(pick.connectionId);
  if (!profile) throw new Error('The connection that recorded this restore no longer exists. Pick an open tab of the same server.');
  const info = await withConnectPrompts(profile, (secrets) => openSession(profile.id, secrets));
  if (!info) return false;
  try {
    await work(info.id);
  } finally {
    await closeSession(info.id).catch(() => undefined);
  }
  return true;
}

/** The name of the connection a restore runs on, for the confirmation's name gate. */
function connectionNameFor(pick: { sessionId: string } | { connectionId: string }, entry: PendingRestore): string {
  const connectionId =
    'connectionId' in pick ? pick.connectionId : (useTabs().tabs.find((t) => t.session?.id === pick.sessionId)?.connectionId ?? entry.connectionId);
  return useWorkspace().connection(connectionId)?.name ?? entry.route;
}

/**
 * Restores a database's collation through an open session, or through a new
 * session of the recording connection. A restore Varve cannot prove safe
 * (unverified server, other route) shows the backend's statement verbatim and
 * runs again only after a yes. Resolves true when the entry was cleared.
 */
async function restore(
  entry: PendingRestore,
  pick: { sessionId: string } | { connectionId: string },
): Promise<boolean> {
  const key = restoreKey(entry);
  if (state.busy.has(key)) return false;
  state.busy.add(key);
  state.problems.delete(key);
  let cleared = false;
  try {
    const ran = await withSession(pick, async (sessionId) => {
      const result = await runWithConfirmation(
        (confirmed) => ddlRestoreCollation(sessionId, entry.database, confirmed).then(() => true),
        // The destructive dialog, as for every confirmationRequired: it follows
        // the environment (a production session asks for the connection name).
        (request) =>
          useDialogs().askDestructive({
            connectionName: connectionNameFor(pick, entry),
            environment: request.environment,
            statements: request.statements,
          }),
      );
      cleared = result === true;
    });
    if (ran && cleared) useToasts().push('success', `Collation of ${entry.database} restored`, `It is back at ${entry.collation}.`);
  } catch (e: unknown) {
    state.problems.set(key, restoreProblem(toCmdError(e)));
  } finally {
    state.busy.delete(key);
    await refreshRestores();
  }
  return cleared;
}

/** Asks, then forgets a pending restore without touching the server. */
async function dismiss(entry: PendingRestore): Promise<boolean> {
  const ok = await useDialogs().askConfirm({
    title: `Dismiss the restore of ${entry.database}?`,
    message: 'Varve will forget this restore; the database keeps its current collation.',
    confirmLabel: 'Dismiss',
    danger: false,
  });
  if (!ok) return false;
  const key = restoreKey(entry);
  state.busy.add(key);
  try {
    await ddlDismissPendingRestore(entry.server, entry.database);
  } catch (e: unknown) {
    const cmd = toCmdError(e);
    // Already gone (restored or dismissed elsewhere): the refresh below shows that.
    if (cmd.kind !== 'notFound') state.problems.set(key, restoreProblem(cmd));
  } finally {
    state.busy.delete(key);
    await refreshRestores();
  }
  return true;
}

const POLL_MS = 30_000;
let started = false;
let poll: ReturnType<typeof setInterval> | null = null;

/**
 * Loads both lists once and keeps them current: again whenever the
 * connection records change (an edit or delete may leave keychain entries
 * behind), and every 30 s while keychain entries are pending (the backend
 * retries in the background). Safe to call more than once.
 */
function start(): void {
  if (started) return;
  started = true;
  void refreshRestores();
  void refreshKeychain();
  watch(
    () => useWorkspace().records.value,
    () => {
      void refreshKeychain();
    },
  );
  watch(
    () => state.keychain.length > 0,
    (pending) => {
      if (pending && poll === null) poll = setInterval(() => void refreshKeychain(), POLL_MS);
      if (!pending && poll !== null) {
        clearInterval(poll);
        poll = null;
      }
    },
    { immediate: true },
  );
}

/** App-wide notices: pending collation restores and keychain cleanup. */
export function useNotices() {
  return { state, start, refreshRestores, refreshKeychain, retryKeychain, restore, dismiss };
}
