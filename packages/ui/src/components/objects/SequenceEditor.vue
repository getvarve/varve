<script setup lang="ts">
import MySequenceEditor from './MySequenceEditor.vue';
import PgSequenceEditor from './PgSequenceEditor.vue';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';

/**
 * Sequence editor (design canvas SequenceEditor) for PostgreSQL and
 * MariaDB: properties, current value, reset (setval / RESTART) and DDL.
 * Props and events: `ObjectEditorProps` / `ObjectEditorEmits`. Opens for a
 * `sequence` node, or with `object: null`, `createKind: 'sequence'`.
 */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();
</script>

<template>
  <PgSequenceEditor
    v-if="props.session.engine === 'postgres'"
    v-bind="props"
    @changed="emit('changed')"
    @opened="emit('opened', $event)"
    @dropped="emit('dropped', $event)"
    @open-sql="emit('openSql', $event)"
    @open-object="emit('openObject', $event)"
    @create-object="(k, c) => emit('createObject', k, c)"
    @reconnect="emit('reconnect')"
  />
  <MySequenceEditor
    v-else
    v-bind="props"
    @changed="emit('changed')"
    @opened="emit('opened', $event)"
    @dropped="emit('dropped', $event)"
    @open-sql="emit('openSql', $event)"
    @open-object="emit('openObject', $event)"
    @create-object="(k, c) => emit('createObject', k, c)"
    @reconnect="emit('reconnect')"
  />
</template>
