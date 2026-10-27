<script setup lang="ts">
import { computed, ref, shallowRef } from 'vue';
import type { CmdError } from '../../api/errors';
import { redisFunctionPlan, redisFunctions } from '../../api/live';
import type { FlushMode, FunctionLibrary } from '../../api/live';
import type { SessionInfo } from '../../api/sessions';
import { useToasts } from '../../composables/useToasts';
import { latestOnly } from '../../lib/async';
import { libraryNameOf } from '../../lib/live';
import EnvBadge from '../common/EnvBadge.vue';
import Icon from '../common/Icon.vue';
import LiveError from './LiveError.vue';
import RedisPlanPreview from './RedisPlanPreview.vue';
import { useReporter } from './report';
import { useRedisPlan } from './useRedisPlan';

/**
 * Redis functions (`FUNCTION LIST`, Redis 7.0 and newer, Valkey): the
 * libraries with their functions and code, loading a library
 * (`FUNCTION LOAD`, optionally `REPLACE`), deleting one (`FUNCTION DELETE`)
 * and flushing the Lua script cache (`SCRIPT FLUSH`). Every change is a
 * plan previewed before Apply and confirmed when the server asks; all are
 * refused on read-only connections. Redis 6.2 has no functions: the list
 * says so and the script cache flush stays available.
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
const readOnly = computed(() => props.session.readOnly);

const libraries = shallowRef<FunctionLibrary[] | null>(null);
const error = shallowRef<CmdError | null>(null);
const loading = ref(false);

async function load(): Promise<void> {
  loading.value = true;
  try {
    libraries.value = await redisFunctions(props.session.id, null, false);
    error.value = null;
  } catch (e: unknown) {
    error.value = report(e);
    libraries.value = null;
  } finally {
    loading.value = false;
  }
}
void load();

const unsupported = computed(() => {
  const e = error.value;
  if (!e) return false;
  if (e.kind === 'catalog' || e.kind === 'live') return e.error.kind === 'unsupported';
  return e.kind === 'session' && e.error.kind === 'driver' && e.error.error.kind === 'unsupported';
});

// Selected library and its code

const selected = ref<string | null>(null);
const code = ref<string | null>(null);
const codeError = shallowRef<CmdError | null>(null);
const codeRequests = latestOnly();
const selectedLibrary = computed(() => libraries.value?.find((l) => l.name === selected.value) ?? null);

async function select(name: string): Promise<void> {
  selected.value = name;
  code.value = null;
  codeError.value = null;
  const token = codeRequests.next();
  try {
    // The pattern is the exact name; glob characters in it are escaped.
    const pattern = name.replace(/[*?[\]\\]/g, (c) => `\\${c}`);
    const found = await redisFunctions(props.session.id, pattern, true);
    if (!codeRequests.isCurrent(token)) return;
    code.value = found.find((l) => l.name === name)?.code ?? '';
  } catch (e: unknown) {
    if (codeRequests.isCurrent(token)) codeError.value = report(e);
  }
}

// Changes

type Mode = 'none' | 'load';
const mode = ref<Mode>('none');
const source = ref('#!lua name=mylib\n\nredis.register_function(\'hello\', function(keys, args)\n  return \'hello\'\nend)\n');
const replace = ref(false);
const flushMode = ref<FlushMode>('default');

const sourceName = computed(() => libraryNameOf(source.value));
const exists = computed(() => !!sourceName.value && !!libraries.value?.some((l) => l.name === sourceName.value));
const loadProblem = computed(() => {
  if (source.value.trim() === '') return 'Enter the library code.';
  if (!sourceName.value) return 'The first line must name the library: #!lua name=<library>.';
  if (exists.value && !replace.value) return `A library named ${sourceName.value} exists: tick Replace to load over it.`;
  return '';
});

function startLoad(fromSelected: boolean): void {
  plans.discard();
  mode.value = 'load';
  if (fromSelected && selectedLibrary.value && code.value) {
    source.value = code.value;
    replace.value = true;
  }
}

async function previewLoad(): Promise<void> {
  if (loadProblem.value) return;
  await plans.plan(`Load library ${sourceName.value ?? ''}${replace.value ? ' (replace)' : ''}`, () =>
    redisFunctionPlan(props.session.id, { kind: 'load', code: source.value, replace: replace.value }),
  );
}

async function previewDelete(name: string): Promise<void> {
  mode.value = 'none';
  await plans.plan(`Delete library ${name}`, () => redisFunctionPlan(props.session.id, { kind: 'delete', library: name }));
}

async function previewFlush(): Promise<void> {
  mode.value = 'none';
  await plans.plan('Flush the Lua script cache', () => redisFunctionPlan(props.session.id, { kind: 'scriptFlush', mode: flushMode.value }));
}

async function apply(): Promise<void> {
  const title = plans.shown.value?.title ?? '';
  const replies = await plans.apply();
  if (replies === null) return;
  toasts.push('success', title, 'Done.');
  mode.value = 'none';
  await load();
  if (selected.value && libraries.value?.some((l) => l.name === selected.value)) await select(selected.value);
  else selected.value = null;
}
</script>

<template>
  <div class="panel">
    <div class="head">
      <h1>Functions</h1>
      <span class="source">FUNCTION LIST</span>
      <EnvBadge :environment="session.environment" />
      <span class="grow" />
      <button
        type="button"
        class="btn compact primary"
        :disabled="readOnly || unsupported"
        :title="readOnly ? 'Read-only connection' : ''"
        @click="startLoad(false)"
      >
        <Icon name="plus" />Load library
      </button>
      <label class="check">Script cache
        <select
          v-model="flushMode"
          class="select small-select"
          aria-label="Flush mode"
        >
          <option value="default">server default</option>
          <option value="async">ASYNC</option>
          <option value="sync">SYNC</option>
        </select>
      </label>
      <button
        type="button"
        class="btn compact"
        :disabled="readOnly || plans.busy.value"
        @click="previewFlush"
      >
        Flush script cache
      </button>
      <button
        type="button"
        class="icon-btn"
        aria-label="Reload functions"
        title="Reload functions"
        :disabled="loading"
        @click="load"
      >
        <Icon name="refresh" />
      </button>
    </div>

    <p
      v-if="unsupported"
      class="note scope-note"
    >
      This server has no functions (they arrived in Redis 7.0). The Lua script cache can still be flushed.
    </p>
    <LiveError
      v-else-if="error"
      :error="error"
      class="panel-error"
      @reconnect="emit('reconnect')"
    />

    <div
      v-if="mode === 'load'"
      class="box load"
    >
      <div class="section-head">
        <h2>Load a library</h2>
        <span class="note">The first line names the engine and the library: <span class="mono">#!lua name=mylib</span></span>
      </div>
      <label
        class="visually-hidden"
        for="fn-code"
      >Library code</label>
      <textarea
        id="fn-code"
        v-model="source"
        class="textarea code-input"
        rows="10"
        spellcheck="false"
        @input="plans.discard"
      />
      <div class="edit-line">
        <label class="check"><input
          v-model="replace"
          type="checkbox"
          @change="plans.discard"
        >Replace a library of the same name</label>
        <span
          v-if="loadProblem"
          class="problem"
        >{{ loadProblem }}</span>
        <span class="grow" />
        <button
          type="button"
          class="btn compact"
          @click="mode = 'none'; plans.discard()"
        >
          Cancel
        </button>
        <button
          v-if="!plans.shown.value"
          type="button"
          class="btn compact primary"
          :disabled="!!loadProblem || plans.busy.value"
          @click="previewLoad"
        >
          Preview
        </button>
      </div>
    </div>

    <div
      v-if="plans.shown.value"
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
      v-if="plans.error.value"
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

    <div class="split">
      <div class="scroll list">
        <table
          class="grid"
          aria-label="Function libraries"
        >
          <thead>
            <tr>
              <th>Library</th><th>Engine</th><th>Functions</th><th><span class="visually-hidden">Actions</span></th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="l in libraries ?? []"
              :key="l.name"
              class="selectable"
              :class="{ selected: selected === l.name }"
              tabindex="0"
              :aria-selected="selected === l.name"
              @click="select(l.name)"
              @keydown.enter.prevent="select(l.name)"
            >
              <td class="mono">
                {{ l.name }}
              </td>
              <td class="mono">
                {{ l.engine }}
              </td>
              <td
                class="mono wrap"
                :title="l.functions.map((f) => f.name).join(', ')"
              >
                {{ l.functions.map((f) => f.name).join(', ') }}
              </td>
              <td class="num">
                <button
                  type="button"
                  class="btn compact"
                  :disabled="readOnly || plans.busy.value"
                  :aria-label="`Delete library ${l.name}`"
                  @click.stop="previewDelete(l.name)"
                >
                  Delete
                </button>
              </td>
            </tr>
          </tbody>
        </table>
        <div
          v-if="libraries && libraries.length === 0"
          class="empty"
        >
          No function libraries are loaded.
        </div>
      </div>
      <aside
        v-if="selectedLibrary"
        class="detail"
        aria-label="Library details"
      >
        <div class="section-head">
          <h2 class="mono">
            {{ selectedLibrary.name }}
          </h2>
          <span class="grow" />
          <button
            type="button"
            class="btn compact"
            :disabled="readOnly || code === null"
            @click="startLoad(true)"
          >
            Edit and reload
          </button>
        </div>
        <dl class="facts">
          <template
            v-for="f in selectedLibrary.functions"
            :key="f.name"
          >
            <dt class="mono">
              {{ f.name }}
            </dt>
            <dd>{{ [f.description, f.flags.length > 0 ? `flags: ${f.flags.join(', ')}` : ''].filter(Boolean).join(' · ') || 'no flags' }}</dd>
          </template>
        </dl>
        <LiveError
          v-if="codeError"
          :error="codeError"
          @reconnect="emit('reconnect')"
        />
        <pre
          v-else
          class="code source-code"
          aria-label="Library code"
        >{{ code ?? 'Loading code…' }}</pre>
      </aside>
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
  margin: 8px 16px 0;
}

.box {
  display: flex;
  flex-direction: column;
  gap: 8px;
  margin: 8px 16px 0;
  padding: 10px 12px;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
  background: var(--surface);
}

.box .section-head {
  padding: 0;
}

.code-input {
  min-height: 160px;
}

.edit-line {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 10px;
}

.split {
  display: flex;
  flex: 1;
  min-height: 0;
  margin-top: 8px;
  border-top: 1px solid var(--border-inner);
}

.list {
  flex: 1;
  min-width: 0;
}

.detail {
  display: flex;
  flex-direction: column;
  gap: 10px;
  width: 420px;
  flex-shrink: 0;
  padding: 0 16px 12px;
  border-left: 1px solid var(--border);
  overflow: auto;
}

.detail .section-head {
  padding: 10px 0 0;
}

.source-code {
  white-space: pre;
  overflow: auto;
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
