<script setup lang="ts">
import { computed, ref, shallowRef } from 'vue';
import { presentError } from '../../api/errors';
import type { CmdError } from '../../api/errors';
import { galeraStatus, serverVariables, variableChangeApply, variableChangePlan } from '../../api/live';
import type { EditPlan, EditScope, GaleraStatus, ServerVariable, VariableChange } from '../../api/live';
import type { SessionInfo } from '../../api/sessions';
import { useDialogs } from '../../composables/useDialogs';
import { useToasts } from '../../composables/useToasts';
import { runWithConfirmation } from '../../lib/destructive';
import { changeKind, checkVariableValue, filterVariables, isChanged, scopeLabel, scopedValue, variableCategory } from '../../lib/live';
import EnvBadge from '../common/EnvBadge.vue';
import Icon from '../common/Icon.vue';
import GaleraSummary from './GaleraSummary.vue';
import LiveError from './LiveError.vue';
import { useReporter } from './report';

/**
 * Server variables (design: ServerVariables): search, category, "changed
 * from default", "dynamic only", and changes in the scopes the server allows
 * (`settableScopes`). A change is planned on the server, previewed with its
 * exact statements and warnings, and runs only after Apply (plus the
 * destructive confirmation when the server asks). MariaDB shows its Galera
 * status beside the list.
 */
const props = defineProps<{
  session: SessionInfo;
  /** Connection name, for the destructive confirmation dialog. */
  connectionName: string;
  /** The workspace tab the panel runs in; its errors are reported to the tab. */
  tabId?: string | null;
}>();
const emit = defineEmits<{ reconnect: [] }>();
const report = useReporter(() => props.tabId);

const toasts = useToasts();
const dialogs = useDialogs();
const engine = computed(() => props.session.engine);
const isPg = computed(() => engine.value === 'postgres');

const vars = shallowRef<ServerVariable[] | null>(null);
const error = shallowRef<CmdError | null>(null);
const loading = ref(false);
const galera = shallowRef<GaleraStatus | null>(null);

async function load(): Promise<void> {
  loading.value = true;
  try {
    vars.value = await serverVariables(props.session.id);
    error.value = null;
  } catch (e: unknown) {
    error.value = report(e);
  } finally {
    loading.value = false;
  }
  if (engine.value === 'mariadb') {
    try {
      galera.value = await galeraStatus(props.session.id);
    } catch {
      // Galera is optional context; the variables are the panel. A refusal leaves the side panel out.
      galera.value = null;
    }
  }
}
void load();

// Filters

const text = ref('');
const category = ref('');
const changedOnly = ref(false);
const dynamicOnly = ref(false);
/** MySQL and MariaDB report both values; PostgreSQL has one. */
const valueScope = ref<'global' | 'session'>('global');

const categories = computed(() => [...new Set((vars.value ?? []).map(variableCategory))].sort((a, b) => a.localeCompare(b)));
const visible = computed(() =>
  filterVariables(vars.value ?? [], { text: text.value, category: category.value, changedOnly: changedOnly.value, dynamicOnly: dynamicOnly.value }),
);

function shown(v: ServerVariable): string {
  const value = isPg.value ? v.value : scopedValue(v, valueScope.value);
  if (value === null) return '';
  return v.unit ? `${value} ${v.unit}` : value;
}

// Editing

interface Editor {
  name: string;
  action: 'set' | 'reset';
  scope: EditScope;
  value: string;
  /** Planned on the server: the preview. */
  planned: { planId: string; plan: EditPlan; change: VariableChange } | null;
  error: string;
  busy: boolean;
}

const editor = ref<Editor | null>(null);
const editing = computed(() => (editor.value ? (vars.value ?? []).find((v) => v.name === editor.value?.name) ?? null : null));

function canEdit(v: ServerVariable): boolean {
  return !props.session.readOnly && !v.readOnly && v.settableScopes.length > 0;
}

function editLabel(v: ServerVariable): string {
  if (props.session.readOnly) return `${v.name}: read-only connection`;
  if (!canEdit(v)) return `${v.name} cannot be changed`;
  return `Edit ${v.name}`;
}

function openEditor(v: ServerVariable): void {
  const scope: EditScope = v.settableScopes.includes('global') ? 'global' : (v.settableScopes[0] ?? 'session');
  // A masked value holds a secret: never prefill it.
  let current = v.masked ? '' : ((scope === 'session' ? v.sessionValue : v.globalValue) ?? v.value ?? '');
  if (v.varType === 'boolean') current = ['on', 'true', 'yes', '1'].includes(current.trim().toLowerCase()) ? 'on' : 'off';
  editor.value = { name: v.name, action: 'set', scope, value: current, planned: null, error: '', busy: false };
}

function closeEditor(): void {
  editor.value = null;
}

/** Any change to the form drops the preview: Apply always runs what is shown. */
function touched(): void {
  if (editor.value) {
    editor.value.planned = null;
    editor.value.error = '';
  }
}

const valueProblem = computed(() => {
  const e = editor.value;
  const v = editing.value;
  if (!e || !v || e.action === 'reset') return null;
  return checkVariableValue(v, e.value);
});

async function preview(): Promise<void> {
  const e = editor.value;
  if (!e || e.busy || valueProblem.value) return;
  const change: VariableChange = e.action === 'set' ? { action: 'set', scope: e.scope, value: e.value } : { action: 'reset', scope: e.scope };
  e.busy = true;
  e.error = '';
  try {
    const planned = await variableChangePlan(props.session.id, e.name, change);
    if (editor.value === e) e.planned = { ...planned, change };
  } catch (err: unknown) {
    e.error = presentError(report(err)).message;
  } finally {
    e.busy = false;
  }
}

async function apply(): Promise<void> {
  const e = editor.value;
  if (!e?.planned || e.busy) return;
  const planId = e.planned.planId;
  e.busy = true;
  e.error = '';
  try {
    const done = await runWithConfirmation(
      async (confirmed) => {
        await variableChangeApply(props.session.id, planId, confirmed);
        return true;
      },
      (request) => dialogs.askDestructive({ connectionName: props.connectionName, environment: request.environment, statements: request.statements }),
    );
    if (done === null) return;
    toasts.push('success', `${e.name} changed`, e.planned.plan.statements.join(';\n'));
    editor.value = null;
    await load();
  } catch (err: unknown) {
    // The server takes the plan when it starts running it, so after any
    // failure the preview is spent: drop it, and Preview plans again.
    const p = presentError(report(err));
    e.planned = null;
    e.error = `${p.detail ? `${p.message} (${p.detail})` : p.message} Preview again to retry.`;
  } finally {
    e.busy = false;
  }
}

const applyLabel = computed(() => {
  const e = editor.value;
  if (!e) return 'Apply';
  if (e.action === 'reset') return `Apply reset (${scopeLabel(e.scope, engine.value)})`;
  return `Apply ${scopeLabel(e.scope, engine.value)}`;
});

const scopeNote = computed(() => {
  const e = editor.value;
  if (!e) return '';
  switch (e.scope) {
    case 'session':
      return "Changes this tab's own session only; other connections keep their value.";
    case 'global':
      return isPg.value
        ? 'Writes postgresql.auto.conf and reloads the configuration. Variables that need a restart take effect after the next restart.'
        : engine.value === 'mariadb'
          ? 'Changes the running server for new connections; lost at restart (MariaDB has no SET PERSIST). Galera does not replicate SET GLOBAL.'
          : 'Changes the running server for new connections; lost at restart unless persisted.';
    case 'persist':
      return 'Changes the running server and writes mysqld-auto.cnf, so it survives a restart.';
    case 'persistOnly':
      return 'Writes mysqld-auto.cnf only; the value takes effect at the next restart.';
  }
  return '';
});

function inputId(name: string): string {
  return `sv-edit-${name.replace(/[^\w-]/g, '_')}`;
}
</script>

<template>
  <div class="layout">
    <div class="panel main">
      <div class="head">
        <h1>Server variables</h1>
        <div
          v-if="!isPg"
          class="segmented"
          role="tablist"
          aria-label="Value scope"
        >
          <button
            type="button"
            role="tab"
            :aria-selected="valueScope === 'global'"
            :aria-checked="valueScope === 'global'"
            @click="valueScope = 'global'"
          >
            Global
          </button>
          <button
            type="button"
            role="tab"
            :aria-selected="valueScope === 'session'"
            :aria-checked="valueScope === 'session'"
            @click="valueScope = 'session'"
          >
            Session
          </button>
        </div>
        <EnvBadge :environment="session.environment" />
        <span class="source">{{ session.server.version }}</span>
        <span class="grow" />
        <button
          type="button"
          class="icon-btn"
          aria-label="Reload variables"
          title="Reload variables"
          :disabled="loading"
          @click="load"
        >
          <Icon name="refresh" />
        </button>
      </div>

      <div class="bar">
        <label class="search">
          <Icon name="search" />
          <input
            v-model="text"
            :placeholder="`Search ${vars?.length ?? ''} variables`"
            aria-label="Search variables"
          >
        </label>
        <label class="check">Category
          <select
            v-model="category"
            class="select small-select"
          >
            <option value="">All</option>
            <option
              v-for="c in categories"
              :key="c"
              :value="c"
            >{{ c }}</option>
          </select>
        </label>
        <label class="check"><input
          v-model="changedOnly"
          type="checkbox"
        >Changed from default</label>
        <label class="check"><input
          v-model="dynamicOnly"
          type="checkbox"
        >Dynamic only</label>
        <span class="grow" />
        <span class="count">{{ visible.length }} of {{ vars?.length ?? 0 }}</span>
      </div>
      <p
        v-if="!isPg && valueScope === 'session'"
        class="note scope-note"
      >
        Session values are read on a side connection, so they show the server defaults, not changes made in this tab's session.
      </p>

      <LiveError
        v-if="error"
        :error="error"
        class="panel-error"
        @reconnect="emit('reconnect')"
      />

      <div class="scroll">
        <table
          class="grid"
          aria-label="Server variables"
        >
          <thead>
            <tr>
              <th>Variable</th><th>Value</th><th>Default</th><th>Change</th><th><span class="visually-hidden">Edit</span></th>
            </tr>
          </thead>
          <tbody>
            <template
              v-for="v in visible"
              :key="v.name"
            >
              <tr :class="{ changed: isChanged(v), selected: editor?.name === v.name }">
                <td
                  class="mono"
                  :title="v.description ?? ''"
                >
                  {{ v.name }}
                </td>
                <td
                  class="mono wrap"
                  :title="v.masked ? 'Hidden: holds a secret' : shown(v)"
                >
                  {{ shown(v) }}<span
                    v-if="v.pendingRestart"
                    class="pill tone-warning tiny"
                  >restart pending</span>
                </td>
                <td class="mono wrap subtle">
                  {{ v.defaultValue ?? '' }}
                </td>
                <td>
                  <span
                    class="pill tiny"
                    :class="changeKind(v) === 'Dynamic' ? 'tone-success' : changeKind(v) === 'Read only' || changeKind(v) === 'Not settable' ? 'tone-neutral' : 'tone-warning'"
                  >{{ changeKind(v) }}</span>
                </td>
                <td class="num">
                  <button
                    type="button"
                    class="icon-btn"
                    :aria-label="editLabel(v)"
                    :title="editLabel(v)"
                    :disabled="!canEdit(v)"
                    @click="editor?.name === v.name ? closeEditor() : openEditor(v)"
                  >
                    <svg
                      width="14"
                      height="14"
                      viewBox="0 0 24 24"
                      fill="none"
                      stroke="currentColor"
                      stroke-width="2"
                      stroke-linecap="round"
                      stroke-linejoin="round"
                      aria-hidden="true"
                      focusable="false"
                    ><path d="M4 20h4L19 9l-4-4L4 16zM14 6l4 4" /></svg>
                  </button>
                </td>
              </tr>
              <tr
                v-if="editor && editor.name === v.name"
                class="edit-row"
              >
                <td colspan="5">
                  <form
                    class="edit"
                    @submit.prevent="editor.planned ? apply() : preview()"
                  >
                    <div class="edit-line">
                      <div
                        class="segmented"
                        role="radiogroup"
                        aria-label="Change"
                      >
                        <button
                          type="button"
                          role="radio"
                          :aria-checked="editor.action === 'set'"
                          @click="editor.action = 'set'; touched()"
                        >
                          Set value
                        </button>
                        <button
                          type="button"
                          role="radio"
                          :aria-checked="editor.action === 'reset'"
                          @click="editor.action = 'reset'; touched()"
                        >
                          Reset to default
                        </button>
                      </div>
                      <label class="check">Scope
                        <select
                          v-model="editor.scope"
                          class="select small-select"
                          @change="touched"
                        >
                          <option
                            v-for="s in v.settableScopes"
                            :key="s"
                            :value="s"
                          >{{ scopeLabel(s, engine) }}</option>
                        </select>
                      </label>
                      <template v-if="editor.action === 'set'">
                        <label
                          class="visually-hidden"
                          :for="inputId(v.name)"
                        >New value of {{ v.name }}</label>
                        <select
                          v-if="v.varType === 'enum' && v.enumValues.length > 0"
                          :id="inputId(v.name)"
                          v-model="editor.value"
                          class="select value-input mono"
                          @change="touched"
                        >
                          <option
                            v-for="o in v.enumValues"
                            :key="o"
                            :value="o"
                          >
                            {{ o }}
                          </option>
                        </select>
                        <select
                          v-else-if="v.varType === 'boolean'"
                          :id="inputId(v.name)"
                          v-model="editor.value"
                          class="select value-input mono"
                          @change="touched"
                        >
                          <option value="on">
                            on
                          </option>
                          <option value="off">
                            off
                          </option>
                        </select>
                        <input
                          v-else
                          :id="inputId(v.name)"
                          v-model="editor.value"
                          class="input value-input mono"
                          autocomplete="off"
                          spellcheck="false"
                          :type="v.masked ? 'password' : 'text'"
                          :aria-invalid="!!valueProblem"
                          :placeholder="v.masked ? 'Enter a new value' : ''"
                          @input="touched"
                        >
                        <span class="note">
                          <template v-if="v.unit">unit {{ v.unit }} · </template>
                          <template v-if="v.minValue !== null || v.maxValue !== null">range {{ v.minValue ?? '' }} to {{ v.maxValue ?? '' }} · </template>
                          <template v-if="v.varType === 'set' && v.enumValues.length > 0">allowed {{ v.enumValues.join(', ') }}</template>
                          <template v-if="v.typeInferred"> · type inferred</template>
                        </span>
                      </template>
                    </div>
                    <p
                      v-if="valueProblem"
                      class="problem"
                      role="alert"
                    >
                      {{ valueProblem }}
                    </p>
                    <p class="note">
                      {{ scopeNote }}
                    </p>
                    <div
                      v-if="editor.planned"
                      class="preview"
                    >
                      <span class="label">Preview</span>
                      <pre class="code">{{ editor.planned.plan.statements.join(';\n') }};</pre>
                      <ul
                        v-if="editor.planned.plan.warnings.length > 0"
                        class="warnings"
                      >
                        <li
                          v-for="(w, i) in editor.planned.plan.warnings"
                          :key="i"
                          class="notice tone-warning"
                        >
                          <Icon name="warning" />{{ w }}
                        </li>
                      </ul>
                    </div>
                    <p
                      v-if="editor.error"
                      class="problem"
                      role="alert"
                    >
                      {{ editor.error }}
                    </p>
                    <div class="edit-actions">
                      <span class="grow" />
                      <button
                        type="button"
                        class="btn compact"
                        @click="closeEditor"
                      >
                        Cancel
                      </button>
                      <button
                        v-if="!editor.planned"
                        type="submit"
                        class="btn compact primary"
                        :disabled="editor.busy || !!valueProblem"
                      >
                        Preview
                      </button>
                      <button
                        v-else
                        type="submit"
                        class="btn compact"
                        :class="editor.planned.plan.destructive ? 'danger' : 'primary'"
                        :disabled="editor.busy"
                      >
                        {{ applyLabel }}
                      </button>
                    </div>
                  </form>
                </td>
              </tr>
            </template>
          </tbody>
        </table>
        <div
          v-if="vars && visible.length === 0"
          class="empty"
        >
          No variables match the filters.
        </div>
        <div
          v-else-if="!vars && !error"
          class="empty"
        >
          Loading…
        </div>
      </div>
    </div>
    <aside
      v-if="galera"
      class="side"
      aria-label="Galera cluster status"
    >
      <h2>Galera cluster</h2>
      <GaleraSummary :galera="galera" />
    </aside>
  </div>
</template>

<style scoped src="./panel.css"></style>

<style scoped>
.layout {
  display: flex;
  height: 100%;
  min-height: 0;
}

.main {
  flex: 1;
  min-width: 0;
}

.side {
  display: flex;
  flex-direction: column;
  gap: 10px;
  width: 320px;
  flex-shrink: 0;
  padding: 12px 16px;
  border-left: 1px solid var(--border);
  background: var(--surface);
  overflow: auto;
}

.side h2 {
  margin: 0;
  font-size: 13px;
  font-weight: 600;
}

.panel-error {
  margin: 8px 16px 0;
}

.scope-note {
  margin: 6px 16px 0;
}

.grid tbody tr.changed td:first-child {
  box-shadow: inset 2px 0 0 var(--accent);
}

.tiny {
  height: 18px;
  margin-left: 6px;
  padding: 0 6px;
  font-size: 11px;
}

.edit-row td {
  height: auto;
  padding: 0;
  white-space: normal;
  background: var(--surface);
}

.edit {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 10px 12px 12px;
  border-bottom: 1px solid var(--border);
  background: var(--accent-tint);
}

.edit-line {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 10px;
}

.value-input {
  width: 260px;
  height: 28px;
  font-size: 12px;
}

.preview {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.warnings {
  display: flex;
  flex-direction: column;
  gap: 6px;
  margin: 0;
  padding: 0;
  list-style: none;
}

.problem {
  margin: 0;
  color: var(--danger-text);
  font-size: 12px;
}

.edit-actions {
  display: flex;
  gap: 8px;
}

.note {
  margin: 0;
}

.subtle {
  color: var(--text-subtle);
}
</style>
