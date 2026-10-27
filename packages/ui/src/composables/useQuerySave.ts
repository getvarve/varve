import { reactive } from 'vue';
import { inputFromQuery, updateQuery } from '../api/records';
import { useDialogs } from './useDialogs';
import { useTabs } from './useTabs';
import type { QueryTab } from './useTabs';
import { useToasts } from './useToasts';
import { useWorkspace } from './useWorkspace';

/** Saves running per tab: a second Save or Save as while one runs joins it instead of starting another. */
const inflight = new Map<string, Promise<boolean>>();
/** Tab ids with a save running, for disabling buttons. */
const saving = reactive(new Set<string>());

function queryTab(id: string): QueryTab | null {
  const tab = useTabs().tabs.find((t) => t.id === id);
  return tab?.kind === 'query' ? tab : null;
}

/** True when a query tab has text that is not stored in a saved query. */
export function hasUnsavedEdits(tab: QueryTab): boolean {
  if (tab.savedQuery) return tab.savedQuery.sql !== tab.sql;
  return tab.sql.trim() !== '';
}

async function saveAs(id: string): Promise<boolean> {
  const tab = queryTab(id);
  if (!tab) return false;
  const profile = useWorkspace().connection(tab.connectionId);
  if (!profile) {
    useToasts().push(
      'warning',
      'Cannot save this query',
      'Its connection was deleted. Copy the text into a tab of another connection to save it there.',
    );
    return false;
  }
  const saved = await useDialogs().saveQuery({
    request: {
      connection: profile,
      sql: tab.sql,
      database: profile.database,
      name: tab.savedQuery ? `${tab.savedQuery.name} (copy)` : '',
      folderId: tab.savedQuery?.folderId ?? null,
      tags: tab.savedQuery?.tags ?? [],
    },
  });
  if (!saved) return false;
  const current = queryTab(id);
  if (current) {
    current.savedQuery = saved;
    current.title = saved.name;
  }
  useToasts().push('success', `Saved “${saved.name}”`, '', 2500);
  await useWorkspace().refresh();
  return true;
}

async function save(id: string): Promise<boolean> {
  const tab = queryTab(id);
  if (!tab) return false;
  const opened = tab.savedQuery;
  if (!opened) return saveAs(id);
  // Re-read the saved query from the tree first, so a rename or move made
  // since the tab opened is kept: only the SQL text changes here.
  const workspace = useWorkspace();
  await workspace.refresh();
  const current = workspace.query(opened.id);
  if (!current) {
    useToasts().push('warning', 'The saved query was deleted', 'Save it again under a name.');
    return saveAs(id);
  }
  try {
    const saved = await updateQuery(current.id, { ...inputFromQuery(current), sql: tab.sql });
    const latest = queryTab(id);
    if (latest) latest.savedQuery = saved;
    useToasts().push('success', `Saved “${saved.name}”`, '', 2500);
    await workspace.refresh();
    return true;
  } catch (error: unknown) {
    useToasts().pushError(error, 'Could not save the query');
    return false;
  }
}

/**
 * Saves a query tab ("save" updates its saved query or asks for a name;
 * "saveAs" always asks). One save per tab at a time: calls while one runs
 * get the running save's result. Resolves `true` when the text was saved.
 */
function saveQueryTab(id: string, mode: 'save' | 'saveAs'): Promise<boolean> {
  const running = inflight.get(id);
  if (running) return running;
  saving.add(id);
  const work = (mode === 'save' ? save(id) : saveAs(id)).finally(() => {
    inflight.delete(id);
    saving.delete(id);
  });
  inflight.set(id, work);
  return work;
}

/**
 * Asks about every listed tab that has unsaved edits: Save (which must
 * succeed), Discard, or Cancel. Resolves `true` when all of them may close,
 * `false` as soon as the user cancels or a save fails. Used before closing a
 * tab, deleting a connection or folder, and quitting.
 */
async function confirmUnsavedTabs(ids: readonly string[]): Promise<boolean> {
  for (const id of ids) {
    const tab = queryTab(id);
    if (!tab || !hasUnsavedEdits(tab)) continue;
    useTabs().activate(id);
    const choice = await useDialogs().askUnsavedClose({ title: tab.title, saved: tab.savedQuery !== null });
    if (choice === null) return false;
    if (choice === 'save' && !(await saveQueryTab(id, 'save'))) return false;
  }
  return confirmUnsavedEditors(ids);
}

/**
 * Asks once about every listed editor tab (object, designer, DDL, key
 * editor, ...) that holds changes it has not applied: Discard or Cancel.
 * Applying is the editor's own step (a plan to preview), so there is no
 * Save here.
 */
async function confirmUnsavedEditors(ids: readonly string[]): Promise<boolean> {
  const wanted = new Set(ids);
  const editors = useTabs().tabs.filter((t) => wanted.has(t.id) && t.kind !== 'query' && t.unsaved);
  const [first] = editors;
  if (!first) return true;
  useTabs().activate(first.id);
  const one = editors.length === 1;
  return useDialogs().askConfirm({
    title: one ? `Discard changes in “${first.title}”?` : `Discard changes in ${editors.length} tabs?`,
    message: one
      ? 'This tab has changes that are not applied. They are lost if you close it.'
      : `These tabs have changes that are not applied: ${editors.map((t) => `“${t.title}”`).join(', ')}. They are lost if you close them.`,
    confirmLabel: 'Discard',
    danger: true,
  });
}

/** Closes running per tab, so a second close request while the question is open does nothing. */
const closing = new Map<string, Promise<void>>();

/** Closes a tab, asking first when it has unsaved edits. One close per tab at a time. */
function closeWithCheck(id: string): Promise<void> {
  const running = closing.get(id);
  if (running) return running;
  const work = (async () => {
    if (await confirmUnsavedTabs([id])) await useTabs().closeTab(id);
  })().finally(() => closing.delete(id));
  closing.set(id, work);
  return work;
}

export function useQuerySave() {
  return { saving, saveQueryTab, closeWithCheck, confirmUnsavedTabs, hasUnsavedEdits };
}
