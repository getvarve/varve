import { createApp, ref } from 'vue';
import type { App } from 'vue';
import { createRouter, createWebHashHistory } from 'vue-router';
import type { AppInfo } from './backend';
import { extInvoke, fetchAppInfo } from './backend';
import AppShell from './components/AppShell.vue';
import { ExtensionRegistry } from './extensions';
import type { UiExtension } from './extensions';
import { APP_INFO_KEY, REGISTRY_KEY } from './keys';
import AboutProView from './views/AboutProView.vue';
import HomeView from './views/HomeView.vue';

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
      { path: '/', name: 'home', component: HomeView },
      { path: '/about-pro', name: 'about-pro', component: AboutProView },
      ...registry.extensionRoutes(),
    ],
  });

  const info = ref<AppInfo | null>(null);
  fetchAppInfo()
    .then((value) => {
      info.value = value;
    })
    .catch((error: unknown) => {
      console.error('Could not read app info from the backend', error);
    });

  const app = createApp(AppShell);
  app.provide(REGISTRY_KEY, registry);
  app.provide(APP_INFO_KEY, info);
  app.use(router);
  return app;
}
