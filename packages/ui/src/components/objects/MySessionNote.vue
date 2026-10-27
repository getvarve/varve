<script setup lang="ts">
import { computed } from 'vue';
import Icon from '../common/Icon.vue';

/**
 * MySQL and MariaDB store the session settings an object was created with
 * (sql_mode, character set, collation) and use them whenever it runs.
 * Saving from a form recreates the object with this tab's settings; the
 * DDL tab carries the object's own settings in its session header.
 */
const props = withDefaults(
  defineProps<{ sqlMode?: string | null; charsetClient?: string | null; collationConnection?: string | null; databaseCollation?: string | null }>(),
  { sqlMode: null, charsetClient: null, collationConnection: null, databaseCollation: null },
);

const settings = computed(() =>
  [
    ['sql_mode', props.sqlMode],
    ['character_set_client', props.charsetClient],
    ['collation_connection', props.collationConnection],
    ['database collation', props.databaseCollation],
  ].filter((pair): pair is [string, string] => typeof pair[1] === 'string'),
);
</script>

<template>
  <div
    v-if="settings.length > 0"
    class="notice tone-info note"
    role="note"
  >
    <Icon name="info" />
    <div>
      <p>
        Saving from this form recreates the object with this tab's session settings. It was created with:
      </p>
      <p
        v-for="[key, value] in settings"
        :key="key"
        class="mono setting"
      >
        {{ key }} = {{ value === '' ? "''" : value }}
      </p>
      <p>To keep those exactly, use Edit and run in the DDL tab: its DDL carries them.</p>
    </div>
  </div>
</template>

<style scoped>
.note {
  align-items: flex-start;
}

.note p {
  margin: 0 0 4px;
  overflow-wrap: anywhere;
}

.setting {
  font-size: 11px;
}
</style>
