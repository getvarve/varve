import { createApp, ref } from 'vue';
import type { App } from 'vue';
import { createRouter, createWebHashHistory } from 'vue-router';
import type { AppInfo } from './backend';
import { extInvoke, fetchAppInfo } from './backend';
import AppShell from './components/AppShell.vue';
import { openTransactions } from './api/sessions';
import { useDialogs } from './composables/useDialogs';
import { useWorkspace } from './composables/useWorkspace';
import { ExtensionRegistry } from './extensions';
import type { UiExtension } from './extensions';
import { APP_INFO_KEY, REGISTRY_KEY } from './keys';
import { watchGlass } from './lib/glass';
import { installWebviewGuards } from './lib/webviewGuards';
import './styles/base.css';
import './styles/glass.css';
import AboutProView from './views/AboutProView.vue';
import ActivityView from './views/ActivityView.vue';
import SettingsView from './views/SettingsView.vue';
import WorkspaceView from './views/WorkspaceView.vue';

export interface VarveAppOptions {
  /** UI extensions to install. Varve Community passes none. */
  extensions?: readonly UiExtension[];
}

/**
 * Creates the Varve app. Each edition's frontend entry calls this with its
 * extensions and mounts the result.
 */
export function createVarveApp(options: VarveAppOptions = {}): App {
  const registry = new ExtensionRegistry(extInvoke);
  registry.install(options.extensions ?? []);

  const router = createRouter({
    history: createWebHashHistory(),
    routes: [
      { path: '/', name: 'home', component: WorkspaceView },
      { path: '/activity', name: 'activity', component: ActivityView },
      { path: '/settings', name: 'settings', component: SettingsView },
      { path: '/about-pro', name: 'about-pro', component: AboutProView },
      ...registry.extensionRoutes(),
    ],
  });

  const info = ref<AppInfo | null>(null);
  fetchAppInfo()
    .then((value) => {
      info.value = value;
      watchGlass(value.glass);
    })
    .catch((error: unknown) => {
      console.error('Could not read app info from the backend', error);
    });

  // A reload closes every session of the page: not one click or key away.
  installWebviewGuards({
    debugBuild: () => info.value?.debugBuild ?? null,
    openTransactions: async () => {
      const ids = await openTransactions();
      return ids.map((id) => useWorkspace().connection(id)?.name ?? id);
    },
    confirm: (names) =>
      useDialogs().askConfirm({
        title: 'Reload the window?',
        message: `A reload closes every tab's session. Open transactions are rolled back: ${[...new Set(names)].join(', ')}.`,
        confirmLabel: 'Reload',
        danger: true,
      }),
    reload: () => window.location.reload(),
  });

  const app = createApp(AppShell);
  app.provide(REGISTRY_KEY, registry);
  app.provide(APP_INFO_KEY, info);
  app.use(router);
  return app;
}
