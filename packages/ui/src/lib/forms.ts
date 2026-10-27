import { z } from 'zod';
import type { Environment, SecretKindName } from '../api/records';
import { secretLabel } from '../api/errors';
import { ACTION_PATTERN, actionTokens } from './activityFilter';
import { canConfirm, needsTypedName } from './destructive';

const custom = (ctx: z.RefinementCtx, path: string, message: string): void => {
  ctx.addIssue({ code: z.ZodIssueCode.custom, path: [path], message });
};

/** The password or passphrase prompt. */
export function secretFormSchema(secret: SecretKindName) {
  return z
    .object({ value: z.string(), save: z.boolean() })
    .superRefine((v, ctx) => {
      if (v.value === '') custom(ctx, 'value', `Enter the ${secretLabel(secret)}.`);
    });
}

/** The destructive statement confirmation: on production the connection name must be typed exactly. */
export function destructiveFormSchema(environment: Environment, connectionName: string) {
  return z.object({ typed: z.string() }).superRefine((v, ctx) => {
    if (!needsTypedName(environment)) return;
    if (v.typed.trim() === '') custom(ctx, 'typed', 'Type the connection name to confirm.');
    else if (!canConfirm(environment, connectionName, v.typed)) custom(ctx, 'typed', 'The name does not match this connection.');
  });
}

export const SCAN_COUNTS = [500, 1000, 5000] as const;

/** The key browser's SCAN form. */
export const scanFormSchema = z
  .object({
    pattern: z.string(),
    /** '' for every type, a core type, `json`, or a module `TYPE` name the server offers (api/redis.ts `moduleTypeFilters`). */
    typeFilter: z.string(),
    delimiter: z.string(),
    count: z.number(),
  })
  .superRefine((v, ctx) => {
    if (v.pattern.trim() === '') custom(ctx, 'pattern', 'Enter a MATCH pattern, for example * or session:*.');
    if (v.delimiter.length > 8) custom(ctx, 'delimiter', 'Use a delimiter of at most 8 characters.');
    if (!SCAN_COUNTS.includes(v.count as (typeof SCAN_COUNTS)[number])) custom(ctx, 'count', 'Choose 500, 1000 or 5000.');
  });

export type ScanFormValues = z.input<typeof scanFormSchema>;

/** Renaming a folder, connection or saved query (the server allows 1 to 200 characters). */
export const renameFormSchema = z.object({ name: z.string() }).superRefine((v, ctx) => {
  const name = v.name.trim();
  if (name === '') custom(ctx, 'name', 'Enter a name.');
  else if ([...name].length > 200) custom(ctx, 'name', 'Names can be at most 200 characters.');
});

/** The "Move to" dialog: `''` is the top level. */
export function moveFormSchema(allowed: readonly string[]) {
  return z.object({ folderId: z.string() }).superRefine((v, ctx) => {
    if (v.folderId !== '' && !allowed.includes(v.folderId)) {
      custom(ctx, 'folderId', 'A folder cannot be moved into itself or one of its subfolders.');
    }
  });
}

/** The activity panel's filter fields. Invalid action entries stop the query and show a message. */
export const activityFilterSchema = z
  .object({
    text: z.string(),
    outcome: z.enum(['', 'success', 'failure', 'cancelled']),
    connectionId: z.string(),
    actions: z.string(),
  })
  .superRefine((v, ctx) => {
    const invalid = actionTokens(v.actions).filter((a) => !ACTION_PATTERN.test(a));
    if (invalid.length > 0) {
      custom(
        ctx,
        'actions',
        `Not ${invalid.length === 1 ? 'an action' : 'actions'}: ${invalid.join(', ')}. Use names such as query.failed, or connection.* for a prefix.`,
      );
    }
    if (v.text.length > 500) custom(ctx, 'text', 'Search for at most 500 characters.');
  });

export type ActivityFilterValues = z.input<typeof activityFilterSchema>;

/** Longest retention the backend accepts (`MAX_RETENTION_DAYS` in settings.rs, 100 years). */
export const MAX_RETENTION_DAYS = 36_500;
const UNIT_DAYS = { days: 1, months: 30, years: 365 } as const;
export type RetentionUnit = keyof typeof UNIT_DAYS;

/** The stored activity settings (design doc 9.3 and 9.5): text level and how long events are kept. */
export const activitySettingsSchema = z
  .object({
    textLevel: z.enum(['none', 'normalized', 'full']),
    keep: z.enum(['forever', 'period']),
    amount: z.string(),
    unit: z.enum(['days', 'months', 'years']),
  })
  .superRefine((v, ctx) => {
    if (v.keep !== 'period') return;
    const amount = v.amount.trim();
    if (amount === '') {
      custom(ctx, 'amount', 'Enter how long to keep events.');
      return;
    }
    if (!/^\d+$/.test(amount) || Number(amount) < 1) {
      custom(ctx, 'amount', 'Enter a whole number of at least 1.');
      return;
    }
    if (Number(amount) * UNIT_DAYS[v.unit] > MAX_RETENTION_DAYS) {
      custom(ctx, 'amount', 'Keep events for at most 100 years, or choose Forever.');
    }
  });

export type ActivitySettingsValues = z.input<typeof activitySettingsSchema>;

/** Form values for stored settings: a period is shown in the largest whole unit. */
export function activitySettingsForm(settings: { textLevel: 'none' | 'normalized' | 'full'; retentionDays: number | null }): ActivitySettingsValues {
  const days = settings.retentionDays;
  if (days === null) return { textLevel: settings.textLevel, keep: 'forever', amount: '90', unit: 'days' };
  const unit: RetentionUnit = days % 365 === 0 ? 'years' : days % 30 === 0 ? 'months' : 'days';
  return { textLevel: settings.textLevel, keep: 'period', amount: String(days / UNIT_DAYS[unit]), unit };
}

/** The settings to store for valid form values. */
export function activitySettingsFrom(v: ActivitySettingsValues): { textLevel: 'none' | 'normalized' | 'full'; retentionDays: number | null } {
  return {
    textLevel: v.textLevel,
    retentionDays: v.keep === 'forever' ? null : Number(v.amount.trim()) * UNIT_DAYS[v.unit],
  };
}

/** Deleting a folder: what happens to its contents. Deleting contents needs something to delete. */
export function deleteFolderFormSchema(itemCount: number) {
  return z.object({ mode: z.enum(['move_contents_to_parent', 'delete_contents']) }).superRefine((v, ctx) => {
    if (v.mode === 'delete_contents' && itemCount === 0) custom(ctx, 'mode', 'The folder is empty; there are no contents to delete.');
  });
}

/** Trusting a changed host key needs the explicit acknowledgement. */
export function hostKeyFormSchema(changed: boolean) {
  return z.object({ acknowledged: z.boolean() }).superRefine((v, ctx) => {
    if (changed && !v.acknowledged) custom(ctx, 'acknowledged', 'Confirm that you checked the new fingerprint.');
  });
}

/** The filter of the connection picker. */
export const pickerFormSchema = z.object({ filter: z.string() }).superRefine((v, ctx) => {
  if (v.filter.length > 200) custom(ctx, 'filter', 'Filter on at most 200 characters.');
});

/** True when a new retention period removes events the old one kept (so the user confirms first). */
export function retentionShortened(before: number | null, after: number | null): boolean {
  if (after === null) return false;
  return before === null || after < before;
}
