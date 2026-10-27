import { flushPromises, shallowMount } from '@vue/test-utils';
import type { Component } from 'vue';
import type { ObjectKind } from '../../api/catalog';
import type { SessionInfo } from '../../api/sessions';
import type { ObjectTab } from '../../composables/useTabs';
import { canEdit, hasObjectEditor } from '../../lib/editorKinds';
import EventEditor from './EventEditor.vue';
import EventTriggerEditor from './EventTriggerEditor.vue';
import ExtensionsManager from './ExtensionsManager.vue';
import MatViewEditor from './MatViewEditor.vue';
import ObjectTabView from './ObjectTabView.vue';
import RoutineEditor from './RoutineEditor.vue';
import SequenceEditor from './SequenceEditor.vue';
import TriggerEditor from './TriggerEditor.vue';
import TypeEditor from './TypeEditor.vue';
import ViewEditor from './ViewEditor.vue';

vi.mock('../tree/objectBrowser', () => ({ useObjectBrowser: () => ({ refresh: vi.fn(async () => undefined) }) }));
vi.mock('../../composables/useWorkspace', async () => {
  const { ref: vueRef } = await import('vue');
  const records = vueRef({ connections: [], folders: [], queries: [] });
  return { useWorkspace: () => ({ records, connection: () => null, refresh: vi.fn(async () => undefined) }) };
});

function session(engine: SessionInfo['engine']): SessionInfo {
  return {
    id: 's1',
    connectionId: 'c1',
    engine,
    server: { engine, version: '18.0', major: 18, minor: 0, patch: 0 },
    environment: 'development',
    database: 'shop',
    readOnly: false,
    confirmDestructive: false,
    purpose: 'user',
    openedAt: 0,
    encryption: { tls: true, mode: 'prefer', passwordUnencrypted: false },
  } as SessionInfo;
}

/** The tab a tree "New" entry opens (objectActions.ts `new-kind:<kind>` and `new-trigger`). */
function createTab(engine: SessionInfo['engine'], createKind: ObjectKind, schema: string | null, parent: string | null): ObjectTab {
  return {
    id: 't1',
    kind: 'object',
    connectionId: 'c1',
    title: 'New',
    session: session(engine),
    database: 'shop',
    object: null,
    createKind,
    container: {
      database: 'shop',
      schema,
      parent: parent === null ? null : { kind: 'table', database: 'shop', schema, parent: null, name: parent, signature: null },
    },
    intent: 'create',
  } as unknown as ObjectTab;
}

describe('the editor a tree New entry opens', () => {
  const cases: [SessionInfo['engine'], ObjectKind, Component, string | null, string | null][] = [
    ['postgres', 'view', ViewEditor, 'fresh', null],
    ['postgres', 'materializedView', MatViewEditor, 'fresh', null],
    ['postgres', 'sequence', SequenceEditor, 'fresh', null],
    ['postgres', 'function', RoutineEditor, 'fresh', null],
    ['postgres', 'procedure', RoutineEditor, 'fresh', null],
    ['postgres', 'enumType', TypeEditor, 'fresh', null],
    ['postgres', 'compositeType', TypeEditor, 'fresh', null],
    ['postgres', 'rangeType', TypeEditor, 'fresh', null],
    ['postgres', 'domain', TypeEditor, 'fresh', null],
    ['postgres', 'eventTrigger', EventTriggerEditor, null, null],
    ['postgres', 'extension', ExtensionsManager, null, null],
    ['postgres', 'trigger', TriggerEditor, 'public', 'orders'],
    ['mysql', 'view', ViewEditor, null, null],
    ['mysql', 'function', RoutineEditor, null, null],
    ['mysql', 'procedure', RoutineEditor, null, null],
    ['mysql', 'event', EventEditor, null, null],
    ['mysql', 'trigger', TriggerEditor, null, 'orders'],
    ['mariadb', 'sequence', SequenceEditor, null, null],
  ];

  it.each(cases)('%s %s opens its editor with the kind and the container', async (engine, kind, editor, schema, parent) => {
    expect(hasObjectEditor(kind)).toBe(true);
    expect(canEdit(kind, { engine, kinds: [], editable: [kind], features: [] })).toBe(true);
    const wrapper = shallowMount(ObjectTabView, { props: { tab: createTab(engine, kind, schema, parent) } });
    await flushPromises();
    const found = wrapper.findComponent(editor);
    expect(found.exists()).toBe(true);
    const props = found.props() as Record<string, unknown>;
    expect(props.createKind).toBe(kind);
    expect(props.object).toBeNull();
    expect(props.container).toEqual({ database: 'shop', schema, parent });
  });
});
