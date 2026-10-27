<script setup lang="ts">
import { computed, onMounted, ref, useId } from 'vue';
import { catalogList } from '../../api/catalog';
import type { ObjectSummary } from '../../api/catalog';
import type { PgFunction } from '../../api/ddl';
import { buildPgRoutine, createsOverload, errorsOf, identitySignature, objectRef, pgRoutineFormFrom, pgRoutineSchema } from '../../lib/ddlPlan';
import type { PgRoutineForm } from '../../lib/ddlPlan';
import FormField from '../common/FormField.vue';
import Icon from '../common/Icon.vue';
import ChangePanel from './ChangePanel.vue';
import DdlView from './DdlView.vue';
import EditorFrame from './EditorFrame.vue';
import IdentityFields from './IdentityFields.vue';
import SqlCode from './SqlCode.vue';
import { displayName } from './types';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';
import { useObjectEditor } from './useObjectEditor';

/** PostgreSQL function and procedure editor (design canvas FunctionEditor). Use `RoutineEditor.vue`. */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const kind = computed<'function' | 'procedure'>(() => {
  const k = props.object?.kind ?? props.createKind;
  return k === 'procedure' ? 'procedure' : 'function';
});
const schemaDefault = computed(() => props.container?.schema ?? 'public');
const database = computed(() => props.object?.database ?? props.container?.database ?? props.session.database);
const overloads = ref<ObjectSummary[]>([]);

const editor = useObjectEditor<PgFunction, PgRoutineForm>({
  props,
  emit,
  pick: (d) =>
    d.engine === 'postgres' && (d.object.kind === 'function' || d.object.kind === 'procedure') ? d.object : 'This object is not a PostgreSQL function or procedure.',
  afterLoad: async (fn) => {
    overloads.value = [];
    if (!fn) return;
    const list = await catalogList(props.session.id, { database: database.value, schema: fn.schema, parent: null }, kind.value);
    overloads.value = list.filter((o) => o.object.name === fn.name);
  },
  formFrom: (fn) => pgRoutineFormFrom(fn, kind.value, schemaDefault.value),
  validate: (form) => errorsOf(pgRoutineSchema, form),
  build: (form, fn) => buildPgRoutine(form, fn, database.value).map((change) => ({ kind: 'edit', change })),
  identity: (form, fn) =>
    objectRef(kind.value, database.value, form.schema.trim(), form.name.trim(), {
      signature: fn && !createsOverload(form, fn) ? fn.signature : identitySignature(form.args),
    }),
  resolveIdentity: async (guess, form, fn) => {
    if (fn && !createsOverload(form, fn)) return guess;
    // A new routine or overload: the server prints its signature its own way; find the one that did not exist before.
    const before = new Set(fn && fn.schema === guess.schema && fn.name === guess.name ? overloads.value.map((o) => o.object.signature) : []);
    const list = await catalogList(props.session.id, { database: database.value, schema: guess.schema, parent: null }, kind.value);
    const same = list.filter((o) => o.object.name === guess.name);
    const fresh = same.filter((o) => !before.has(o.object.signature));
    const match = fresh.length === 1 ? fresh[0] : same.length === 1 ? same[0] : null;
    return match ? match.object : guess;
  },
});
const { form, errors, details } = editor;

const isFunction = computed(() => kind.value === 'function');
const overload = computed(() => (details.value ? createsOverload(form, details.value) : false));
const tab = ref('definition');
const tabs = computed(() => [{ id: 'definition', label: 'Definition' }, ...(editor.current.value ? [{ id: 'ddl', label: 'DDL' }] : [])]);
const title = computed(() => (editor.current.value ? displayName(editor.current.value, 'postgres') : `New ${kind.value}`));
const blocked = computed(() => Object.values(errors.value)[0] ?? null);
const ids = {
  returns: useId(),
  language: useId(),
  parallel: useId(),
  cost: useId(),
  rows: useId(),
  bodyKind: useId(),
  file: useId(),
  settings: useId(),
};
const bodyHeader = computed(() => {
  switch (form.bodyKind) {
    case 'definition':
      return 'AS $function$ ... $function$';
    case 'return':
      return 'RETURN expression';
    case 'atomic':
      return 'BEGIN ATOMIC ... END (one statement per ;)';
    case 'object':
      return 'AS library file, link symbol';
  }
  return '';
});

function addArg(): void {
  form.args.push({ mode: 'in', name: '', dataType: '', default: '' });
}

function addColumn(): void {
  form.returnColumns.push({ name: '', dataType: '' });
}

onMounted(() => void editor.load());
</script>

<template>
  <EditorFrame
    v-model:active="tab"
    :title="title"
    :kind-label="isFunction ? 'Function' : 'Procedure'"
    :environment="session.environment"
    :subtitle="details ? `owner ${details.meta.owner ?? 'none'}` : ''"
    :tabs="tabs"
    :read-only="session.readOnly"
    :dirty-text="editor.dirty.value ? 'Unsaved changes' : ''"
    :loading="editor.loading.value"
    :load-error="editor.loadError.value"
    @reload="editor.load"
  >
    <template #badges>
      <span
        v-if="overloads.length > 1"
        class="subtitle hint"
      >{{ overloads.length }} overloads</span>
      <span
        v-if="details?.meta.extension"
        class="pill tone-neutral"
      >extension {{ details.meta.extension }}</span>
    </template>
    <template #actions>
      <button
        type="button"
        class="btn compact"
        :disabled="!editor.dirty.value"
        @click="editor.revert"
      >
        Revert
      </button>
      <button
        v-if="editor.current.value"
        type="button"
        class="btn compact"
        :disabled="session.readOnly || editor.mode.value === 'drop'"
        @click="editor.startDrop"
      >
        Drop
      </button>
    </template>

    <template #definition>
      <div class="layout">
        <div class="column">
          <section
            class="section"
            aria-label="Signature and options"
          >
            <IdentityFields
              v-model:name="form.name"
              v-model:schema="form.schema"
              v-model:owner="form.owner"
              v-model:comment="form.comment"
              :session="session"
              :errors="errors"
            />
            <table class="table">
              <caption class="visually-hidden">
                Arguments
              </caption>
              <thead>
                <tr>
                  <th>Mode</th>
                  <th>Argument name</th>
                  <th>Type</th>
                  <th>Default</th>
                  <th><span class="visually-hidden">Remove</span></th>
                </tr>
              </thead>
              <tbody>
                <tr
                  v-for="(arg, i) in form.args"
                  :key="i"
                >
                  <td>
                    <select
                      v-model="arg.mode"
                      class="select"
                      :aria-label="`Mode of argument ${i + 1}`"
                    >
                      <option value="in">
                        IN
                      </option>
                      <option value="out">
                        OUT
                      </option>
                      <option value="inOut">
                        INOUT
                      </option>
                      <option value="variadic">
                        VARIADIC
                      </option>
                    </select>
                  </td>
                  <td>
                    <input
                      v-model="arg.name"
                      class="input mono"
                      :aria-label="`Name of argument ${i + 1}`"
                      spellcheck="false"
                    >
                  </td>
                  <td>
                    <input
                      v-model="arg.dataType"
                      class="input mono"
                      :aria-label="`Type of argument ${i + 1}`"
                      spellcheck="false"
                      :aria-invalid="!!errors[`args.${i}.dataType`]"
                    >
                    <span
                      v-if="errors[`args.${i}.dataType`]"
                      class="field-error"
                    >{{ errors[`args.${i}.dataType`] }}</span>
                  </td>
                  <td>
                    <input
                      v-model="arg.default"
                      class="input mono"
                      :aria-label="`Default of argument ${i + 1}`"
                      placeholder="no default"
                      spellcheck="false"
                      :aria-invalid="!!errors[`args.${i}.default`]"
                    >
                    <span
                      v-if="errors[`args.${i}.default`]"
                      class="field-error"
                    >{{ errors[`args.${i}.default`] }}</span>
                  </td>
                  <td>
                    <button
                      type="button"
                      class="icon-btn"
                      :aria-label="`Remove argument ${arg.name || i + 1}`"
                      @click="form.args.splice(i, 1)"
                    >
                      <Icon name="close" />
                    </button>
                  </td>
                </tr>
              </tbody>
            </table>
            <div>
              <button
                type="button"
                class="btn compact"
                @click="addArg"
              >
                <Icon name="plus" />Add argument
              </button>
            </div>
            <p
              v-if="overload"
              class="notice tone-warning"
              role="note"
            >
              <Icon name="warning" />Changing argument types creates a new overload instead of replacing this one. The current one stays until you drop it.
            </p>
            <div
              v-if="isFunction"
              class="grid"
            >
              <FormField
                label="Returns"
                :input-id="ids.returns"
                :error="errors.returnType ?? ''"
              >
                <div class="returns">
                  <select
                    v-model="form.returnsKind"
                    class="select"
                    aria-label="Kind of result"
                  >
                    <option value="type">
                      A type
                    </option>
                    <option value="table">
                      TABLE (...)
                    </option>
                  </select>
                  <input
                    v-if="form.returnsKind === 'type'"
                    :id="ids.returns"
                    v-model="form.returnType"
                    class="input mono"
                    spellcheck="false"
                    :aria-invalid="!!errors.returnType"
                  >
                </div>
              </FormField>
              <label
                v-if="form.returnsKind === 'type'"
                class="check"
              >
                <input
                  v-model="form.setof"
                  type="checkbox"
                ><span>Returns SETOF</span>
              </label>
            </div>
            <div
              v-if="isFunction && form.returnsKind === 'table'"
              class="wide"
            >
              <table class="table">
                <caption class="visually-hidden">
                  Result columns
                </caption>
                <thead>
                  <tr>
                    <th>Column</th>
                    <th>Type</th>
                    <th><span class="visually-hidden">Remove</span></th>
                  </tr>
                </thead>
                <tbody>
                  <tr
                    v-for="(col, i) in form.returnColumns"
                    :key="i"
                  >
                    <td>
                      <input
                        v-model="col.name"
                        class="input mono"
                        :aria-label="`Name of result column ${i + 1}`"
                      >
                    </td>
                    <td>
                      <input
                        v-model="col.dataType"
                        class="input mono"
                        :aria-label="`Type of result column ${i + 1}`"
                      >
                    </td>
                    <td>
                      <button
                        type="button"
                        class="icon-btn"
                        :aria-label="`Remove result column ${col.name || i + 1}`"
                        @click="form.returnColumns.splice(i, 1)"
                      >
                        <Icon name="close" />
                      </button>
                    </td>
                  </tr>
                </tbody>
              </table>
              <span
                v-if="errors.returnColumns"
                class="field-error"
              >{{ errors.returnColumns }}</span>
              <button
                type="button"
                class="btn compact"
                @click="addColumn"
              >
                <Icon name="plus" />Add column
              </button>
            </div>
            <div class="grid">
              <FormField
                label="Language"
                :input-id="ids.language"
                required
                :error="errors.language ?? ''"
              >
                <input
                  :id="ids.language"
                  v-model="form.language"
                  class="input mono"
                  list="pg-routine-languages"
                  spellcheck="false"
                >
                <datalist id="pg-routine-languages">
                  <option value="plpgsql" />
                  <option value="sql" />
                  <option value="c" />
                  <option value="internal" />
                </datalist>
              </FormField>
              <template v-if="isFunction">
                <FormField
                  label="Parallel"
                  :input-id="ids.parallel"
                >
                  <select
                    :id="ids.parallel"
                    v-model="form.parallel"
                    class="select"
                  >
                    <option value="safe">
                      SAFE
                    </option>
                    <option value="restricted">
                      RESTRICTED
                    </option>
                    <option value="unsafe">
                      UNSAFE
                    </option>
                  </select>
                </FormField>
                <FormField
                  label="Cost"
                  :input-id="ids.cost"
                  :error="errors.cost ?? ''"
                >
                  <input
                    :id="ids.cost"
                    v-model="form.cost"
                    class="input mono"
                    inputmode="decimal"
                  >
                </FormField>
                <FormField
                  v-if="form.setof || form.returnsKind === 'table'"
                  label="Rows"
                  :input-id="ids.rows"
                  :error="errors.rows ?? ''"
                >
                  <input
                    :id="ids.rows"
                    v-model="form.rows"
                    class="input mono"
                    inputmode="decimal"
                  >
                </FormField>
              </template>
            </div>
            <div
              v-if="isFunction"
              role="radiogroup"
              aria-label="Volatility"
              class="segmented"
            >
              <button
                v-for="v in (['immutable', 'stable', 'volatile'] as const)"
                :key="v"
                type="button"
                role="radio"
                :aria-checked="form.volatility === v"
                @click="form.volatility = v"
              >
                {{ v.toUpperCase() }}
              </button>
            </div>
            <div class="checks">
              <label
                v-if="isFunction"
                class="check"
              >
                <input
                  v-model="form.strict"
                  type="checkbox"
                ><span>Strict (NULL in, NULL out)</span>
              </label>
              <label class="check">
                <input
                  v-model="form.securityDefiner"
                  type="checkbox"
                ><span>Security definer</span>
              </label>
              <label
                v-if="isFunction"
                class="check"
              >
                <input
                  v-model="form.leakproof"
                  type="checkbox"
                ><span>Leakproof</span>
              </label>
              <label
                v-if="isFunction"
                class="check"
              >
                <input
                  v-model="form.window"
                  type="checkbox"
                ><span>Window function</span>
              </label>
            </div>
            <FormField
              label="Settings"
              :input-id="ids.settings"
              :error="errors.settings ?? ''"
              help="One per line, for example search_path = public, pg_temp"
            >
              <textarea
                :id="ids.settings"
                v-model="form.settings"
                class="textarea"
                rows="2"
                spellcheck="false"
              />
            </FormField>
          </section>
          <section
            class="section"
            aria-label="Body"
          >
            <div class="section-head">
              <h2>Body</h2>
              <span class="hint mono">{{ bodyHeader }}</span>
              <span class="grow" />
              <select
                :id="ids.bodyKind"
                v-model="form.bodyKind"
                class="select body-kind"
                aria-label="Kind of body"
              >
                <option value="definition">
                  Source text
                </option>
                <option value="return">
                  SQL standard RETURN
                </option>
                <option value="atomic">
                  SQL standard BEGIN ATOMIC
                </option>
                <option value="object">
                  Library object
                </option>
              </select>
            </div>
            <FormField
              v-if="form.bodyKind === 'object'"
              label="Library file"
              :input-id="ids.file"
              :error="errors.file ?? ''"
            >
              <input
                :id="ids.file"
                v-model="form.file"
                class="input mono"
              >
            </FormField>
            <div class="code-box tall">
              <SqlCode
                v-model="form.body"
                engine="postgres"
                :label="form.bodyKind === 'object' ? 'Link symbol' : 'Body'"
              />
            </div>
            <span
              v-if="errors.body"
              class="field-error"
              role="alert"
            >{{ errors.body }}</span>
          </section>
        </div>
        <div class="column">
          <ChangePanel
            v-model:cascade="editor.cascade.value"
            :plan="editor.plan"
            engine="postgres"
            :read-only="session.readOnly"
            :mode="editor.mode.value"
            :creating="editor.creating.value"
            :object-name="title"
            :blocked="editor.dirty.value ? blocked : null"
            @applied="editor.onApplied"
            @cancel-drop="editor.cancelDrop"
            @reconnect="emit('reconnect')"
            @refresh="editor.load"
          />
          <section
            v-if="overloads.length > 0"
            class="section"
            aria-label="Overloads"
          >
            <h2>Overloads</h2>
            <ul class="list">
              <li
                v-for="o in overloads"
                :key="o.object.signature ?? ''"
              >
                <button
                  type="button"
                  class="list-item"
                  :aria-current="o.object.signature === editor.current.value?.signature"
                  @click="emit('openObject', o.object)"
                >
                  <span class="mono">{{ o.object.name }}({{ o.object.signature }})</span>
                  <span class="grow" />
                  <span class="hint">{{ o.detail ?? '' }}</span>
                </button>
              </li>
            </ul>
          </section>
        </div>
      </div>
    </template>

    <template #ddl>
      <DdlView
        v-if="editor.current.value"
        :session="session"
        :connection-name="connectionName"
        :object="editor.current.value"
        :report-error="reportError"
        @open-sql="emit('openSql', $event)"
        @changed="editor.load(); emit('changed')"
        @reconnect="emit('reconnect')"
      />
    </template>
  </EditorFrame>
</template>

<style scoped src="./editor.css"></style>
<style scoped>
.returns {
  display: flex;
  gap: 8px;
}

.checks {
  display: flex;
  flex-wrap: wrap;
  gap: 8px 16px;
}

.body-kind {
  width: auto;
  height: 28px;
}
</style>
