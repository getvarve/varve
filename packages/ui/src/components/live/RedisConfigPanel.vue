<script setup lang="ts">
import { computed, ref, shallowRef } from 'vue';
import type { CmdError } from '../../api/errors';
import { redisConfig, redisConfigPlan } from '../../api/live';
import type { ConfigEntry } from '../../api/live';
import type { SessionInfo } from '../../api/sessions';
import { useToasts } from '../../composables/useToasts';
import { configNote, filterConfig } from '../../lib/live';
import EnvBadge from '../common/EnvBadge.vue';
import Icon from '../common/Icon.vue';
import LiveError from './LiveError.vue';
import RedisPlanPreview from './RedisPlanPreview.vue';
import { useReporter } from './report';
import { useRedisPlan } from './useRedisPlan';

/**
 * Redis configuration (`CONFIG GET *`): filter, change one parameter with
 * `CONFIG SET`, or write the running configuration to the file with
 * `CONFIG REWRITE`. Every change is planned on the server and previewed
 * (secret values masked) before Apply; destructive plans and, on
 * confirm-destructive connections, risky commands ask again. Secret
 * parameters never show or prefill their value. Changes are refused on
 * read-only connections.
 */
const props = defineProps<{
  /** The tab's Redis session. */
  session: SessionInfo;
  /** Connection name, for the confirmation dialog. */
  connectionName: string;
  /** The workspace tab the panel runs in; its errors are reported to the tab. */
  tabId?: string | null;
}>();
const emit = defineEmits<{ reconnect: [] }>();
const report = useReporter(() => props.tabId);
const toasts = useToasts();
const plans = useRedisPlan(
  () => props.session,
  () => props.connectionName,
  report,
);

const entries = shallowRef<ConfigEntry[] | null>(null);
const error = shallowRef<CmdError | null>(null);
const loading = ref(false);

async function load(): Promise<void> {
  loading.value = true;
  try {
    entries.value = await redisConfig(props.session.id);
    error.value = null;
  } catch (e: unknown) {
    error.value = report(e);
  } finally {
    loading.value = false;
  }
}
void load();

const text = ref('');
const riskyOnly = ref(false);
const visible = computed(() => filterConfig(entries.value ?? [], text.value, riskyOnly.value));
const readOnly = computed(() => props.session.readOnly);

// Editing one parameter

const editing = ref<string | null>(null);
const draft = ref('');

function canEdit(e: ConfigEntry): boolean {
  return !readOnly.value && !e.immutable;
}

function editLabel(e: ConfigEntry): string {
  if (readOnly.value) return `${e.name}: read-only connection`;
  if (e.immutable) return `${e.name} can only be set at startup`;
  return `Edit ${e.name}`;
}

function open(e: ConfigEntry): void {
  plans.discard();
  editing.value = e.name;
  // A secret is never shown or prefilled.
  draft.value = e.sensitive ? '' : (e.value ?? '');
}

function close(): void {
  editing.value = null;
  plans.discard();
}

function touched(): void {
  plans.discard();
}

async function previewSet(e: ConfigEntry): Promise<void> {
  await plans.plan(`Set ${e.name}`, () => redisConfigPlan(props.session.id, { kind: 'set', changes: [{ name: e.name, value: draft.value }] }));
}

async function previewRewrite(): Promise<void> {
  editing.value = null;
  await plans.plan('Write the running configuration to the configuration file', () => redisConfigPlan(props.session.id, { kind: 'rewrite' }));
}

async function apply(): Promise<void> {
  const title = plans.shown.value?.title ?? '';
  const replies = await plans.apply();
  if (replies === null) return;
  toasts.push('success', title, `${replies.length} ${replies.length === 1 ? 'command' : 'commands'} ran.`);
  editing.value = null;
  await load();
}
</script>

<template>
  <div class="panel">
    <div class="head">
      <h1>Configuration</h1>
      <span class="source">CONFIG GET *</span>
      <EnvBadge :environment="session.environment" />
      <span class="grow" />
      <button
        type="button"
        class="btn compact"
        :disabled="readOnly || plans.busy.value"
        :title="readOnly ? 'Read-only connection' : 'CONFIG REWRITE'"
        @click="previewRewrite"
      >
        <Icon name="save" />Rewrite config file
      </button>
      <button
        type="button"
        class="icon-btn"
        aria-label="Reload configuration"
        title="Reload configuration"
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
          :placeholder="`Search ${entries?.length ?? ''} parameters`"
          aria-label="Search parameters"
        >
      </label>
      <label class="check"><input
        v-model="riskyOnly"
        type="checkbox"
      >Risky, protected or startup-only</label>
      <span class="grow" />
      <span class="count">{{ visible.length }} of {{ entries?.length ?? 0 }}</span>
    </div>
    <p
      v-if="readOnly"
      class="note scope-note"
    >
      This connection is read-only: the configuration can be viewed, not changed.
    </p>

    <LiveError
      v-if="error"
      :error="error"
      class="panel-error"
      @reconnect="emit('reconnect')"
    />

    <div
      v-if="plans.shown.value && editing === null"
      class="top-preview"
    >
      <RedisPlanPreview
        :shown="plans.shown.value"
        :busy="plans.busy.value"
        @apply="apply"
        @discard="plans.discard"
      />
    </div>
    <p
      v-if="plans.error.value && editing === null"
      class="problem top-preview"
      role="alert"
    >
      {{ plans.error.value }}
      <button
        v-if="plans.canReplan.value"
        type="button"
        class="btn compact"
        :disabled="plans.busy.value"
        @click="plans.replan"
      >
        Plan again
      </button>
    </p>

    <div class="scroll">
      <table
        class="grid"
        aria-label="Configuration parameters"
      >
        <thead>
          <tr>
            <th>Parameter</th><th>Value</th><th>Notes</th><th><span class="visually-hidden">Edit</span></th>
          </tr>
        </thead>
        <tbody>
          <template
            v-for="e in visible"
            :key="e.name"
          >
            <tr :class="{ selected: editing === e.name }">
              <td class="mono">
                {{ e.name }}
              </td>
              <td
                class="mono wrap"
                :title="e.sensitive ? 'Secret: not shown' : (e.value ?? '')"
              >
                <span
                  v-if="e.sensitive"
                  class="subtle"
                >{{ e.hasValue ? '(set, hidden)' : '(not set)' }}</span>
                <template v-else>
                  {{ e.value ?? '' }}
                </template>
              </td>
              <td>
                <span
                  v-for="n in configNote(e)"
                  :key="n.label"
                  class="pill tiny"
                  :class="n.tone"
                  :title="n.help"
                >{{ n.label }}</span>
              </td>
              <td class="num">
                <button
                  type="button"
                  class="icon-btn"
                  :aria-label="editLabel(e)"
                  :title="editLabel(e)"
                  :disabled="!canEdit(e)"
                  @click="editing === e.name ? close() : open(e)"
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
              v-if="editing === e.name"
              class="edit-row"
            >
              <td colspan="4">
                <form
                  class="edit"
                  @submit.prevent="plans.shown.value ? apply() : previewSet(e)"
                >
                  <div class="edit-line">
                    <label
                      class="visually-hidden"
                      :for="`cfg-${e.name}`"
                    >New value of {{ e.name }}</label>
                    <input
                      :id="`cfg-${e.name}`"
                      v-model="draft"
                      class="input value-input mono"
                      :type="e.sensitive ? 'password' : 'text'"
                      autocomplete="off"
                      spellcheck="false"
                      :placeholder="e.sensitive ? 'Enter a new value' : ''"
                      @input="touched"
                    >
                    <span class="note">CONFIG SET applies now and is lost at restart unless you rewrite the config file.</span>
                  </div>
                  <p
                    v-if="e.protected"
                    class="notice tone-warning"
                  >
                    <Icon name="warning" />Protected parameter: the server accepts a change only when it runs with enable-protected-configs.
                  </p>
                  <RedisPlanPreview
                    v-if="plans.shown.value"
                    :shown="plans.shown.value"
                    :busy="plans.busy.value"
                    @apply="apply"
                    @discard="plans.discard"
                  />
                  <p
                    v-if="plans.error.value"
                    class="problem"
                    role="alert"
                  >
                    {{ plans.error.value }}
                    <button
                      v-if="plans.canReplan.value"
                      type="button"
                      class="btn compact"
                      :disabled="plans.busy.value"
                      @click="plans.replan"
                    >
                      Plan again
                    </button>
                  </p>
                  <div
                    v-if="!plans.shown.value"
                    class="edit-actions"
                  >
                    <span class="grow" />
                    <button
                      type="button"
                      class="btn compact"
                      @click="close"
                    >
                      Cancel
                    </button>
                    <button
                      type="submit"
                      class="btn compact primary"
                      :disabled="plans.busy.value"
                    >
                      Preview
                    </button>
                  </div>
                </form>
              </td>
            </tr>
          </template>
        </tbody>
      </table>
      <div
        v-if="entries && visible.length === 0"
        class="empty"
      >
        No parameters match the filter.
      </div>
      <div
        v-else-if="!entries && !error"
        class="empty"
      >
        Loading…
      </div>
    </div>
  </div>
</template>

<style scoped src="./panel.css"></style>

<style scoped>
.panel-error,
.top-preview {
  margin: 8px 16px 0;
}

.scope-note {
  margin: 6px 16px 0;
}

.tiny {
  height: 18px;
  margin-right: 4px;
  padding: 0 6px;
  font-size: 11px;
}

.subtle {
  color: var(--text-subtle);
}

.edit-row td {
  height: auto;
  padding: 0;
  white-space: normal;
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
  width: 320px;
  height: 28px;
  font-size: 12px;
}

.edit-actions {
  display: flex;
  gap: 8px;
}

.problem {
  margin: 0;
  color: var(--danger-text);
  font-size: 12px;
}

.note {
  margin: 0;
}
</style>
