import type { Router } from 'vue-router';
import type { ObjectActionId } from './objectActions';

/**
 * Shows the workspace page (where the tabs are) when another page, such as
 * Activity or Settings, is open. The sidebar is visible on every page, so an
 * action there that opens or activates a tab calls this; otherwise the tab
 * opens out of sight.
 */
export function showWorkspace(router: Router): void {
  if (router.currentRoute.value.name !== 'home') void router.push({ name: 'home' });
}

/** Object tree actions that do not open or activate a tab. */
const NO_TAB_ACTIONS: readonly ObjectActionId[] = ['refresh', 'copy-name', 'copy-qualified'];

/** True when the object tree action opens or activates a tab. */
export function actionOpensTab(id: ObjectActionId): boolean {
  return !NO_TAB_ACTIONS.includes(id);
}
