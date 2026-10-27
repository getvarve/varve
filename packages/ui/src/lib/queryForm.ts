import { z } from 'zod';
import type { SavedQueryInput } from '../api/records';

const MAX_SQL_BYTES = 1_048_576;

/** The save-query form. Mirrors `validate_query` in crates/records/src/repo.rs. */
export const savedQueryFormSchema = z
  .object({
    name: z.string(),
    folderId: z.string(),
    tags: z.string(),
    sql: z.string(),
  })
  .superRefine((v, ctx) => {
    const name = v.name.trim();
    if (name === '') ctx.addIssue({ code: z.ZodIssueCode.custom, path: ['name'], message: 'A saved query needs a name.' });
    else if ([...name].length > 200) {
      ctx.addIssue({ code: z.ZodIssueCode.custom, path: ['name'], message: 'Saved query names can be at most 200 characters.' });
    }
    if (v.sql.trim() === '') ctx.addIssue({ code: z.ZodIssueCode.custom, path: ['sql'], message: 'A saved query needs some SQL.' });
    else if (new TextEncoder().encode(v.sql).length > MAX_SQL_BYTES) {
      ctx.addIssue({ code: z.ZodIssueCode.custom, path: ['sql'], message: 'Saved queries can be at most 1 MB.' });
    }
  });

export type SavedQueryFormValues = z.input<typeof savedQueryFormSchema>;

/** Comma separated tags, trimmed, without empties or repeats, sorted (as the server stores them). */
export function parseTags(text: string): string[] {
  return [...new Set(text.split(',').map((t) => t.trim()).filter((t) => t !== ''))].sort();
}

export function toSavedQueryInput(
  v: SavedQueryFormValues,
  connectionId: string | null,
  database: string | null,
): SavedQueryInput {
  return {
    folderId: v.folderId === '' ? null : v.folderId,
    connectionId,
    database,
    name: v.name.trim(),
    sql: v.sql,
    tags: parseTags(v.tags),
  };
}
