import { mount } from '@vue/test-utils';
import { defineComponent, h } from 'vue';
import ExtensionSlot from './components/ExtensionSlot.vue';
import { ExtensionRegistry } from './extensions';
import type { ExtensionInvoker, UiExtension } from './extensions';
import { REGISTRY_KEY } from './keys';

const Dummy = defineComponent({ props: { label: { type: String, default: 'x' } }, render: () => h('span', 'item') });

function registry() {
  const invoker = vi.fn(async (_command: string, _payload?: unknown): Promise<unknown> => 'ok');
  return { reg: new ExtensionRegistry(invoker as unknown as ExtensionInvoker), invoker };
}

describe('ExtensionRegistry', () => {
  it('adds slot items in order and namespaces commands', async () => {
    const { reg, invoker } = registry();
    const ext: UiExtension = {
      id: 'pro.test',
      setup(ctx) {
        ctx.addSlotItem('tools', { id: 'pro.test.b', component: Dummy, order: 20 });
        ctx.addSlotItem('tools', { id: 'pro.test.a', component: Dummy, order: 10 });
        void ctx.invoke('ping', { n: 1 });
      },
    };
    reg.install([ext]);
    expect(reg.items('tools').map((i) => i.id)).toEqual(['pro.test.a', 'pro.test.b']);
    expect(reg.items('main-menu')).toEqual([]);
    expect(invoker).toHaveBeenCalledWith('pro.test.ping', { n: 1 });
  });

  it('rejects items and routes outside the extension namespace', () => {
    const { reg } = registry();
    expect(() =>
      reg.install([{ id: 'pro.test', setup: (ctx) => ctx.addSlotItem('tools', { id: 'other.item', component: Dummy }) }]),
    ).toThrow(/must start with "pro.test."/);
    expect(() =>
      reg.install([{ id: 'pro.routes', setup: (ctx) => ctx.addRoute({ path: '/settings', component: Dummy }) }]),
    ).toThrow(/must start with "\/x\/pro.routes\/"/);
  });

  it('keeps nothing from an extension whose setup fails', () => {
    const { reg } = registry();
    expect(() =>
      reg.install([
        {
          id: 'pro.broken',
          setup(ctx) {
            ctx.addSlotItem('tools', { id: 'pro.broken.ok', component: Dummy });
            throw new Error('boom');
          },
        },
      ]),
    ).toThrow('boom');
    expect(reg.items('tools')).toEqual([]);
    expect(reg.installed()).toEqual([]);
  });

  it('rejects reserved, invalid and conflicting ids', () => {
    const { reg } = registry();
    expect(() => reg.install([{ id: 'core', setup: () => {} }])).toThrow(/reserved/);
    expect(() => reg.install([{ id: 'Bad-Id', setup: () => {} }])).toThrow(/Invalid/);
    reg.install([{ id: 'pro.a', setup: () => {} }]);
    expect(() => reg.install([{ id: 'pro.a.b', setup: () => {} }])).toThrow(/conflicts/);
  });

  it('renders nothing for an empty slot and the items of a filled one', () => {
    const { reg } = registry();
    const empty = mount(ExtensionSlot, { props: { name: 'tools' }, global: { provide: { [REGISTRY_KEY as symbol]: reg } } });
    expect(empty.text()).toBe('');
    reg.install([{ id: 'pro.view', setup: (ctx) => ctx.addSlotItem('tools', { id: 'pro.view.item', component: Dummy }) }]);
    const filled = mount(ExtensionSlot, { props: { name: 'tools' }, global: { provide: { [REGISTRY_KEY as symbol]: reg } } });
    expect(filled.text()).toBe('item');
  });
});
