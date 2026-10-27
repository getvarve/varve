import { toCmdError } from '../../api/errors';
import type { CmdError } from '../../api/errors';
import { useTabs } from '../../composables/useTabs';

/**
 * Parses a failed call and, when the panel runs inside a workspace tab,
 * reports it to the tab (`reportTabError`: marks a lost session and a
 * needed reconnect, so the tab shows its banner). Panels used outside a tab
 * pass no id and only get the parsed error.
 */
export function useReporter(tabId: () => string | null | undefined): (error: unknown) => CmdError {
  const tabs = useTabs();
  return (error: unknown): CmdError => {
    const id = tabId();
    return id ? tabs.reportTabError(id, error) : toCmdError(error);
  };
}
