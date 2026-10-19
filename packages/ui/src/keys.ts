import type { InjectionKey, Ref } from 'vue';
import type { AppInfo } from './backend';
import type { ExtensionRegistry } from './extensions';

export const REGISTRY_KEY: InjectionKey<ExtensionRegistry> = Symbol('varve.registry');
export const APP_INFO_KEY: InjectionKey<Ref<AppInfo | null>> = Symbol('varve.appInfo');
