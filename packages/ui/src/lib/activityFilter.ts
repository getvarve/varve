/** An action name (`query.failed`) or a prefix pattern (`connection.*`). */
export const ACTION_PATTERN = /^[a-z0-9_]+(\.[a-z0-9_]+)*(\.\*)?$/;

/** The entries typed in the action filter, comma or space separated, without empties. */
export function actionTokens(text: string): string[] {
  return text
    .split(/[\s,]+/)
    .map((a) => a.trim())
    .filter((a) => a !== '');
}

/**
 * Actions typed in the activity filter: comma or space separated, trimmed,
 * without repeats. An entry ending in `.*` matches a prefix (the backend
 * handles that, design doc 9.6).
 */
export function parseActions(text: string): string[] {
  return [...new Set(actionTokens(text).filter((a) => ACTION_PATTERN.test(a)))];
}
