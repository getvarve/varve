/**
 * What a table designer tab was opened for, beyond its object: a new
 * partitioned table ("New partitioned table..." on the Partitioned tables
 * group), or an index, constraint or partition to show (their tree nodes
 * open the owning table's designer on that item).
 *
 * Kept per tab id in a module store next to the designer, because the tab
 * model (composables/useTabs.ts) carries only the object and container. A
 * tab that is already open gets the new focus too: the designer watches it.
 */
import { reactive } from 'vue';

/** A designer section. */
export type DesignerSection = 'columns' | 'indexes' | 'constraints' | 'partitions' | 'properties';

/** The item the designer shows first. */
export interface DesignerFocus {
  section: DesignerSection;
  /** Name of the index, constraint or partition as the catalog lists it; `null` for the section only. */
  name: string | null;
  /** Bumped on every request, so asking for the same item again still moves there. */
  seq: number;
}

/** What a designer tab was opened for. */
export interface DesignerIntent {
  /** A new table starts as a PostgreSQL partitioned table. */
  partitioned: boolean;
  focus: DesignerFocus | null;
}

const intents = reactive(new Map<string, DesignerIntent>());
/** Per tab: the `seq` of the last focus the designer showed. */
const shown = new Map<string, number>();
let seq = 0;

/** Records that the designer tab `tabId` creates a partitioned table. */
export function requestPartitioned(tabId: string): void {
  const now = intents.get(tabId);
  intents.set(tabId, { partitioned: true, focus: now?.focus ?? null });
}

/** Asks the designer tab `tabId` to show `section` (and the item `name` in it). */
export function requestFocus(tabId: string, section: DesignerSection, name: string | null): void {
  seq += 1;
  const now = intents.get(tabId);
  intents.set(tabId, { partitioned: now?.partitioned ?? false, focus: { section, name, seq } });
}

/** What the designer tab `tabId` was opened for (reactive). */
export function designerIntent(tabId: string): DesignerIntent {
  return intents.get(tabId) ?? { partitioned: false, focus: null };
}

/** The focus request of the tab that its designer has not shown yet, or `null`. */
export function pendingFocus(tabId: string): DesignerFocus | null {
  const focus = intents.get(tabId)?.focus ?? null;
  return focus && focus.seq > (shown.get(tabId) ?? 0) ? focus : null;
}

/** The designer showed `focus`; opening the tab again (its view is mounted anew) does not jump back to it. */
export function focusShown(tabId: string, focus: DesignerFocus): void {
  shown.set(tabId, Math.max(shown.get(tabId) ?? 0, focus.seq));
}

/** Forgets the intents of tabs that are closed (every id not in `openIds`). */
export function pruneIntents(openIds: readonly string[]): void {
  const open = new Set(openIds);
  for (const id of [...intents.keys()]) if (!open.has(id)) intents.delete(id);
  for (const id of [...shown.keys()]) if (!open.has(id)) shown.delete(id);
}
