<script setup lang="ts">
import MyDatabaseEditor from './MyDatabaseEditor.vue';
import PgDatabaseEditor from './PgDatabaseEditor.vue';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';

/**
 * Database editor for PostgreSQL, MySQL and MariaDB. Props and events:
 * `ObjectEditorProps` / `ObjectEditorEmits`. Opens for a `database` node
 * (Properties, Drop database), or with `object: null`,
 * `createKind: 'database'` ("New database").
 */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();
</script>

<template>
  <PgDatabaseEditor
    v-if="props.session.engine === 'postgres'"
    v-bind="props"
    @changed="emit('changed')"
    @opened="emit('opened', $event)"
    @dropped="emit('dropped', $event)"
    @open-sql="emit('openSql', $event)"
    @reconnect="emit('reconnect')"
    @switch-database="emit('switchDatabase', $event)"
  />
  <MyDatabaseEditor
    v-else
    v-bind="props"
    @changed="emit('changed')"
    @opened="emit('opened', $event)"
    @dropped="emit('dropped', $event)"
    @open-sql="emit('openSql', $event)"
    @reconnect="emit('reconnect')"
  />
</template>
