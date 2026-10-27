<script setup lang="ts">
import { computed, onMounted, ref, useId, watch } from 'vue';
import { catalogList } from '../../api/catalog';
import type { ObjectKind, ObjectRef, ObjectSummary } from '../../api/catalog';
import type { SessionInfo } from '../../api/sessions';
import Icon from '../common/Icon.vue';
import type { NewObjectContainer } from './types';

/** The enums, domains, composite and range types of a PostgreSQL schema (design canvas TypeEditor, left column). */
const props = defineProps<{
  session: SessionInfo;
  database: string | null;
  schema: string;
  current: ObjectRef | null;
  /** Bumped by the editor after a change so the list reloads. */
  revision: number;
  reportError: ((error: unknown) => void) | null;
}>();
const emit = defineEmits<{ openObject: [object: ObjectRef]; createObject: [kind: ObjectKind, container: NewObjectContainer] }>();

const GROUPS: { kind: ObjectKind; label: string }[] = [
  { kind: 'enumType', label: 'Enums' },
  { kind: 'domain', label: 'Domains' },
  { kind: 'compositeType', label: 'Composites' },
  { kind: 'rangeType', label: 'Ranges' },
];
const items = ref<Partial<Record<ObjectKind, ObjectSummary[]>>>({});
const filter = ref('');
const failed = ref(false);
const filterId = useId();
const newKind = ref<ObjectKind>('enumType');

async function load(): Promise<void> {
  failed.value = false;
  const next: Partial<Record<ObjectKind, ObjectSummary[]>> = {};
  try {
    for (const g of GROUPS) {
      next[g.kind] = await catalogList(props.session.id, { database: props.database, schema: props.schema, parent: null }, g.kind);
    }
    items.value = next;
  } catch (error: unknown) {
    failed.value = true;
    props.reportError?.(error);
  }
}

const shown = computed(() =>
  GROUPS.map((g) => ({
    ...g,
    items: (items.value[g.kind] ?? []).filter((i) => i.object.name.toLowerCase().includes(filter.value.trim().toLowerCase())),
  })).filter((g) => g.items.length > 0),
);
const total = computed(() => GROUPS.reduce((n, g) => n + (items.value[g.kind]?.length ?? 0), 0));

onMounted(load);
watch(() => [props.schema, props.revision], load);
</script>

<template>
  <section
    class="section types"
    :aria-label="`Types in ${schema}`"
  >
    <div class="section-head">
      <label
        :for="filterId"
        class="visually-hidden"
      >Filter types</label>
      <input
        :id="filterId"
        v-model="filter"
        class="input"
        placeholder="Filter types"
      >
    </div>
    <div class="section-head">
      <select
        v-model="newKind"
        class="select"
        aria-label="Kind of new type"
      >
        <option
          v-for="g in GROUPS"
          :key="g.kind"
          :value="g.kind"
        >
          {{ g.label }}
        </option>
      </select>
      <button
        type="button"
        class="icon-btn"
        aria-label="New type"
        @click="emit('createObject', newKind, { database, schema, parent: null })"
      >
        <Icon name="plus" />
      </button>
    </div>
    <p
      v-if="failed"
      class="hint"
      role="alert"
    >
      The type list could not be read.
      <button
        type="button"
        class="btn compact ghost"
        @click="load"
      >
        Retry
      </button>
    </p>
    <template
      v-for="g in shown"
      :key="g.kind"
    >
      <div class="label">
        {{ g.label }}
      </div>
      <ul class="list">
        <li
          v-for="i in g.items"
          :key="i.object.name"
        >
          <button
            type="button"
            class="list-item stacked"
            :aria-current="current !== null && current.kind === i.object.kind && current.name === i.object.name"
            @click="emit('openObject', i.object)"
          >
            <span class="mono">{{ i.object.name }}</span>
            <span
              v-if="i.detail"
              class="hint"
            >{{ i.detail }}</span>
          </button>
        </li>
      </ul>
    </template>
    <p class="hint">
      {{ total }} types in {{ schema }}
    </p>
  </section>
</template>

<style scoped src="./editor.css"></style>
<style scoped>
.stacked {
  flex-direction: column;
  align-items: flex-start;
  gap: 2px;
}
</style>
