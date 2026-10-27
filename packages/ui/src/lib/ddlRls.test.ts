import type { PgPolicy, PgRule } from '../api/ddl';
import {
  buildPgPolicy,
  buildPgPolicyDrop,
  buildPgRule,
  buildPgRuleDrop,
  parseRuleDefinition,
  pgPolicyErrors,
  pgPolicyFormFrom,
  pgRuleErrors,
  pgRuleFormFrom,
  policyNeedsRecreate,
  policyRoles,
  unwrapParens,
} from './ddlRls';

const table = { schema: 'app', name: 'orders' };

function rule(definition: string, extra: Partial<PgRule> = {}): PgRule {
  return { schema: 'app', table: 'orders', name: 'r', event: 'insert', instead: true, enabled: 'origin', definition, comment: null, ...extra };
}

describe('pg_get_ruledef parsing', () => {
  it('reads condition, mode and commands', () => {
    expect(parseRuleDefinition('CREATE RULE r AS\n    ON INSERT TO app.orders\n   WHERE (new.amount < 0) DO INSTEAD NOTHING;')).toEqual({
      condition: 'new.amount < 0',
      instead: true,
      actions: [],
    });
    expect(parseRuleDefinition("CREATE RULE r AS\n    ON UPDATE TO app.orders DO  INSERT INTO app.log (note) VALUES ('do; it'::text);")).toEqual({
      condition: '',
      instead: false,
      actions: ["INSERT INTO app.log (note) VALUES ('do; it'::text)"],
    });
    expect(
      parseRuleDefinition('CREATE RULE r AS\n    ON DELETE TO app.orders DO INSTEAD ( UPDATE app.orders SET gone = true\n  WHERE (orders.id = old.id);\n NOTIFY orders_changed;\n);'),
    ).toEqual({
      condition: '',
      instead: true,
      actions: ['UPDATE app.orders SET gone = true\n  WHERE (orders.id = old.id)', 'NOTIFY orders_changed'],
    });
    // PostgreSQL 18 prints the condition without brackets.
    expect(parseRuleDefinition('CREATE RULE r AS\n    ON INSERT TO app.orders\n   WHERE new.amount < 0 DO INSTEAD NOTHING')?.condition).toBe('new.amount < 0');
    expect(parseRuleDefinition('not a rule')).toBeNull();
  });

  it('ignores DO and WHERE inside strings, quoted names and brackets', () => {
    expect(parseRuleDefinition(`CREATE RULE r AS ON INSERT TO app."do" WHERE (new.note <> 'DO') DO ALSO SELECT 1`)).toEqual({
      condition: "new.note <> 'DO'",
      instead: false,
      actions: ['SELECT 1'],
    });
  });

  it('unwraps only brackets around the whole text', () => {
    expect(unwrapParens('(a < 0)')).toBe('a < 0');
    expect(unwrapParens('(a) AND (b)')).toBe('(a) AND (b)');
    expect(unwrapParens('(a) > (b)')).toBe('(a) > (b)');
  });
});

describe('rule form', () => {
  it('creates a rule and its state', () => {
    const form = { ...pgRuleFormFrom(null), name: 'm2_ed_r', event: 'delete' as const, condition: 'old.locked', actions: 'NOTHING_ELSE_HERE;', state: 'disabled' as const };
    form.actions = '';
    expect(pgRuleErrors(form)).toEqual({});
    expect(buildPgRule(form, null, table, 'shop').map((c) => c.change)).toEqual([
      { object: 'rule', action: 'create', name: 'm2_ed_r', table, orReplace: false, event: 'delete', condition: 'old.locked', instead: true, actions: [] },
      { object: 'rule', action: 'setState', name: 'm2_ed_r', table, state: 'disabled' },
    ]);
  });

  it('replaces a changed definition, then state, comment and the rename', () => {
    const original = rule('CREATE RULE r AS\n    ON INSERT TO app.orders\n   WHERE (new.amount < 0) DO INSTEAD NOTHING;');
    const form = { ...pgRuleFormFrom(original), condition: 'new.amount <= 0', state: 'always' as const, comment: 'no negatives', name: 'r2' };
    expect(buildPgRule(form, { rule: original, parts: parseRuleDefinition(original.definition) }, table, 'shop').map((c) => c.change)).toEqual([
      { object: 'rule', action: 'create', name: 'r', table, orReplace: true, event: 'insert', condition: 'new.amount <= 0', instead: true, actions: [] },
      { object: 'rule', action: 'setState', name: 'r', table, state: 'always' },
      { object: 'comment', target: { kind: 'rule', database: 'shop', schema: 'app', parent: 'orders', name: 'r', signature: null }, comment: 'no negatives' },
      { object: 'rule', action: 'rename', name: 'r', table, newName: 'r2' },
    ]);
    expect(buildPgRule(pgRuleFormFrom(original), { rule: original, parts: null }, table, 'shop')).toEqual([]);
  });

  it('checks SELECT rules', () => {
    const errors = pgRuleErrors({ ...pgRuleFormFrom(null), name: 'r', event: 'select', instead: false, condition: 'x', actions: '' });
    expect(Object.keys(errors).sort()).toEqual(['actions', 'condition', 'instead']);
    expect(pgRuleErrors({ ...pgRuleFormFrom(null), name: '_RETURN' }).name).toMatch(/view/);
    expect(pgRuleErrors({ ...pgRuleFormFrom(null), name: 'r', condition: 'new.a < 0', actions: 'NOTIFY x' }).actions).toMatch(/only SELECT/);
    expect(pgRuleErrors({ ...pgRuleFormFrom(null), name: 'r', actions: 'NOTIFY x' })).toEqual({});
  });

  it('drops with or without CASCADE', () => {
    expect(buildPgRuleDrop(table, 'r', true).change).toEqual({ object: 'rule', action: 'drop', name: 'r', table, behavior: 'cascade' });
  });
});

describe('policy form', () => {
  const policy: PgPolicy = {
    schema: 'app',
    table: 'orders',
    name: 'own_rows',
    command: 'select',
    permissive: true,
    roles: ['app_user'],
    using: '(owner = CURRENT_USER)',
    withCheck: null,
    comment: null,
  };

  it('creates a policy, turning row level security on only when chosen', () => {
    const base = pgPolicyFormFrom(null, { enabled: false, forced: false });
    // Enabling RLS changes who sees which rows: never on by itself.
    expect(base.rlsEnabled).toBe(false);
    expect(buildPgPolicy({ ...base, name: 'p' }, null, { enabled: false, forced: false }, table, 'shop')).toHaveLength(1);
    const form = { ...base, name: 'm2_ed_p', command: 'update' as const, roles: 'app_user, public', using: 'owner = current_user', withCheck: 'amount >= 0', rlsEnabled: true };
    expect(pgPolicyErrors(form)).toEqual({});
    expect(buildPgPolicy(form, null, { enabled: false, forced: false }, table, 'shop').map((c) => c.change)).toEqual([
      {
        object: 'policy',
        action: 'create',
        name: 'm2_ed_p',
        table,
        restrictive: false,
        command: 'update',
        roles: [{ kind: 'name', name: 'app_user' }, { kind: 'public' }],
        using: 'owner = current_user',
        withCheck: 'amount >= 0',
      },
      { object: 'table', action: 'alter', name: table, operations: [{ op: 'rowLevelSecurity', enabled: true }] },
    ]);
  });

  it('alters roles and USING, comments, then renames', () => {
    const rls = { enabled: true, forced: false };
    const form = { ...pgPolicyFormFrom(policy, rls), roles: '', using: 'owner = session_user', comment: 'mine', name: 'mine', rlsForced: true };
    expect(policyNeedsRecreate(form, policy)).toBeNull();
    expect(buildPgPolicy(form, policy, rls, table, 'shop').map((c) => c.change)).toEqual([
      { object: 'comment', target: { kind: 'policy', database: 'shop', schema: 'app', parent: 'orders', name: 'own_rows', signature: null }, comment: 'mine' },
      { object: 'policy', action: 'alter', name: 'own_rows', table, roles: [{ kind: 'public' }], using: 'owner = session_user', newName: 'mine' },
      { object: 'table', action: 'alter', name: table, operations: [{ op: 'forceRowLevelSecurity', forced: true }] },
    ]);
    expect(buildPgPolicy(pgPolicyFormFrom(policy, rls), policy, rls, table, 'shop')).toEqual([]);
  });

  it('replaces the policy in one change (one transaction) when the command or permissiveness changes or USING is removed', () => {
    const rls = { enabled: true, forced: false };
    const form = { ...pgPolicyFormFrom(policy, rls), permissive: false };
    expect(policyNeedsRecreate(form, policy)).toMatch(/one transaction/);
    // One change: the backend plans DROP and CREATE in one transaction, so a failing create keeps the old policy.
    expect(buildPgPolicy(form, policy, rls, table, null).map((c) => c.change)).toEqual([
      {
        object: 'policy',
        action: 'replace',
        name: 'own_rows',
        policy: { name: 'own_rows', table, restrictive: true, command: 'select', roles: [{ kind: 'name', name: 'app_user' }], using: '(owner = CURRENT_USER)', withCheck: null },
      },
    ]);
    expect(policyNeedsRecreate({ ...pgPolicyFormFrom(policy, rls), using: '' }, policy)).toMatch(/USING/);
  });

  it('checks which expressions a command takes', () => {
    expect(pgPolicyErrors({ ...pgPolicyFormFrom(null, null), name: 'p', command: 'insert', using: 'true' }).using).toMatch(/WITH CHECK/);
    expect(pgPolicyErrors({ ...pgPolicyFormFrom(null, null), name: 'p', command: 'delete', withCheck: 'true' }).withCheck).toMatch(/USING/);
    expect(pgPolicyErrors({ ...pgPolicyFormFrom(null, null), name: 'p', rlsEnabled: false, rlsForced: true }).rlsForced).toMatch(/enabled/);
  });

  it('leaves row level security alone when it could not be read', () => {
    const form = pgPolicyFormFrom(policy, null);
    expect(form.rlsEnabled).toBe(false);
    expect(buildPgPolicy({ ...form, comment: 'x' }, policy, null, table, null)).toHaveLength(1);
  });

  it('reads role lists', () => {
    expect(policyRoles('')).toEqual([{ kind: 'public' }]);
    expect(policyRoles('a, CURRENT_USER')).toEqual([{ kind: 'name', name: 'a' }, { kind: 'currentUser' }]);
    expect(buildPgPolicyDrop(table, 'p').change).toEqual({ object: 'policy', action: 'drop', name: 'p', table });
  });
});
