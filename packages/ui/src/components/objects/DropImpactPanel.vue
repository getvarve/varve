<script setup lang="ts">
import { computed, onMounted, ref, useId, watch } from 'vue';
import { catalogDropImpact } from '../../api/ddl';
import type { DropImpact, ObjectRef } from '../../api/ddl';
import { presentError, toCmdError } from '../../api/errors';
import type { SessionInfo } from '../../api/sessions';
import { latestOnly } from '../../lib/async';
import { appTabsNote, contentsText, dependentsNote, impactHeading, sessionsNote } from '../../lib/dropImpact';
import { appTabsOf } from './appTabs';
import Icon from '../common/Icon.vue';

/**
 * What dropping an object affects, shown in an editor's drop mode (the
 * `drop` slot of `ChangePanel.vue`): the objects that depend on it (what
 * CASCADE also drops on PostgreSQL), what it holds, and for a database the
 * other sessions connected to it and what makes the server refuse the
 * drop. Read on the metadata session; nothing runs.
 */
const props = withDefaults(
  defineProps<{
    session: SessionInfo;
    object: ObjectRef;
    /** CASCADE is chosen (PostgreSQL). */
    cascade?: boolean;
    /** WITH (FORCE) is chosen (PostgreSQL database drop). */
    force?: boolean;
    reportError?: ((error: unknown) => void) | null;
  }>(),
  { cascade: false, force: false, reportError: null },
);
const emit = defineEmits<{ loaded: [impact: DropImpact | null] }>();

const impact = ref<DropImpact | null>(null);
const loading = ref(false);
const error = ref<string | null>(null);
const latest = latestOnly();
const headingId = useId();
const engine = computed(() => (props.session.engine === 'redis' ? 'postgres' : props.session.engine));
const isDatabase = computed(() => props.object.kind === 'database');

async function load(): Promise<void> {
  const token = latest.next();
  loading.value = true;
  error.value = null;
  try {
    const found = await catalogDropImpact(props.session.id, props.object);
    if (!latest.isCurrent(token)) return;
    impact.value = found;
    emit('loaded', found);
  } catch (e: unknown) {
    props.reportError?.(e);
    if (!latest.isCurrent(token)) return;
    impact.value = null;
    const p = presentError(toCmdError(e));
    error.value = p.detail ? `${p.message} (${p.detail})` : p.message;
    emit('loaded', null);
  } finally {
    if (latest.isCurrent(token)) loading.value = false;
  }
}

const dependents = computed(() => (impact.value ? dependentsNote(engine.value, impact.value, props.cascade) : null));
const sessions = computed(() => (impact.value && isDatabase.value ? sessionsNote(engine.value, impact.value, props.force) : null));
const contents = computed(() => (impact.value ? contentsText(impact.value.contents) : ''));
const tabsNote = computed(() => (impact.value && isDatabase.value ? appTabsNote(engine.value, appTabsOf(impact.value), props.force) : null));

onMounted(() => void load());
watch(
  () => JSON.stringify([props.session.id, props.object]),
  () => void load(),
);
defineExpose({ load });
</script>

<template>
  <section
    class="impact"
    :aria-labelledby="headingId"
    aria-live="polite"
  >
    <header class="head">
      <h3 :id="headingId">
        {{ impactHeading(object.kind) }}
      </h3>
      <span class="grow" />
      <button
        type="button"
        class="btn compact"
        :disabled="loading"
        @click="load"
      >
        Check again
      </button>
    </header>
    <p
      v-if="loading && !impact"
      class="muted"
      role="status"
    >
      Looking up what depends on it
    </p>
    <p
      v-else-if="error"
      class="notice tone-warning"
      role="alert"
    >
      <Icon name="warning" />Could not look up what the drop affects: {{ error }} The drop can still run; the server refuses it when something depends on the object.
    </p>
    <template v-else-if="impact">
      <p
        v-if="contents"
        class="line"
      >
        Goes with it: {{ contents }}.
      </p>
      <ul
        v-if="impact.blockers.length > 0"
        class="list"
        aria-label="What makes the server refuse the drop"
      >
        <li
          v-for="b in impact.blockers"
          :key="b"
          class="notice tone-danger"
        >
          <Icon name="warning" />The server refuses the drop: {{ b }}.
        </li>
      </ul>
      <p
        v-for="note in impact.notes"
        :key="note"
        class="muted line"
      >
        {{ note }}
      </p>
      <p
        v-if="dependents"
        class="notice tone-warning"
      >
        <Icon name="warning" />{{ dependents }}
      </p>
      <ul
        v-if="impact.dependents.length > 0"
        class="deps mono"
        aria-label="Dependent objects"
      >
        <li
          v-for="d in impact.dependents"
          :key="d.description"
        >
          {{ d.description }}<span
            v-if="d.effect === 'breaks'"
            class="tag"
          >stops working</span><span
            v-else-if="d.effect === 'mayBreak'"
            class="tag"
          >may stop working</span>
        </li>
        <li
          v-if="impact.dependentsTruncated"
          class="muted"
        >
          and more
        </li>
      </ul>
      <p
        v-else-if="!isDatabase && !contents"
        class="muted line"
      >
        Nothing depends on it.
      </p>
      <template v-if="isDatabase">
        <p
          v-if="tabsNote"
          class="notice tone-warning"
        >
          <Icon name="warning" />{{ tabsNote }}
        </p>
        <p
          v-if="sessions"
          :class="['notice', impact.sessions.length > 0 && engine === 'postgres' && !force ? 'tone-warning' : 'tone-neutral']"
        >
          {{ sessions }}
        </p>
        <table
          v-if="impact.sessions.length > 0"
          class="sessions"
        >
          <caption class="sr-only">
            Sessions connected to {{ object.name }}
          </caption>
          <thead>
            <tr>
              <th scope="col">
                Id
              </th>
              <th scope="col">
                User
              </th>
              <th scope="col">
                Application
              </th>
              <th scope="col">
                Client
              </th>
              <th scope="col">
                State
              </th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="s in impact.sessions"
              :key="s.id"
            >
              <td class="mono">
                {{ s.id }}
              </td>
              <td>{{ s.user ?? '' }}</td>
              <td>{{ s.application ?? '' }}</td>
              <td class="mono">
                {{ s.client ?? '' }}
              </td>
              <td>{{ s.state ?? '' }}</td>
            </tr>
          </tbody>
        </table>
      </template>
    </template>
  </section>
</template>

<style scoped>
.impact {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 12px;
  border: 1px solid var(--border-inner);
  border-radius: var(--radius-panel);
  background: var(--surface);
  font-size: 13px;
}

.head {
  display: flex;
  align-items: center;
  gap: 8px;
}

h3 {
  margin: 0;
  font-size: 13px;
  font-weight: 600;
}

.grow {
  flex-grow: 1;
}

.line {
  margin: 0;
}

.list,
.deps {
  display: flex;
  flex-direction: column;
  gap: 4px;
  margin: 0;
  padding: 0;
  list-style: none;
}

.deps {
  max-height: 200px;
  overflow: auto;
  font-size: 12px;
}

.tag {
  margin-left: 8px;
  color: var(--text-muted);
  font-family: var(--font-sans, inherit);
}

.sessions {
  width: 100%;
  border-collapse: collapse;
  font-size: 12px;
}

.sessions th,
.sessions td {
  padding: 4px 6px;
  border-bottom: 1px solid var(--border-inner);
  text-align: left;
  overflow-wrap: anywhere;
}

.sessions th {
  color: var(--text-muted);
  font-weight: 500;
}

.sr-only {
  position: absolute;
  width: 1px;
  height: 1px;
  overflow: hidden;
  clip: rect(0 0 0 0);
  white-space: nowrap;
}
</style>
