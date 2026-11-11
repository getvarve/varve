/**
 * Server-held plans and edit plans, shared by every API module
 * (app/src-tauri/src/m2/plans.rs, crates/catalog/src/model.rs).
 */

/** `Planned<T>` in app/src-tauri/src/m2/plans.rs: a plan kept on the server; apply it by `planId`. */
export interface Planned<T> {
  planId: string;
  plan: T;
}

/** `EditPlan` in crates/catalog/src/model.rs: SQL an editor produced, shown before it runs. */
export interface EditPlan {
  /** The statements as they run, one by one. Applying, and anything that counts executed steps, goes by these. */
  statements: string[];
  /**
   * The statements as the user should read them: the same change, with the
   * groups of statements MySQL and MariaDB need at run time collapsed to one
   * readable `ALTER` or `CREATE`. Absent on a plan made in the UI (a DDL
   * step) and from a backend that does not send it; read it through
   * `readableStatements`.
   */
  shown?: string[];
  /** PostgreSQL only: the statements run in one `BEGIN ... COMMIT`. */
  transactional: boolean;
  destructive: boolean;
  warnings: string[];
}

/** The statements of a plan to display (preview, review, confirmation, copy): `shown`, or `statements` without it. Never what runs. */
export function readableStatements(plan: Pick<EditPlan, 'statements' | 'shown'>): string[] {
  return plan.shown ?? plan.statements;
}
