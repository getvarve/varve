import { shallowReactive } from 'vue';
import type { Component } from 'vue';
import type { RouteRecordRaw } from 'vue-router';

/**
 * Named places in the app where extensions can add UI (design doc 2.3).
 * Extensions can only add to these slots; they cannot replace app UI.
 */
export const SLOT_NAMES = [
  'title-bar',
  'main-menu',
  'tools',
  'sidebar-sections',
  'connection-tabs',
  'settings-sections',
] as const;

export type SlotName = (typeof SLOT_NAMES)[number];

/** One thing an extension adds to a slot. */
export interface SlotItem {
  /** Unique id, must start with the extension id: `acme.stats.history-tab`. */
  id: string;
  /** Component rendered in the slot. */
  component: Component;
  /** Lower comes first. Default 100. */
  order?: number;
}

/** What an extension can use while it sets itself up. */
export interface ExtensionSetupContext {
  /** Adds an item to a slot. */
  addSlotItem(slot: SlotName, item: SlotItem): void;
  /** Adds a route under `/x/<extension id>/`. */
  addRoute(route: RouteRecordRaw): void;
  /** Calls one of this extension's backend commands by short name. */
  invoke<T = unknown>(command: string, payload?: unknown): Promise<T>;
}

/** The frontend half of an extension. Its id matches the backend extension. */
export interface UiExtension {
  id: string;
  setup(ctx: ExtensionSetupContext): void;
}

/** Calls a backend extension command by full name. */
export type ExtensionInvoker = <T = unknown>(command: string, payload?: unknown) => Promise<T>;

const DOTTED_ID = /^[a-z0-9_]+(\.[a-z0-9_]+)*$/;

function belongsTo(id: string, extensionId: string): boolean {
  return id.startsWith(`${extensionId}.`) && id.length > extensionId.length + 1;
}

/** Holds every slot item and extension route. */
export class ExtensionRegistry {
  private readonly slots = shallowReactive(new Map<SlotName, SlotItem[]>());
  private readonly routes: RouteRecordRaw[] = [];
  private readonly extensionIds = new Set<string>();

  constructor(private readonly invoker: ExtensionInvoker) {}

  /** Runs `setup` for each extension. Throws on the first broken one. */
  install(extensions: readonly UiExtension[]): void {
    for (const extension of extensions) this.installOne(extension);
  }

  private installOne(extension: UiExtension): void {
    const { id } = extension;
    if (!DOTTED_ID.test(id)) throw new Error(`Invalid extension id "${id}".`);
    if (id === 'core' || id.startsWith('core.')) throw new Error(`Extension id "${id}" is reserved for the app.`);
    for (const existing of this.extensionIds) {
      if (existing === id || belongsTo(id, existing) || belongsTo(existing, id)) {
        throw new Error(`Extension id "${id}" conflicts with "${existing}".`);
      }
    }

    const pendingSlots: [SlotName, SlotItem][] = [];
    const pendingRoutes: RouteRecordRaw[] = [];
    const ctx: ExtensionSetupContext = {
      addSlotItem: (slot, item) => {
        if (!SLOT_NAMES.includes(slot)) throw new Error(`Unknown slot "${slot}".`);
        if (!belongsTo(item.id, id)) throw new Error(`Slot item "${item.id}" must start with "${id}.".`);
        const taken = [...(this.slots.get(slot) ?? []), ...pendingSlots.filter(([s]) => s === slot).map(([, i]) => i)];
        if (taken.some((i) => i.id === item.id)) throw new Error(`Slot item "${item.id}" is already registered.`);
        pendingSlots.push([slot, item]);
      },
      addRoute: (route) => {
        const prefix = `/x/${id}/`;
        if (typeof route.path !== 'string' || !route.path.startsWith(prefix)) {
          throw new Error(`Route "${String(route.path)}" must start with "${prefix}".`);
        }
        pendingRoutes.push(route);
      },
      invoke: (command, payload) => this.invoker(`${id}.${command}`, payload),
    };

    extension.setup(ctx);

    this.extensionIds.add(id);
    for (const [slot, item] of pendingSlots) {
      const items = [...(this.slots.get(slot) ?? []), item];
      items.sort((a, b) => (a.order ?? 100) - (b.order ?? 100) || a.id.localeCompare(b.id));
      this.slots.set(slot, items);
    }
    this.routes.push(...pendingRoutes);
  }

  /** Items in a slot, in display order. Empty when nothing is registered. */
  items(slot: SlotName): readonly SlotItem[] {
    return this.slots.get(slot) ?? [];
  }

  /** Routes added by extensions. */
  extensionRoutes(): readonly RouteRecordRaw[] {
    return this.routes;
  }

  /** Ids of installed UI extensions. */
  installed(): readonly string[] {
    return [...this.extensionIds];
  }
}
