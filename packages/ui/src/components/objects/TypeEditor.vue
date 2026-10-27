<script setup lang="ts">
import { computed } from 'vue';
import CompositeTypeEditor from './CompositeTypeEditor.vue';
import DomainEditor from './DomainEditor.vue';
import EnumTypeEditor from './EnumTypeEditor.vue';
import RangeTypeEditor from './RangeTypeEditor.vue';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';

/**
 * PostgreSQL type and domain editor (design canvas TypeEditor): the types
 * of the schema on the left, the editor of the chosen kind (enum,
 * composite, range, domain) in the middle. Props and events:
 * `ObjectEditorProps` / `ObjectEditorEmits`. Opens for `enumType`,
 * `compositeType`, `rangeType` and `domain` nodes, or with `object: null`
 * and `createKind` one of those.
 */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();
const kind = computed(() => props.object?.kind ?? props.createKind ?? 'enumType');
const component = computed(() => {
  switch (kind.value) {
    case 'compositeType':
      return CompositeTypeEditor;
    case 'rangeType':
      return RangeTypeEditor;
    case 'domain':
      return DomainEditor;
    default:
      return EnumTypeEditor;
  }
});
</script>

<template>
  <component
    :is="component"
    v-bind="props"
    @changed="emit('changed')"
    @opened="emit('opened', $event)"
    @dropped="emit('dropped', $event)"
    @open-sql="emit('openSql', $event)"
    @open-object="emit('openObject', $event)"
    @create-object="(k: ObjectEditorEmits['createObject'][0], c: ObjectEditorEmits['createObject'][1]) => emit('createObject', k, c)"
    @reconnect="emit('reconnect')"
  />
</template>
