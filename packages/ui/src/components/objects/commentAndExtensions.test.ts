import { flushPromises, mount } from '@vue/test-utils';
import type { VueWrapper } from '@vue/test-utils';
import type * as CatalogModule from '../../api/catalog';
import type * as DdlModule from '../../api/ddl';
import type { EngineChange, ObjectDetails, ObjectRef, PgAvailableExtension } from '../../api/ddl';
import type { SessionInfo } from '../../api/sessions';
import type { ObjectTab } from '../../composables/useTabs';
import CommentEditor from './CommentEditor.vue';
import ExtensionsManager from './ExtensionsManager.vue';
import ObjectTabView from './ObjectTabView.vue';

const planned: EngineChange[] = [];
const applied: string[] = [];
let details: Record<string, ObjectDetails> = {};
let available: PgAvailableExtension[] | Error = [];
let installedList: string[] = [];
const describeCalls: string[] = [];
/** Describes held back until the test releases them (latest-only checks). */
const held: { key: string; release: () => void }[] = [];
let holdDescribes = false;
/** Answers of the confirm dialog, in order; `true` when empty. */
const confirmAnswers: boolean[] = [];
const confirms: string[] = [];

vi.mock('../../api/ddl', async (original) => ({
  ...(await original<typeof DdlModule>()),
  catalogDescribe: vi.fn(async (_s: string, object: ObjectRef) => {
    const key = `${object.kind}:${object.name}`;
    describeCalls.push(key);
    if (holdDescribes) await new Promise<void>((release) => held.push({ key, release }));
    const found = details[key];
    if (!found) throw { kind: 'catalog', error: { kind: 'notFound', message: `${object.kind} ${object.name} not found` } };
    return found;
  }),
  catalogAvailableExtensions: vi.fn(async () => {
    if (available instanceof Error) throw { kind: 'catalog', error: { kind: 'driver', message: available.message } };
    return available;
  }),
  editPlan: vi.fn(async (_s: string, change: EngineChange) => {
    planned.push(change);
    return { planId: `p${planned.length}`, plan: { statements: ['COMMENT ON planned'], transactional: true, destructive: false, warnings: [] } };
  }),
  editApply: vi.fn(async (_s: string, planId: string) => {
    applied.push(planId);
    return { statements: 1, transactional: true, outputs: [], durationMs: 1 };
  }),
}));
vi.mock('../../api/catalog', async (original) => ({
  ...(await original<typeof CatalogModule>()),
  catalogList: vi.fn(async () =>
    installedList.map((name) => ({ object: { kind: 'extension', database: 'shop', schema: null, parent: null, name, signature: null }, comment: null, detail: '1.0 in public', flags: [], hasChildren: false, system: false, rowEstimate: null, sizeBytes: null })),
  ),
  catalogSchemas: vi.fn(async () =>
    ['public', 'ext', 'pg_catalog'].map((name) => ({ object: { kind: 'schema', database: 'shop', schema: null, parent: null, name, signature: null }, system: name === 'pg_catalog' })),
  ),
}));
vi.mock('../../composables/useDialogs', () => ({
  useDialogs: () => ({
    askConfirm: vi.fn(async (request: { title: string }) => {
      confirms.push(request.title);
      return confirmAnswers.shift() ?? true;
    }),
    askDestructive: vi.fn(async () => true),
  }),
}));
vi.mock('../../api/activity', () => ({ fetchActivity: vi.fn(async () => ({ events: [] })) }));
vi.mock('../tree/objectBrowser', () => ({ useObjectBrowser: () => ({ refresh: vi.fn(async () => undefined) }) }));
vi.mock('../notices/useNotices', () => ({ useNotices: () => ({ refreshRestores: vi.fn(async () => undefined) }) }));
vi.mock('../../composables/useWorkspace', async () => {
  const { ref: vueRef } = await import('vue');
  const records = vueRef({ connections: [], folders: [], queries: [] });
  return { useWorkspace: () => ({ records, connection: () => null, refresh: vi.fn(async () => undefined) }) };
});

function session(engine: SessionInfo['engine'], extra: Partial<SessionInfo> = {}): SessionInfo {
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
    ...extra,
  } as SessionInfo;
}

const constraint: ObjectRef = { kind: 'constraint', database: 'shop', schema: 'public', parent: 'orders', name: 'orders_pk', signature: null };
const cast: ObjectRef = { kind: 'cast', database: 'shop', schema: null, parent: null, name: 'text AS shop.money', signature: 'text AS shop.money' };

function ext(name: string, extra: Partial<PgAvailableExtension> = {}): PgAvailableExtension {
  return {
    name,
    defaultVersion: '1.6',
    versions: ['1.4', '1.5', '1.6'],
    installedVersion: null,
    installedSchema: null,
    comment: `${name} things`,
    relocatable: true,
    fixedSchema: null,
    superuser: false,
    trusted: true,
    requires: [],
    ...extra,
    versionDetails:
      extra.versionDetails ??
      (extra.versions ?? ['1.4', '1.5', '1.6']).map((version) => ({
        version,
        superuser: extra.superuser ?? false,
        trusted: extra.trusted ?? true,
        relocatable: extra.relocatable ?? true,
        fixedSchema: extra.fixedSchema ?? null,
        requires: extra.requires ?? [],
      })),
  };
}

function button(wrapper: VueWrapper, text: string) {
  const found = wrapper.findAll('button').find((b) => b.text() === text);
  if (!found) throw new Error(`no button ${text}`);
  return found;
}

function field(wrapper: VueWrapper, label: string) {
  const el = wrapper.findAll('label').find((l) => l.text().startsWith(label));
  const id = el?.attributes('for');
  if (!id) throw new Error(`no field ${label}`);
  return wrapper.find(`[id="${id}"]`);
}

beforeEach(() => {
  planned.length = 0;
  applied.length = 0;
  describeCalls.length = 0;
  held.length = 0;
  holdDescribes = false;
  confirmAnswers.length = 0;
  confirms.length = 0;
  installedList = [];
  available = [];
  details = {
    'constraint:orders_pk': { engine: 'postgres', object: { kind: 'constraint', name: 'orders_pk', comment: 'old' } } as unknown as ObjectDetails,
    'cast:text AS shop.money': { engine: 'postgres', object: { kind: 'cast', meta: { owner: null, comment: null, extension: null, system: false } } } as unknown as ObjectDetails,
  };
});

describe('CommentEditor', () => {
  it('reads the comment, plans COMMENT ON for the exact object and reloads after applying', async () => {
    const dirty: boolean[] = [];
    const wrapper = mount(CommentEditor, {
      props: { session: session('postgres'), connectionName: 'Shop', object: constraint, reportDirty: (d: boolean) => dirty.push(d) },
      global: { stubs: { SqlCode: true } },
    });
    await flushPromises();
    const box = field(wrapper, 'Comment');
    expect((box.element as HTMLTextAreaElement).value).toBe('old');
    expect(button(wrapper, 'Preview change').attributes('disabled')).toBeDefined();
    await box.setValue("the order's key");
    expect(dirty).toEqual([true]);
    await button(wrapper, 'Preview change').trigger('click');
    await flushPromises();
    expect(planned).toEqual([{ engine: 'postgres', change: { object: 'comment', target: constraint, comment: "the order's key" } }]);
    expect(wrapper.text()).toContain('Will run');
    details['constraint:orders_pk'] = { engine: 'postgres', object: { kind: 'constraint', name: 'orders_pk', comment: "the order's key" } } as unknown as ObjectDetails;
    await button(wrapper, 'Set comment').trigger('click');
    await flushPromises();
    expect(applied).toEqual(['p1']);
    expect(wrapper.emitted('changed')).toHaveLength(1);
    expect(describeCalls).toEqual(['constraint:orders_pk', 'constraint:orders_pk']);
    expect(dirty.at(-1)).toBe(false);
  });

  it('removes the comment with a blank text, read from meta.comment', async () => {
    details['cast:text AS shop.money'] = { engine: 'postgres', object: { kind: 'cast', meta: { comment: 'as money' } } } as unknown as ObjectDetails;
    const wrapper = mount(CommentEditor, { props: { session: session('postgres'), connectionName: 'Shop', object: cast }, global: { stubs: { SqlCode: true } } });
    await flushPromises();
    await field(wrapper, 'Comment').setValue('  ');
    await button(wrapper, 'Preview change').trigger('click');
    await flushPromises();
    expect(planned.at(-1)).toEqual({ engine: 'postgres', change: { object: 'comment', target: cast, comment: null } });
  });

  it('refuses to plan on a read-only session', async () => {
    const wrapper = mount(CommentEditor, { props: { session: session('postgres', { readOnly: true }), connectionName: 'Shop', object: constraint }, global: { stubs: { SqlCode: true } } });
    await flushPromises();
    expect(wrapper.text()).toContain('Read only');
    expect(field(wrapper, 'Comment').attributes('disabled')).toBeDefined();
    expect(button(wrapper, 'Preview change').attributes('disabled')).toBeDefined();
    expect(planned).toEqual([]);
  });

  it('says so when the details carry no comment, and renders nothing on MySQL', async () => {
    details['cast:text AS shop.money'] = { engine: 'postgres', object: { kind: 'cast' } } as unknown as ObjectDetails;
    const pg = mount(CommentEditor, { props: { session: session('postgres'), connectionName: 'Shop', object: cast }, global: { stubs: { SqlCode: true } } });
    await flushPromises();
    // No comment field is not the same as a hidden comment (CM-R5).
    expect(pg.find('[role="alert"]').text()).toContain('do not include its comment');
    expect(pg.find('[role="alert"]').text()).not.toContain('with this role');
    const my = mount(CommentEditor, {
      props: { session: session('mysql'), connectionName: 'Shop', object: { ...constraint, schema: null } },
      global: { stubs: { SqlCode: true } },
    });
    await flushPromises();
    expect(my.find('section').exists()).toBe(false);
  });
});

describe('CommentEditor safety', () => {
  const sub: ObjectRef = { kind: 'subscription', database: 'shop', schema: null, parent: null, name: 'feed', signature: null };

  it('shows an unreadable subscription comment as unknown, never as an empty editable field (CM-M1)', async () => {
    details['subscription:feed'] = { engine: 'postgres', object: { kind: 'subscription', commentVisible: false, meta: { comment: null } } } as unknown as ObjectDetails;
    const wrapper = mount(CommentEditor, { props: { session: session('postgres'), connectionName: 'Shop', object: sub }, global: { stubs: { SqlCode: true } } });
    await flushPromises();
    expect(wrapper.find('[role="alert"]').text()).toContain('cannot be read with this role');
    expect(field(wrapper, 'Comment').attributes('disabled')).toBeDefined();
    expect(button(wrapper, 'Preview change').attributes('disabled')).toBeDefined();
  });

  it('does not count a stored CRLF comment shown with LF as unsaved (CM-R4)', async () => {
    details['constraint:orders_pk'] = { engine: 'postgres', object: { kind: 'constraint', name: 'orders_pk', comment: 'line 1\r\nline 2' } } as unknown as ObjectDetails;
    const dirty: boolean[] = [];
    const wrapper = mount(CommentEditor, { props: { session: session('postgres'), connectionName: 'Shop', object: constraint, reportDirty: (d: boolean) => dirty.push(d) }, global: { stubs: { SqlCode: true } } });
    await flushPromises();
    await field(wrapper, 'Comment').setValue('line 1\nline 2');
    expect(dirty).toEqual([]);
    expect(button(wrapper, 'Preview change').attributes('disabled')).toBeDefined();
    await field(wrapper, 'Comment').setValue('line 1\nline 3');
    expect(dirty).toEqual([true]);
  });

  it('keeps the comment exactly as typed (CM-L7)', async () => {
    const wrapper = mount(CommentEditor, { props: { session: session('postgres'), connectionName: 'Shop', object: constraint }, global: { stubs: { SqlCode: true } } });
    await flushPromises();
    await field(wrapper, 'Comment').setValue('  key\nline 2 ');
    await button(wrapper, 'Preview change').trigger('click');
    await flushPromises();
    expect(planned.at(-1)).toEqual({ engine: 'postgres', change: { object: 'comment', target: constraint, comment: '  key\nline 2 ' } });
    expect(wrapper.text()).toContain('Saved exactly as typed');
  });

  it('asks before Reload discards typed text (CM-L1)', async () => {
    const wrapper = mount(CommentEditor, { props: { session: session('postgres'), connectionName: 'Shop', object: constraint }, global: { stubs: { SqlCode: true } } });
    await flushPromises();
    await field(wrapper, 'Comment').setValue('typed');
    confirmAnswers.push(false);
    await wrapper.find('[aria-label="Reload the comment"]').trigger('click');
    await flushPromises();
    expect(confirms).toEqual(['Discard the typed comment?']);
    expect((field(wrapper, 'Comment').element as HTMLTextAreaElement).value).toBe('typed');
    confirmAnswers.push(true);
    await wrapper.find('[aria-label="Reload the comment"]').trigger('click');
    await flushPromises();
    expect((field(wrapper, 'Comment').element as HTMLTextAreaElement).value).toBe('old');
    // Nothing typed: reload without asking.
    await wrapper.find('[aria-label="Reload the comment"]').trigger('click');
    await flushPromises();
    expect(confirms).toHaveLength(2);
  });

  it('shows the comment of the latest object when an older read answers last (CM-L2)', async () => {
    details['constraint:other_pk'] = { engine: 'postgres', object: { kind: 'constraint', name: 'other_pk', comment: 'newer' } } as unknown as ObjectDetails;
    holdDescribes = true;
    const wrapper = mount(CommentEditor, { props: { session: session('postgres'), connectionName: 'Shop', object: constraint }, global: { stubs: { SqlCode: true } } });
    await flushPromises();
    await wrapper.setProps({ object: { ...constraint, name: 'other_pk' } });
    await flushPromises();
    expect(held.map((h) => h.key)).toEqual(['constraint:orders_pk', 'constraint:other_pk']);
    held[1]?.release();
    await flushPromises();
    held[0]?.release();
    await flushPromises();
    expect((field(wrapper, 'Comment').element as HTMLTextAreaElement).value).toBe('newer');
  });

  it('renders nothing for MySQL routines, whose comment is in the routine editor (CM-L3)', async () => {
    const proc: ObjectRef = { kind: 'procedure', database: 'shop', schema: null, parent: null, name: 'p', signature: null };
    const wrapper = mount(CommentEditor, { props: { session: session('mariadb'), connectionName: 'Shop', object: proc }, global: { stubs: { SqlCode: true } } });
    await flushPromises();
    expect(wrapper.find('section').exists()).toBe(false);
    expect(describeCalls).toEqual([]);
  });
});

describe('ObjectTabView fallback', () => {
  it('shows the comment editor above the DDL for a kind without an editor', async () => {
    const tab = { id: 't1', kind: 'object', connectionId: 'c1', title: 'orders_pk', session: session('postgres'), database: 'shop', object: constraint, createKind: null, container: { database: 'shop', schema: 'public', parent: null }, intent: 'view' } as unknown as ObjectTab;
    const wrapper = mount(ObjectTabView, { props: { tab }, global: { stubs: { DdlView: true, SqlCode: true } } });
    await flushPromises();
    expect(wrapper.text()).toContain('Change its comment below');
    expect(wrapper.findComponent(CommentEditor).exists()).toBe(true);
    expect(wrapper.findComponent({ name: 'DdlView' }).exists()).toBe(true);
  });
});

describe('ExtensionsManager install picker', () => {
  it('lists what is not installed, picks a version, schema and CASCADE, and plans the install', async () => {
    installedList = ['plpgsql', 'cube'];
    available = [
      ext('citext'),
      ext('cube', { installedVersion: '1.5', installedSchema: 'public' }),
      ext('earthdistance', { requires: ['cube'] }),
      ext('postgis', { requires: ['plpgsql', 'missing_dep'], superuser: true, trusted: false }),
      ext('plpgsql', { installedVersion: '1.0', installedSchema: 'pg_catalog', fixedSchema: 'pg_catalog' }),
    ];
    const wrapper = mount(ExtensionsManager, { props: { session: session('postgres'), connectionName: 'Shop', object: null, createKind: 'extension', container: { database: 'shop', schema: null, parent: null } }, global: { stubs: { SqlCode: true } } });
    await flushPromises();
    const list = wrapper.find('[aria-label="Available extensions"]');
    expect(list.text()).toContain('citext');
    expect(list.text()).toContain('earthdistance');
    expect(list.text()).not.toContain('plpgsql');
    expect(list.text()).toContain('superuser');
    expect(button(wrapper, 'Preview install').attributes('disabled')).toBeDefined();

    await wrapper.find('input[type="search"]').setValue('earth');
    expect(wrapper.find('[aria-label="Available extensions"]').text()).not.toContain('citext');
    await wrapper.find('[aria-label="Available extensions"] button').trigger('click');
    // cube is installed: no CASCADE needed.
    expect((wrapper.find('label.check:not(.inline) input').element as HTMLInputElement).checked).toBe(false);
    await wrapper.find('input[type="search"]').setValue('');

    await wrapper.findAll('[aria-label="Available extensions"] button').find((b) => b.text().startsWith('postgis'))?.trigger('click');
    expect(wrapper.text()).toContain('not installed here: missing_dep');
    expect(wrapper.text()).toContain('Only a superuser can install it.');
    expect((wrapper.find('label.check:not(.inline) input').element as HTMLInputElement).checked).toBe(true);
    const versions = field(wrapper, 'Version').findAll('option').map((o) => o.text());
    expect(versions).toEqual(['default (1.6)', '1.6 (default)', '1.5', '1.4']);
    const schemas = field(wrapper, 'Schema').findAll('option').map((o) => o.text());
    expect(schemas).toEqual(['default (first schema in search_path)', 'public', 'ext']);
    await field(wrapper, 'Version').setValue('1.5');
    await field(wrapper, 'Schema').setValue('ext');
    await button(wrapper, 'Preview install').trigger('click');
    await flushPromises();
    expect(planned.at(-1)).toEqual({ engine: 'postgres', change: { object: 'extension', action: 'create', name: 'postgis', schema: 'ext', version: '1.5', cascade: true } });
  });

  it('locks the schema an extension fixes and refuses an installed one', async () => {
    available = [ext('adminpack', { fixedSchema: 'pg_catalog' }), ext('hstore', { installedVersion: '1.8', installedSchema: 'public' })];
    const wrapper = mount(ExtensionsManager, { props: { session: session('postgres'), connectionName: 'Shop', object: null, createKind: 'extension', container: { database: 'shop', schema: null, parent: null } }, global: { stubs: { SqlCode: true } } });
    await flushPromises();
    await wrapper.find('[aria-label="Available extensions"] button').trigger('click');
    expect(field(wrapper, 'Schema').attributes('disabled')).toBeDefined();
    expect(wrapper.text()).toContain('Its control file installs it into pg_catalog.');
    await wrapper.find('label.check.inline input').setValue(true);
    await wrapper.findAll('[aria-label="Available extensions"] button').find((b) => b.text().startsWith('hstore'))?.trigger('click');
    expect(wrapper.text()).toContain('Already installed in this database');
    expect(button(wrapper, 'Preview install').attributes('disabled')).toBeDefined();
  });

  it('falls back to a typed name when the list cannot be read', async () => {
    available = new Error('permission denied for pg_available_extensions');
    const errors: unknown[] = [];
    const wrapper = mount(ExtensionsManager, {
      props: { session: session('postgres'), connectionName: 'Shop', object: null, createKind: 'extension', container: { database: 'shop', schema: null, parent: null }, reportError: (e: unknown) => errors.push(e) },
      global: { stubs: { SqlCode: true } },
    });
    await flushPromises();
    expect(wrapper.text()).toContain('could not be read');
    expect(errors).toHaveLength(1);
    await field(wrapper, 'Name').setValue('pg_trgm');
    await field(wrapper, 'Version').setValue('1.6');
    await button(wrapper, 'Preview install').trigger('click');
    await flushPromises();
    expect(planned.at(-1)).toEqual({ engine: 'postgres', change: { object: 'extension', action: 'create', name: 'pg_trgm', schema: null, version: '1.6', cascade: false } });
  });

  it('edits the comment of the selected extension', async () => {
    installedList = ['hstore'];
    available = [ext('hstore', { installedVersion: '1.8', installedSchema: 'public' })];
    details['extension:hstore'] = {
      engine: 'postgres',
      object: { kind: 'extension', name: 'hstore', schema: 'public', version: '1.8', relocatable: true, defaultVersion: '1.8', availableUpdates: [], updateAvailable: false, description: 'key/value', requires: [], configTables: [], members: [], meta: { owner: 'app', comment: null, extension: null, system: false } },
    } as unknown as ObjectDetails;
    const hstore: ObjectRef = { kind: 'extension', database: 'shop', schema: null, parent: null, name: 'hstore', signature: null };
    const wrapper = mount(ExtensionsManager, { props: { session: session('postgres'), connectionName: 'Shop', object: hstore }, global: { stubs: { SqlCode: true } } });
    await flushPromises();
    await field(wrapper, 'Comment').setValue('tags');
    await button(wrapper, 'Preview change').trigger('click');
    await flushPromises();
    expect(planned.at(-1)).toEqual({ engine: 'postgres', change: { object: 'comment', target: hstore, comment: 'tags' } });
  });

  it('turns install off on a read-only session', async () => {
    available = [ext('citext')];
    const wrapper = mount(ExtensionsManager, { props: { session: session('postgres', { readOnly: true }), connectionName: 'Shop', object: null, createKind: 'extension', container: { database: 'shop', schema: null, parent: null } }, global: { stubs: { SqlCode: true } } });
    await flushPromises();
    await wrapper.find('[aria-label="Available extensions"] button').trigger('click');
    expect(button(wrapper, 'Preview install').attributes('disabled')).toBeDefined();
  });

  function installedDetails(name: string, updates: string[] = []): ObjectDetails {
    return {
      engine: 'postgres',
      object: { kind: 'extension', name, schema: 'public', version: '1.0', relocatable: true, defaultVersion: '1.1', availableUpdates: updates, updateAvailable: updates.length > 0, description: null, requires: [], configTables: [], members: [], meta: { owner: 'app', comment: null, extension: null, system: false } },
    } as unknown as ObjectDetails;
  }
  const extRef = (name: string): ObjectRef => ({ kind: 'extension', database: 'shop', schema: null, parent: null, name, signature: null });
  const newExtension = { object: null, createKind: 'extension' as const, container: { database: 'shop', schema: null, parent: null } };

  it('discards the install preview when version, schema or CASCADE change (CM-M2)', async () => {
    available = [ext('citext')];
    const wrapper = mount(ExtensionsManager, { props: { session: session('postgres'), connectionName: 'Shop', ...newExtension }, global: { stubs: { SqlCode: true } } });
    await flushPromises();
    await wrapper.find('[aria-label="Available extensions"] button').trigger('click');
    for (const change of [
      async () => field(wrapper, 'Version').setValue('1.4'),
      async () => field(wrapper, 'Schema').setValue('ext'),
      async () => wrapper.find('label.check:not(.inline) input').setValue(true),
    ]) {
      await button(wrapper, 'Preview install').trigger('click');
      await flushPromises();
      expect(wrapper.findAll('button').some((b) => b.text() === 'Install')).toBe(true);
      await change();
      await flushPromises();
      expect(wrapper.findAll('button').some((b) => b.text() === 'Install')).toBe(false);
    }
    await button(wrapper, 'Preview install').trigger('click');
    await flushPromises();
    await button(wrapper, 'Install').trigger('click');
    await flushPromises();
    expect(planned.at(-1)).toEqual({ engine: 'postgres', change: { object: 'extension', action: 'create', name: 'citext', schema: 'ext', version: '1.4', cascade: true } });
    expect(applied).toEqual([`p${planned.length}`]);
  });

  it('discards a Drop preview when another extension is selected (CM-M2)', async () => {
    installedList = ['hstore', 'citext'];
    details['extension:hstore'] = installedDetails('hstore');
    details['extension:citext'] = installedDetails('citext');
    const wrapper = mount(ExtensionsManager, { props: { session: session('postgres'), connectionName: 'Shop', object: extRef('hstore') }, global: { stubs: { SqlCode: true } } });
    await flushPromises();
    await button(wrapper, 'Drop').trigger('click');
    await flushPromises();
    expect(planned.at(-1)).toMatchObject({ change: { action: 'drop', name: 'hstore' } });
    expect(wrapper.findAll('button').filter((b) => b.text() === 'Drop')).toHaveLength(2);
    await wrapper.findAll('[aria-label="Installed extensions"] button').find((b) => b.text().startsWith('citext'))?.trigger('click');
    await flushPromises();
    // Only the section's own Drop button is left; the preview for hstore is gone.
    expect(wrapper.findAll('button').filter((b) => b.text() === 'Drop')).toHaveLength(1);
    expect(applied).toEqual([]);
  });

  it('reports a typed extension comment as unsaved and asks before switching (CM-M3)', async () => {
    installedList = ['hstore', 'citext'];
    details['extension:hstore'] = installedDetails('hstore', ['1.1']);
    details['extension:citext'] = installedDetails('citext');
    const dirty: boolean[] = [];
    const wrapper = mount(ExtensionsManager, {
      props: { session: session('postgres'), connectionName: 'Shop', object: extRef('hstore'), reportDirty: (d: boolean) => dirty.push(d) },
      global: { stubs: { SqlCode: true } },
    });
    await flushPromises();
    await field(wrapper, 'Comment').setValue('key/value pairs');
    expect(dirty.at(-1)).toBe(true);
    // An Update of the same extension keeps the typed text.
    await button(wrapper, 'Update').trigger('click');
    await flushPromises();
    await wrapper.findAll('button').filter((b) => b.text() === 'Update').at(-1)?.trigger('click');
    await flushPromises();
    expect((field(wrapper, 'Comment').element as HTMLTextAreaElement).value).toBe('key/value pairs');
    // Switching asks; "no" keeps the extension and the text.
    const citext = () => wrapper.findAll('[aria-label="Installed extensions"] button').find((b) => b.text().startsWith('citext'));
    confirmAnswers.push(false);
    await citext()?.trigger('click');
    await flushPromises();
    expect(confirms).toEqual(['Discard the typed comment?']);
    expect((field(wrapper, 'Comment').element as HTMLTextAreaElement).value).toBe('key/value pairs');
    confirmAnswers.push(true);
    await citext()?.trigger('click');
    await flushPromises();
    expect(wrapper.find('[aria-label="Extension citext"]').exists()).toBe(true);
    expect((field(wrapper, 'Comment').element as HTMLTextAreaElement).value).toBe('');
    expect(dirty.at(-1)).toBe(false);
  });

  it('follows the chosen version for requires, rights and schema (CM-L5)', async () => {
    installedList = ['cube'];
    available = [
      ext('geo', {
        requires: [],
        versionDetails: [
          { version: '1.4', superuser: true, trusted: false, relocatable: false, fixedSchema: 'geo', requires: ['cube', 'missing_dep'] },
          { version: '1.5', superuser: false, trusted: true, relocatable: true, fixedSchema: null, requires: [] },
          { version: '1.6', superuser: false, trusted: true, relocatable: true, fixedSchema: null, requires: [] },
        ],
      }),
      ext('cube', { installedVersion: '1.5', installedSchema: 'public' }),
    ];
    const wrapper = mount(ExtensionsManager, { props: { session: session('postgres'), connectionName: 'Shop', ...newExtension }, global: { stubs: { SqlCode: true } } });
    await flushPromises();
    await wrapper.find('[aria-label="Available extensions"] button').trigger('click');
    const cascadeBox = () => wrapper.find('label.check:not(.inline) input').element as HTMLInputElement;
    expect(wrapper.text()).not.toContain('Requires');
    expect(cascadeBox().checked).toBe(false);
    await field(wrapper, 'Version').setValue('1.4');
    await flushPromises();
    expect(wrapper.text()).toContain('Requires cube, missing_dep; not installed here: missing_dep.');
    expect(wrapper.text()).toContain('Only a superuser can install it.');
    expect(wrapper.text()).toContain('Its control file installs it into geo.');
    expect(field(wrapper, 'Schema').attributes('disabled')).toBeDefined();
    expect(cascadeBox().checked).toBe(true);
    await field(wrapper, 'Version').setValue('1.5');
    await flushPromises();
    expect(wrapper.text()).not.toContain('Requires');
    expect(cascadeBox().checked).toBe(false);
  });

  it('keeps an unapplied comment when another extension is installed (CM-R1)', async () => {
    installedList = ['hstore'];
    available = [ext('hstore', { installedVersion: '1.8', installedSchema: 'public' }), ext('citext')];
    details['extension:hstore'] = installedDetails('hstore');
    details['extension:citext'] = installedDetails('citext');
    const wrapper = mount(ExtensionsManager, { props: { session: session('postgres'), connectionName: 'Shop', object: extRef('hstore') }, global: { stubs: { SqlCode: true } } });
    await flushPromises();
    await field(wrapper, 'Comment').setValue('typed for hstore');
    await wrapper.findAll('[aria-label="Available extensions"] button').find((b) => b.text().startsWith('citext'))?.trigger('click');
    await button(wrapper, 'Preview install').trigger('click');
    await flushPromises();
    await button(wrapper, 'Install').trigger('click');
    await flushPromises();
    expect(planned.at(-1)).toMatchObject({ change: { action: 'create', name: 'citext' } });
    expect(wrapper.find('[aria-label="Extension hstore"]').exists()).toBe(true);
    expect((field(wrapper, 'Comment').element as HTMLTextAreaElement).value).toBe('typed for hstore');
    expect(confirms).toEqual([]);
  });

  it('selects the installed extension when no comment is pending', async () => {
    installedList = ['hstore'];
    available = [ext('hstore', { installedVersion: '1.8', installedSchema: 'public' }), ext('citext')];
    details['extension:hstore'] = installedDetails('hstore');
    details['extension:citext'] = installedDetails('citext');
    const wrapper = mount(ExtensionsManager, { props: { session: session('postgres'), connectionName: 'Shop', object: extRef('hstore') }, global: { stubs: { SqlCode: true } } });
    await flushPromises();
    await wrapper.findAll('[aria-label="Available extensions"] button').find((b) => b.text().startsWith('citext'))?.trigger('click');
    await button(wrapper, 'Preview install').trigger('click');
    await flushPromises();
    await button(wrapper, 'Install').trigger('click');
    await flushPromises();
    expect(wrapper.find('[aria-label="Extension citext"]').exists()).toBe(true);
  });
});
