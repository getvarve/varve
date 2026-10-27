/**
 * State the table designer shares with its sections (columns, indexes,
 * constraints, partitions) through provide / inject, so the sections edit
 * the one working copy without passing it up and down as props.
 */
import { inject } from 'vue';
import type { InjectionKey } from 'vue';
import type { DesignerDetails, EngineChange } from '../../api/grid';
import type { ServerInfo } from '../../api/sessions';
import type { TableDraft } from '../../lib/gridDesigner';

/** What every designer section can read and do. */
export interface DesignerContext {
  /** The working copy; sections change it in place. */
  draft: TableDraft;
  /** The table as read, to show what changed. */
  original: TableDraft;
  /** The catalog details the draft was read from; `null` for a new table. */
  details: DesignerDetails | null;
  /** True when the session is read-only: everything shows, nothing can be saved. */
  readOnly: boolean;
  /** Server major version, for version-dependent options. */
  serverMajor: number;
  /** MySQL 8.0.23+ / MariaDB 10.3.3+: columns can be INVISIBLE (the Columns section shows the control). */
  invisibleColumns: boolean;
  /**
   * Plans and applies changes right away (partition actions, validate,
   * reindex): shows the review dialog and reloads the table afterwards.
   */
  runNow: (title: string, changes: EngineChange[]) => void;
  /** The item a tree node asked to show (sections select it when they open); `null` for none. */
  focus: { section: string; name: string | null } | null;
  /** The tab's user session, for lookups a section makes (the server's storage engines). */
  sessionId: string;
}

/** Injection key of the designer context. */
export const DESIGNER: InjectionKey<DesignerContext> = Symbol('table-designer');

/** The designer context; throws outside the designer. */
export function useDesigner(): DesignerContext {
  const ctx = inject(DESIGNER, null);
  if (!ctx) throw new Error('Designer sections must be used inside TableDesigner.');
  return ctx;
}

/** INVISIBLE columns: MySQL 8.0.23 and later, MariaDB 10.3.3 and later (the catalog's `invisibleColumns` feature). */
export function invisibleColumnsOn(server: ServerInfo): boolean {
  const at = (major: number, minor: number, patch: number): boolean =>
    server.major !== major ? server.major > major : server.minor !== minor ? server.minor > minor : (server.patch ?? 0) >= patch;
  if (server.engine === 'mysql') return at(8, 0, 23);
  if (server.engine === 'mariadb') return at(10, 3, 3);
  return false;
}
