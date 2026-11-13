import { call } from './invoke';

/** `Category` in crates/activity/src/event.rs, in the order of design doc 9.2. */
export const CATEGORIES = [
  'connection',
  'query',
  'transaction',
  'data',
  'schema',
  'tool',
  'security',
  'safety',
  'secrets',
  'stats',
  'app',
  'account',
  'sync',
  'workspace',
  'membership',
  'access',
  'record',
] as const;

export type Category = (typeof CATEGORIES)[number];

export const OUTCOMES = ['success', 'failure', 'cancelled'] as const;
export type Outcome = (typeof OUTCOMES)[number];

/** `ActivityEvent` in crates/activity/src/event.rs. */
export interface ActivityEvent {
  id: string;
  occurredAt: number;
  actorId: string;
  deviceId: string | null;
  workspaceId: string | null;
  connectionId: string | null;
  serverKey: string | null;
  category: Category;
  action: string;
  outcome: Outcome;
  durationMs: number | null;
  target: string | null;
  details: unknown;
}

/** `SortColumn` in crates/activity/src/query.rs: the columns the log sorts on, over every page. */
export const ACTIVITY_SORT_COLUMNS = ['time', 'action', 'outcome', 'duration', 'connection'] as const;
export type ActivitySortColumn = (typeof ACTIVITY_SORT_COLUMNS)[number];

export type SortDirection = 'ascending' | 'descending';

/**
 * `ActivityOrder` in crates/activity/src/query.rs. Values that compare equal
 * (and every order but time) list newest first; events without a duration
 * or connection come last in both directions, a deleted connection's just
 * before them. Connection names compare without regard to case, digits by
 * value.
 */
export interface ActivityOrder {
  column: ActivitySortColumn;
  direction: SortDirection;
}

/** `ActivityFilter` in crates/activity/src/query.rs. Every field is optional. */
export interface ActivityFilter {
  from?: number;
  to?: number;
  categories?: Category[];
  actions?: string[];
  outcomes?: Outcome[];
  connectionId?: string;
  workspaceId?: string;
  actorId?: string;
  deviceId?: string;
  text?: string;
  /** Newest first when left out. Exports ignore it. */
  order?: ActivityOrder;
  /** The `nextCursor` of the previous page, read with the same filter and order; a cursor of another order is refused. */
  beforeId?: string;
  limit?: number;
}

/** `Page` in crates/activity/src/query.rs. */
export interface ActivityPage {
  events: ActivityEvent[];
  nextCursor: string | null;
}

export type ActivityExportFormat = 'csv' | 'jsonl';

export function fetchActivity(filter: ActivityFilter): Promise<ActivityPage> {
  return call<ActivityPage>('activity', { filter });
}

export function fetchQueryHistory(connectionId: string, limit: number): Promise<ActivityPage> {
  return call<ActivityPage>('query_history', { connectionId, limit });
}

/** A result export the UI wrote to a file (`record_export`, recorded as `data.exported`). */
export interface ExportRecord {
  /** The connection the exported result came from. */
  connectionId: string | null;
  /** What was exported, for example the query tab name. */
  objects: string[];
  /** The file name; the backend keeps only the last part of a path. */
  fileName: string;
  format: 'csv' | 'json' | 'jsonl';
  sizeBytes: number | null;
  rows: number | null;
}

export async function recordExport(record: ExportRecord): Promise<void> {
  await call('record_export', { record });
}

/**
 * Records an export that already succeeded. A failure to record never turns
 * the export into a failure: it is only logged to the console.
 */
export async function noteExport(record: ExportRecord): Promise<void> {
  try {
    await recordExport(record);
  } catch (error: unknown) {
    console.warn('could not record the export in the activity log', error);
  }
}

/** UTF-8 size of exported text, for `sizeBytes`. */
export function utf8Size(text: string): number {
  return new TextEncoder().encode(text).length;
}

export function clearActivity(connectionId: string | null): Promise<number> {
  return call<number>('clear_activity', { connectionId });
}

/** How much statement text the log keeps (design doc 9.3). */
export type TextLevel = 'none' | 'normalized' | 'full';

/** `ActivitySettings` in app/src-tauri/src/settings.rs. `retentionDays: null` keeps events forever. */
export interface ActivitySettings {
  textLevel: TextLevel;
  retentionDays: number | null;
}

export function fetchActivitySettings(): Promise<ActivitySettings> {
  return call<ActivitySettings>('activity_settings');
}

/** Stores the settings, applies them at once and resolves how many old events were removed. */
export function saveActivitySettings(settings: ActivitySettings): Promise<number> {
  return call<number>('set_activity_settings', { settings });
}

export function exportActivity(filter: ActivityFilter, path: string, format: ActivityExportFormat): Promise<number> {
  return call<number>('export_activity', { filter, path, format });
}

/** The redacted statement text stored in a query event, if any. */
export function statementOf(event: ActivityEvent): string | null {
  const details = event.details;
  if (typeof details === 'object' && details !== null && !Array.isArray(details)) {
    const statement = (details as Record<string, unknown>).statement;
    if (typeof statement === 'string') return statement;
  }
  return null;
}
