<script setup lang="ts">
import MyRoutineEditor from './MyRoutineEditor.vue';
import PgRoutineEditor from './PgRoutineEditor.vue';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';

/**
 * Function and procedure editor (design canvas FunctionEditor) for
 * PostgreSQL, MySQL and MariaDB. Props and events: `ObjectEditorProps` /
 * `ObjectEditorEmits`. Opens for `function` and `procedure` nodes, or with
 * `object: null`, `createKind: 'function' | 'procedure'` and a container.
 */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();
</script>

<template>
  <PgRoutineEditor
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
  <MyRoutineEditor
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
