/**
 * Form models of the PostgreSQL rule and row level security policy editors
 * and the typed changes they send to `edit_plan`
 * (crates/catalog/src/postgres/edit/rule.rs, plus the table's
 * `rowLevelSecurity` / `forceRowLevelSecurity` operations). The backend
 * writes and quotes the SQL; conditions, commands and expressions are SQL
 * the user writes, checked by the backend for their shape.
 */
import type {
  EngineChange,
  FiringState,
  PgCreatePolicy,
  PgPolicy,
  PgPolicyCommand,
  PgRlsOperation,
  PgRule,
  PgRuleEvent,
  QualifiedName,
  RoleSpec,
} from '../api/ddl';
import { objectRef, roleSpec, textOrNull } from './ddlPlan';
import type { FieldErrors } from './ddlPlan';
import { pgNameProblem } from './ddlDatabase';
import { splitStatements } from './ddlText';

function pg(change: Extract<EngineChange, { engine: 'postgres' }>['change']): EngineChange {
  return { engine: 'postgres', change };
}

function same(a: string | null | undefined, b: string | null | undefined): boolean {
  return (a ?? '').trim() === (b ?? '').trim();
}

// ---------------------------------------------------------------------------
// Reading pg_get_ruledef
// ---------------------------------------------------------------------------

/** A top-level word of PostgreSQL text and where it starts and ends. */
interface Word {
  upper: string;
  from: number;
  to: number;
}

/**
 * The top-level words of PostgreSQL text (outside brackets, strings,
 * quoted names, dollar quotes and comments), so `DO` in a string or a
 * subquery is never mistaken for the rule's `DO`.
 */
function topLevelWords(text: string): Word[] {
  const words: Word[] = [];
  let depth = 0;
  let i = 0;
  const n = text.length;
  while (i < n) {
    const c = text[i] as string;
    if (c === "'" || c === '"') {
      i += 1;
      while (i < n) {
        if (text[i] === c) {
          if (text[i + 1] === c) i += 2;
          else break;
        } else i += 1;
      }
      i += 1;
    } else if (c === '-' && text[i + 1] === '-') {
      while (i < n && text[i] !== '\n') i += 1;
    } else if (c === '/' && text[i + 1] === '*') {
      const end = text.indexOf('*/', i + 2);
      i = end < 0 ? n : end + 2;
    } else if (c === '$') {
      const tag = /^\$([A-Za-z_][A-Za-z_0-9]*)?\$/.exec(text.slice(i));
      if (tag) {
        const end = text.indexOf(tag[0], i + tag[0].length);
        i = end < 0 ? n : end + tag[0].length;
      } else i += 1;
    } else if (c === '(' || c === '[') {
      depth += 1;
      i += 1;
    } else if (c === ')' || c === ']') {
      depth = Math.max(0, depth - 1);
      i += 1;
    } else if (/[A-Za-z_]/.test(c)) {
      const from = i;
      while (i < n && /[A-Za-z0-9_$]/.test(text[i] as string)) i += 1;
      if (depth === 0) words.push({ upper: text.slice(from, i).toUpperCase(), from, to: i });
    } else i += 1;
  }
  return words;
}

/** Removes one pair of brackets around the whole text. */
export function unwrapParens(text: string): string {
  const t = text.trim();
  if (!t.startsWith('(') || !t.endsWith(')')) return t;
  const inner = t.slice(1, -1);
  // Only when the opening bracket closes at the very end: `(a) AND (b)` stays.
  return topLevelWords(`x${t}`).length === 1 && balanced(inner) ? inner.trim() : t;
}

function balanced(text: string): boolean {
  let depth = 0;
  let quote: string | null = null;
  for (let i = 0; i < text.length; i += 1) {
    const c = text[i];
    if (quote) {
      if (c === quote) quote = null;
    } else if (c === "'" || c === '"') quote = c;
    else if (c === '(') depth += 1;
    else if (c === ')') {
      depth -= 1;
      if (depth < 0) return false;
    }
  }
  return depth === 0;
}

/** The parts of a rule that `CREATE OR REPLACE RULE` sets. */
export interface RuleParts {
  condition: string;
  instead: boolean;
  /** Commands without `;`; empty means `NOTHING`. */
  actions: string[];
}

/**
 * Reads the condition and commands out of `pg_get_ruledef`
 * (`CREATE RULE r AS ON INSERT TO s.t WHERE (...) DO INSTEAD (...);`).
 * `null` when the text does not have that shape.
 */
export function parseRuleDefinition(definition: string): RuleParts | null {
  const text = definition.trim().replace(/;\s*$/, '');
  const words = topLevelWords(text);
  const on = words.findIndex((w) => w.upper === 'ON');
  const to = words.findIndex((w, i) => i > on && w.upper === 'TO');
  if (on < 0 || to < 0) return null;
  const doAt = words.findIndex((w, i) => i > to && w.upper === 'DO');
  if (doAt < 0) return null;
  const where = words.findIndex((w, i) => i > to && i < doAt && w.upper === 'WHERE');
  const doWord = words[doAt] as Word;
  const condition = where >= 0 ? unwrapParens(text.slice((words[where] as Word).to, doWord.from)) : '';
  let rest = text.slice(doWord.to).trim();
  let instead = false;
  const mode = /^(INSTEAD|ALSO)\b/i.exec(rest);
  if (mode) {
    instead = (mode[1] as string).toUpperCase() === 'INSTEAD';
    rest = rest.slice(mode[0].length).trim();
  }
  if (/^NOTHING$/i.test(rest)) return { condition, instead, actions: [] };
  const body = rest.startsWith('(') && rest.endsWith(')') && balanced(rest.slice(1, -1)) ? rest.slice(1, -1) : rest;
  const actions = splitStatements(body, 'postgres')
    .map((s) => s.text.trim().replace(/;\s*$/, '').trim())
    .filter((s) => s !== '');
  return { condition, instead, actions };
}

// ---------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------

export type RuleStateForm = 'enabled' | 'always' | 'replica' | 'disabled';

export interface PgRuleForm {
  name: string;
  event: PgRuleEvent;
  condition: string;
  instead: boolean;
  /** Commands separated by `;`; blank means `NOTHING`. */
  actions: string;
  state: RuleStateForm;
  comment: string;
}

const RULE_EVENTS: readonly PgRuleEvent[] = ['select', 'insert', 'update', 'delete'];
const STATE_FROM_ENABLED: Record<string, RuleStateForm> = { origin: 'enabled', always: 'always', replica: 'replica', disabled: 'disabled' };

export function pgRuleFormFrom(rule: PgRule | null): PgRuleForm {
  const parts = rule ? parseRuleDefinition(rule.definition) : null;
  const event = rule && (RULE_EVENTS as readonly string[]).includes(rule.event) ? (rule.event as PgRuleEvent) : 'insert';
  return {
    name: rule?.name ?? '',
    event,
    condition: parts?.condition ?? '',
    instead: parts?.instead ?? rule?.instead ?? true,
    actions: parts ? parts.actions.join(';\n') : '',
    state: rule ? (STATE_FROM_ENABLED[rule.enabled] ?? 'enabled') : 'enabled',
    comment: rule?.comment ?? '',
  };
}

/** The commands of the actions field. */
export function ruleActions(text: string): string[] {
  return splitStatements(text, 'postgres')
    .map((s) => s.text.trim().replace(/;\s*$/, '').trim())
    .filter((s) => s !== '');
}

export function pgRuleErrors(form: PgRuleForm): FieldErrors {
  const out: FieldErrors = {};
  const name = pgNameProblem(form.name);
  if (name) out.name = name;
  else if (form.name.trim() === '_RETURN') out.name = '_RETURN is the rule that makes a view; edit the view instead.';
  const actions = ruleActions(form.actions);
  if (form.condition.trim() !== '' && actions.some((a) => !/^(SELECT|INSERT|UPDATE|DELETE|WITH)\b/i.test(a))) {
    out.actions = 'A rule with a condition takes only SELECT, INSERT, UPDATE or DELETE commands.';
  }
  if (form.event === 'select') {
    if (!form.instead) out.instead = 'ON SELECT rules must be DO INSTEAD.';
    if (actions.length !== 1) out.actions = 'An ON SELECT rule has exactly one SELECT command.';
    if (form.condition.trim() !== '') out.condition = 'ON SELECT rules have no condition.';
  }
  return out;
}

/** The rule as the editor last read it (`null` for a new one). */
export interface RuleOriginal {
  rule: PgRule;
  parts: RuleParts | null;
}

function ruleRef(table: QualifiedName, name: string, database: string | null) {
  return objectRef('rule', database, table.schema, name, { parent: table.name });
}

const FIRING: Record<RuleStateForm, FiringState> = { enabled: 'enabled', always: 'always', replica: 'replica', disabled: 'disabled' };

/**
 * Create (and its state when not enabled), or edit: `CREATE OR REPLACE` when
 * the event, condition or commands changed, the state, the comment, then
 * the rename.
 */
export function buildPgRule(form: PgRuleForm, original: RuleOriginal | null, table: QualifiedName, database: string | null): EngineChange[] {
  const name = form.name.trim();
  const definition = (orReplace: boolean, ruleName: string, comment: string | null): EngineChange =>
    pg({
      object: 'rule',
      action: 'create',
      name: ruleName,
      table,
      orReplace,
      event: form.event,
      condition: textOrNull(form.condition),
      instead: form.instead,
      actions: ruleActions(form.actions),
      ...(comment !== null ? { comment } : {}),
    });
  if (!original) {
    const out = [definition(false, name, textOrNull(form.comment))];
    if (form.state !== 'enabled') out.push(pg({ object: 'rule', action: 'setState', name, table, state: FIRING[form.state] }));
    return out;
  }
  const before = pgRuleFormFrom(original.rule);
  const out: EngineChange[] = [];
  const redefined =
    form.event !== before.event ||
    form.instead !== before.instead ||
    !same(form.condition, before.condition) ||
    ruleActions(form.actions).join('\n') !== ruleActions(before.actions).join('\n');
  if (redefined) out.push(definition(true, original.rule.name, null));
  if (form.state !== before.state) out.push(pg({ object: 'rule', action: 'setState', name: original.rule.name, table, state: FIRING[form.state] }));
  if (!same(form.comment, original.rule.comment)) {
    out.push(pg({ object: 'comment', target: ruleRef(table, original.rule.name, database), comment: textOrNull(form.comment) }));
  }
  if (name !== original.rule.name) out.push(pg({ object: 'rule', action: 'rename', name: original.rule.name, table, newName: name }));
  return out;
}

/** `DROP RULE`, with CASCADE when asked. */
export function buildPgRuleDrop(table: QualifiedName, name: string, cascade: boolean): EngineChange {
  return pg({ object: 'rule', action: 'drop', name, table, behavior: cascade ? 'cascade' : 'restrict' });
}

// ---------------------------------------------------------------------------
// Policies
// ---------------------------------------------------------------------------

export interface PgPolicyForm {
  name: string;
  command: PgPolicyCommand;
  /** True for `PERMISSIVE` (combined with OR), false for `RESTRICTIVE` (AND). */
  permissive: boolean;
  /** Comma separated; blank means `PUBLIC`. */
  roles: string;
  using: string;
  withCheck: string;
  comment: string;
  /** Row level security of the table (`ENABLE` / `FORCE`). */
  rlsEnabled: boolean;
  rlsForced: boolean;
}

/** Row level security of the policy's table, read from the table's details. */
export interface TableRls {
  enabled: boolean;
  forced: boolean;
}

const POLICY_COMMANDS: readonly PgPolicyCommand[] = ['all', 'select', 'insert', 'update', 'delete'];

export function pgPolicyFormFrom(policy: PgPolicy | null, rls: TableRls | null): PgPolicyForm {
  const command = policy && (POLICY_COMMANDS as readonly string[]).includes(policy.command) ? (policy.command as PgPolicyCommand) : 'all';
  const roles = policy ? policy.roles.filter((r) => r.toLowerCase() !== 'public') : [];
  return {
    name: policy?.name ?? '',
    command,
    permissive: policy?.permissive ?? true,
    roles: roles.join(', '),
    using: policy?.using ?? '',
    withCheck: policy?.withCheck ?? '',
    comment: policy?.comment ?? '',
    // Enabling row level security changes who sees which rows: it is never switched on by itself.
    rlsEnabled: rls?.enabled ?? false,
    rlsForced: rls?.forced ?? false,
  };
}

/** The role list of the form; `PUBLIC` when blank. */
export function policyRoles(text: string): RoleSpec[] {
  const names = text
    .split(',')
    .map((r) => r.trim())
    .filter((r) => r !== '');
  if (names.length === 0) return [{ kind: 'public' }];
  return names.map((r) => (r.toUpperCase() === 'PUBLIC' ? { kind: 'public' } : roleSpec(r)));
}

function rolesKey(text: string): string {
  return policyRoles(text)
    .map((r) => (r.kind === 'name' ? `n:${r.name}` : r.kind))
    .sort()
    .join(',');
}

export function pgPolicyErrors(form: PgPolicyForm): FieldErrors {
  const out: FieldErrors = {};
  const name = pgNameProblem(form.name);
  if (name) out.name = name;
  if (form.command === 'insert' && form.using.trim() !== '') out.using = 'An INSERT policy has only WITH CHECK.';
  if ((form.command === 'select' || form.command === 'delete') && form.withCheck.trim() !== '') {
    out.withCheck = 'SELECT and DELETE policies have only USING.';
  }
  if (form.rlsForced && !form.rlsEnabled) out.rlsForced = 'FORCE only matters while row level security is enabled.';
  for (const role of form.roles.split(',')) {
    if (role.trim() === '' && form.roles.trim() !== '') out.roles = 'Separate role names with single commas.';
  }
  return out;
}

/** Why the change drops and creates the policy again (in one transaction), or `null` (`ALTER POLICY` can do it). */
export function policyNeedsRecreate(form: PgPolicyForm, original: PgPolicy): string | null {
  const again = 'dropped and created again in one transaction (if the create fails, the old policy stays).';
  if (form.command !== original.command) return `The command of a policy cannot change: it is ${again}`;
  if (form.permissive !== original.permissive) return `A policy cannot switch between permissive and restrictive: it is ${again}`;
  if (original.using !== null && form.using.trim() === '') return `ALTER POLICY cannot remove USING: the policy is ${again}`;
  if (original.withCheck !== null && form.withCheck.trim() === '') return `ALTER POLICY cannot remove WITH CHECK: the policy is ${again}`;
  return null;
}

function rlsChanges(form: PgPolicyForm, rls: TableRls | null, table: QualifiedName): EngineChange[] {
  const ops: PgRlsOperation[] = [];
  if (form.rlsEnabled !== (rls?.enabled ?? false)) ops.push({ op: 'rowLevelSecurity', enabled: form.rlsEnabled });
  if (form.rlsForced !== (rls?.forced ?? false)) ops.push({ op: 'forceRowLevelSecurity', forced: form.rlsForced });
  return ops.length > 0 ? [pg({ object: 'table', action: 'alter', name: table, operations: ops })] : [];
}

function policyDefinition(form: PgPolicyForm, table: QualifiedName): PgCreatePolicy {
  const comment = textOrNull(form.comment);
  return {
    name: form.name.trim(),
    table,
    restrictive: !form.permissive,
    command: form.command,
    roles: form.roles.trim() === '' ? [] : policyRoles(form.roles),
    using: textOrNull(form.using),
    withCheck: textOrNull(form.withCheck),
    ...(comment !== null ? { comment } : {}),
  };
}

function createPolicy(form: PgPolicyForm, table: QualifiedName): EngineChange {
  return pg({ object: 'policy', action: 'create', ...policyDefinition(form, table) });
}

/**
 * Create, or edit with `ALTER POLICY` (roles, USING, WITH CHECK, then the
 * rename; the comment before it), or drop and create when the command or
 * permissiveness changes or an expression is removed. Row level security
 * of the table changes last.
 */
export function buildPgPolicy(
  form: PgPolicyForm,
  original: PgPolicy | null,
  rls: TableRls | null,
  table: QualifiedName,
  database: string | null,
): EngineChange[] {
  if (!original) return [createPolicy(form, table), ...rlsChanges(form, rls, table)];
  const before = pgPolicyFormFrom(original, rls);
  const out: EngineChange[] = [];
  if (policyNeedsRecreate(form, original) !== null) {
    // One change, one transaction: if the new definition fails, the old policy stays.
    return [pg({ object: 'policy', action: 'replace', name: original.name, policy: policyDefinition(form, table) }), ...rlsChanges(form, rls, table)];
  }
  const alter: Extract<Extract<EngineChange, { engine: 'postgres' }>['change'], { object: 'policy'; action: 'alter' }> = {
    object: 'policy',
    action: 'alter',
    name: original.name,
    table,
  };
  if (rolesKey(form.roles) !== rolesKey(before.roles)) alter.roles = policyRoles(form.roles);
  if (!same(form.using, before.using)) alter.using = form.using.trim();
  if (!same(form.withCheck, before.withCheck)) alter.withCheck = form.withCheck.trim();
  const renamed = form.name.trim() !== original.name;
  if (!same(form.comment, original.comment)) {
    out.push(
      pg({
        object: 'comment',
        target: objectRef('policy', database, table.schema, original.name, { parent: table.name }),
        comment: textOrNull(form.comment),
      }),
    );
  }
  if (renamed) alter.newName = form.name.trim();
  if (alter.roles !== undefined || alter.using !== undefined || alter.withCheck !== undefined || renamed) out.push(pg(alter));
  return [...out, ...rlsChanges(form, rls, table)];
}

/** `DROP POLICY` (policies have no dependents: no CASCADE). */
export function buildPgPolicyDrop(table: QualifiedName, name: string): EngineChange {
  return pg({ object: 'policy', action: 'drop', name, table });
}
