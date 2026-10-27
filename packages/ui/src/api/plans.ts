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
  statements: string[];
  /** PostgreSQL only: the statements run in one `BEGIN ... COMMIT`. */
  transactional: boolean;
  destructive: boolean;
  warnings: string[];
}
