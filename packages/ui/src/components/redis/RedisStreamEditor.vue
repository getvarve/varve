<script setup lang="ts">
import { reactive, ref, useId } from 'vue';
import type { StreamEntry } from '../../api/redis';
import type { Value } from '../../api/sessions';
import { isStreamId, targetOf, textValue } from '../../lib/redisEdit';
import type { EditDraft } from '../../lib/redisEdit';
import { cellClass, cellText, fullText, isPlaceholder, placeholderHelp } from '../../lib/values';
import Icon from '../common/Icon.vue';

/**
 * Stream entries with delete (XDEL), a new entry (XADD, automatic or explicit
 * id) and trimming (XTRIM MAXLEN / MINID), staged into the key's draft.
 */
const props = defineProps<{ entries: readonly StreamEntry[]; draft: EditDraft; editable: boolean }>();

const uid = useId();

function tip(value: Value): string {
  return isPlaceholder(value) ? placeholderHelp(value) : fullText(value);
}

function removed(id: string): boolean {
  void props.draft.size;
  return props.draft.pending(targetOf.id(id))?.op === 'streamDelete';
}

// New entry
const entry = reactive<{ auto: boolean; id: string; fields: { field: string; value: string }[] }>({
  auto: true,
  id: '',
  fields: [{ field: '', value: '' }],
});
const addError = ref<string | null>(null);

function addEntry(): void {
  if (!entry.auto && !isStreamId(entry.id)) {
    addError.value = 'An explicit id is <milliseconds>-<sequence>, for example 1792333914012-0.';
    return;
  }
  const fields = entry.fields.filter((f) => f.field !== '' || f.value !== '');
  if (fields.length === 0 || fields.some((f) => f.field === '')) {
    addError.value = 'Every entry needs at least one field, and every field a name.';
    return;
  }
  addError.value = null;
  props.draft.stage({
    op: 'streamAdd',
    id: entry.auto ? null : entry.id.trim(),
    fields: fields.map((f) => ({ field: textValue(f.field), value: textValue(f.value) })),
  });
  entry.id = '';
  entry.fields = [{ field: '', value: '' }];
}

// Trim
const trim = reactive<{ strategy: 'maxLen' | 'minId'; threshold: string; approximate: boolean }>({
  strategy: 'maxLen',
  threshold: '',
  approximate: true,
});
const trimError = ref<string | null>(null);

function stageTrim(): void {
  const t = trim.threshold.trim();
  const ok = trim.strategy === 'maxLen' ? /^\d{1,19}$/.test(t) : isStreamId(t);
  if (!ok) {
    trimError.value = trim.strategy === 'maxLen' ? 'Enter how many entries to keep.' : 'Enter the oldest id to keep, for example 1792333914012-0.';
    return;
  }
  trimError.value = null;
  props.draft.stage({ op: 'streamTrim', strategy: trim.strategy, threshold: t, approximate: trim.approximate });
}
</script>

<template>
  <div class="editor">
    <table class="table">
      <thead>
        <tr>
          <th scope="col">
            ID
          </th>
          <th scope="col">
            fields
          </th>
          <th
            v-if="editable"
            scope="col"
            class="actions"
          >
            <span class="visually-hidden">Actions</span>
          </th>
        </tr>
      </thead>
      <tbody>
        <tr
          v-for="e in entries"
          :key="e.id"
          :class="{ removed: removed(e.id) }"
        >
          <td class="mono nowrap">
            {{ e.id }}
          </td>
          <td class="mono">
            <span
              v-for="(f, i) in e.fields"
              :key="i"
              class="pair"
            ><span class="subtle">{{ cellText(f.field) }}</span> <span
              :class="cellClass(f.value)"
              :title="tip(f.value)"
            >{{ cellText(f.value) }}</span></span>
          </td>
          <td
            v-if="editable"
            class="actions"
          >
            <button
              v-if="removed(e.id)"
              type="button"
              class="btn ghost compact"
              @click="draft.unstage(targetOf.id(e.id))"
            >
              Undo
            </button>
            <button
              v-else
              type="button"
              class="btn ghost compact"
              :aria-label="`Delete entry ${e.id}`"
              @click="draft.stage({ op: 'streamDelete', id: e.id })"
            >
              Delete
            </button>
          </td>
        </tr>
      </tbody>
    </table>

    <form
      v-if="editable"
      class="panel"
      novalidate
      :aria-labelledby="`${uid}-add-title`"
      @submit.prevent="addEntry"
    >
      <h3
        :id="`${uid}-add-title`"
        class="label"
      >
        Add entry
      </h3>
      <div
        class="row"
        role="radiogroup"
        aria-label="Entry id"
      >
        <label><input
          v-model="entry.auto"
          type="radio"
          :value="true"
        > Auto (<span class="mono">*</span>)</label>
        <label><input
          v-model="entry.auto"
          type="radio"
          :value="false"
        > Explicit</label>
        <label
          v-if="!entry.auto"
          class="visually-hidden"
          :for="`${uid}-id`"
        >Entry id</label>
        <input
          v-if="!entry.auto"
          :id="`${uid}-id`"
          v-model="entry.id"
          class="input compact mono"
          placeholder="1792333914012-0"
          spellcheck="false"
        >
      </div>
      <div
        v-for="(f, i) in entry.fields"
        :key="i"
        class="row"
      >
        <label
          class="visually-hidden"
          :for="`${uid}-f-${i}`"
        >Field {{ i + 1 }}</label>
        <input
          :id="`${uid}-f-${i}`"
          v-model="f.field"
          class="input compact mono"
          placeholder="field"
          spellcheck="false"
        >
        <label
          class="visually-hidden"
          :for="`${uid}-v-${i}`"
        >Value {{ i + 1 }}</label>
        <input
          :id="`${uid}-v-${i}`"
          v-model="f.value"
          class="input compact mono grow"
          placeholder="value"
          spellcheck="false"
        >
        <button
          v-if="entry.fields.length > 1"
          type="button"
          class="icon-btn"
          :aria-label="`Remove field ${i + 1}`"
          @click="entry.fields.splice(i, 1)"
        >
          <Icon name="close" />
        </button>
      </div>
      <div class="row">
        <button
          type="button"
          class="btn ghost compact"
          @click="entry.fields.push({ field: '', value: '' })"
        >
          <Icon name="plus" />Add field
        </button>
        <span class="spacer" />
        <button
          type="submit"
          class="btn compact"
        >
          Add entry (XADD)
        </button>
      </div>
      <span
        v-if="addError"
        class="error"
        role="alert"
      >{{ addError }}</span>
    </form>

    <form
      v-if="editable"
      class="panel"
      novalidate
      :aria-labelledby="`${uid}-trim-title`"
      @submit.prevent="stageTrim"
    >
      <h3
        :id="`${uid}-trim-title`"
        class="label"
      >
        Trim
      </h3>
      <div class="row">
        <label
          class="visually-hidden"
          :for="`${uid}-strategy`"
        >Trim by</label>
        <select
          :id="`${uid}-strategy`"
          v-model="trim.strategy"
          class="select compact"
        >
          <option value="maxLen">
            MAXLEN (keep the newest N)
          </option>
          <option value="minId">
            MINID (drop older ids)
          </option>
        </select>
        <label
          class="visually-hidden"
          :for="`${uid}-threshold`"
        >Threshold</label>
        <input
          :id="`${uid}-threshold`"
          v-model="trim.threshold"
          class="input compact mono"
          :placeholder="trim.strategy === 'maxLen' ? '50000' : '1792333914012-0'"
          spellcheck="false"
        >
        <label><input
          v-model="trim.approximate"
          type="checkbox"
        > Approximate (<span class="mono">~</span>, faster)</label>
        <span class="spacer" />
        <button
          type="submit"
          class="btn compact"
        >
          Trim (XTRIM)
        </button>
      </div>
      <span
        v-if="trimError"
        class="error"
        role="alert"
      >{{ trimError }}</span>
    </form>
  </div>
</template>

<style scoped src="./editorTable.css"></style>
<style scoped>
.pair {
  margin-right: 12px;
}

.panel {
  display: flex;
  flex-direction: column;
  gap: 6px;
  padding: 10px 12px;
  border: 1px solid var(--border-inner);
  border-radius: var(--radius-panel);
  font-size: 12px;
}

.panel h3 {
  margin: 0;
}

.row {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
}

.row label {
  display: inline-flex;
  align-items: center;
  gap: 4px;
}

.grow {
  flex: 1;
  min-width: 160px;
}

.spacer {
  flex: 1;
}
</style>
