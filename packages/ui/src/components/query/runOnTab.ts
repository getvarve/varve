import type { QueryTab } from '../../composables/useTabs';

/** The part of a query tab that decides what Run does. */
export type RunnableTab = Pick<QueryTab, 'status' | 'session' | 'running' | 'sql'>;

/** The statement Run sends: the selection, else the whole editor, and where it starts in the editor. */
export interface RunTarget {
  text: string;
  offset: number;
}

/**
 * What Run does on a query tab now:
 * - `run`: the tab is connected; the statement runs.
 * - `connect`: the tab has no session (its connection was closed, a connect
 *   failed or was cancelled); it connects again first, as using a tab does.
 * - `wait`: a statement is running, or the tab is connecting (from an
 *   earlier Run or Connect again). Nothing starts: that connect already
 *   leads to the tab being used, and a second request would run twice.
 */
export function runStep(tab: Pick<RunnableTab, 'status' | 'session' | 'running'>): 'run' | 'connect' | 'wait' {
  if (tab.running || tab.status === 'connecting') return 'wait';
  if (tab.status === 'ready' && tab.session) return 'run';
  return 'connect';
}

/**
 * Whether Run can be used now (the button, the editor shortcut and the menu
 * bar item). A tab whose connection no longer exists cannot connect, so it
 * cannot run either.
 */
export function canRun(tab: Pick<RunnableTab, 'status' | 'session' | 'running'>, hasConnection: boolean): boolean {
  return hasConnection && runStep(tab) !== 'wait';
}

/** What `runOnTab` uses of `useTabs()`, so the rule is tested without sessions. */
export interface RunDeps {
  find(id: string): RunnableTab | undefined;
  reconnect(id: string): Promise<void>;
  runQuery(id: string, text: string, offset: number): Promise<void>;
}

/**
 * Tabs with a Run waiting for its connect. `connect` marks the tab
 * `connecting` before its first await, so a second Run already sees `wait`;
 * this keeps that true even if a connect ever answers without passing
 * through `connecting` (a missing connection fails at once).
 */
const waiting = new Set<string>();

/**
 * Runs `target` on a query tab, connecting the tab again first when it is
 * not connected. The statement runs once, and only when the connect left the
 * tab ready: when it failed, was cancelled (a dismissed password prompt) or
 * the connection was closed again meanwhile, nothing runs and the tab shows
 * its not-connected state. `retarget` reads the statement again from the
 * editor when its text changed while connecting, so the run and its error
 * position match the editor as it is when the statement is sent.
 */
export async function runOnTab(deps: RunDeps, id: string, target: RunTarget, retarget: () => RunTarget | null): Promise<void> {
  if (target.text.trim() === '' || waiting.has(id)) return;
  const tab = deps.find(id);
  if (!tab) return;
  const step = runStep(tab);
  if (step === 'wait') return;
  if (step === 'run') {
    await deps.runQuery(id, target.text, target.offset);
    return;
  }
  const doc = tab.sql;
  waiting.add(id);
  try {
    await deps.reconnect(id);
  } finally {
    waiting.delete(id);
  }
  const after = deps.find(id);
  if (!after || runStep(after) !== 'run') return;
  const now = after.sql === doc ? target : retarget();
  if (!now || now.text.trim() === '') return;
  await deps.runQuery(id, now.text, now.offset);
}
