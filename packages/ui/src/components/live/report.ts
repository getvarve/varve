import { toCmdError } from '../../api/errors';
import type { CmdError } from '../../api/errors';
import { useTabs } from '../../composables/useTabs';

/**
 * Parses a failed call for display. `sessionId` is the session the call was
 * sent on, read before the await: the answer of a session the tab has
 * replaced since must not mark the new one. `null` is for a failure that is
 * not a backend call on the session (the clipboard, writing a file): it is
 * only parsed.
 */
export type Reporter = (error: unknown, sessionId: string | null) => CmdError;

/**
 * Parses a failed call and, when the panel runs inside a workspace tab,
 * reports it to the tab (`reportTabError`: marks a lost session and a
 * needed reconnect, so the tab shows its banner). Panels used outside a tab
 * pass no id and only get the parsed error.
 */
export function useReporter(tabId: () => string | null | undefined): Reporter {
  const tabs = useTabs();
  return (error: unknown, sessionId: string | null): CmdError => {
    const id = tabId();
    return id && sessionId !== null ? tabs.reportTabError(id, error, sessionId) : toCmdError(error);
  };
}
