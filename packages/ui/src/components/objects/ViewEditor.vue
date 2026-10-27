<script setup lang="ts">
import MyViewEditor from './MyViewEditor.vue';
import PgViewEditor from './PgViewEditor.vue';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';

/**
 * View editor (design canvas ViewEditor): definition, properties, columns
 * and DDL of a view, for PostgreSQL, MySQL and MariaDB. Props and events:
 * `ObjectEditorProps` / `ObjectEditorEmits` in `./types.ts`. Opens for a
 * `view` node, or with `object: null` and a container to create one.
 */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();
</script>

<template>
  <PgViewEditor
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
  <MyViewEditor
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
