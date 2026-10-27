import type { ZodTypeAny } from 'zod';

function setPath(target: unknown, path: readonly string[], value: unknown): unknown {
  if (path.length === 0) return value;
  const [head, ...rest] = path;
  const base = typeof target === 'object' && target !== null ? (target as Record<string, unknown>) : {};
  return { ...base, [head as string]: setPath(base[head as string], rest, value) };
}

function getPath(target: unknown, path: readonly string[]): unknown {
  let cursor = target;
  for (const part of path) {
    if (typeof cursor !== 'object' || cursor === null) return undefined;
    cursor = (cursor as Record<string, unknown>)[part];
  }
  return cursor;
}

/**
 * Whether a field is required, read from the schema itself: the field is
 * cleared in the current values and the schema is asked whether that is
 * valid. This follows every rule of the schema, including conditional ones
 * such as "the SSH host is required when the tunnel is on", so the required
 * markers never drift from the validation. A checkbox counts as required
 * only when the schema rejects it unticked.
 */
export function isRequired(schema: ZodTypeAny, values: unknown, path: string): boolean {
  const parts = path.split('.');
  const current = getPath(values, parts);
  // The "empty" value of a field: '' for text, false for a checkbox. Other
  // kinds (numbers, choices) always hold a value, so they are never marked.
  let empty: string | boolean;
  if (typeof current === 'string') empty = '';
  else if (typeof current === 'boolean') empty = false;
  else return false;
  const result = schema.safeParse(setPath(values, parts, empty));
  if (result.success) return false;
  return result.error.issues.some((issue) => issue.path.join('.') === path);
}

/** The required fields among `paths` for the current values. */
export function requiredFields(schema: ZodTypeAny, values: unknown, paths: readonly string[]): Set<string> {
  return new Set(paths.filter((p) => isRequired(schema, values, p)));
}
