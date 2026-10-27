<script setup lang="ts">
import MyTriggerEditor from './MyTriggerEditor.vue';
import PgTriggerEditor from './PgTriggerEditor.vue';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';

/**
 * Table trigger editor (design canvas TriggerEditor) for PostgreSQL, MySQL
 * and MariaDB. Props and events: `ObjectEditorProps` / `ObjectEditorEmits`.
 * Opens for a `trigger` node (its `parent` is the table), or with
 * `object: null`, `createKind: 'trigger'` and a container whose `parent` is
 * the table. PostgreSQL event triggers use `EventTriggerEditor.vue`.
 */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();
</script>

<template>
  <PgTriggerEditor
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
  <MyTriggerEditor
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
