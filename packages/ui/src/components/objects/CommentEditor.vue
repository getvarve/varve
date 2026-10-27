<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, useId, watch } from 'vue';
import { displayName, kindLabel } from '../../api/catalog';
import type { ObjectRef } from '../../api/catalog';
import { catalogDescribe } from '../../api/ddl';
import { presentError, toCmdError } from '../../api/errors';
import type { SessionInfo } from '../../api/sessions';
import { useDialogs } from '../../composables/useDialogs';
import { latestOnly } from '../../lib/async';
import { buildObjectComment, commentChanged, commentHidden, commentOf, commentSupported } from '../../lib/objectComment';
import FormField from '../common/FormField.vue';
import Icon from '../common/Icon.vue';
import PlanPreview from './PlanPreview.vue';
import { useChangePlan } from './useChangePlan';

/**
 * The comment of one object (design doc 4.2, "Comments E on every
 * object"), for kinds without an editor of their own (partitions,
 * aggregates, operators, casts, collations, publications, subscriptions,
 * foreign data wrappers and servers, foreign tables, tablespaces, indexes)
 * and for editors that lack a comment field (extensions), on PostgreSQL.
 * Table constraints are commented in the table designer's Constraints
 * section (their tree nodes open it) and domain checks in the domain
 * editor; this editor shows a constraint's comment only when its node
 * opens the object tab instead, because the server does not let the
 * table designer edit tables. Reads the current comment from
 * `catalog_describe`, plans `COMMENT ON` through `edit_plan` and shows the
 * SQL in the plan preview before it runs. The comment is stored exactly as
 * typed; an empty one removes it. A comment the role cannot read is shown
 * as unknown and cannot be changed, so applying never overwrites a comment
 * the user did not see. Reloading asks before discarding typed text.
 * Read-only sessions can read but not plan or apply. Renders nothing for
 * kinds the engine cannot comment on (see `lib/objectComment.ts`).
 *
 * `reportDirty` is called whenever typed text starts or stops differing
 * from the stored comment (and with `false` on unmount). Emits `changed`
 * after the comment was applied (refresh the tree and any DDL shown) and
 * `reconnect` when the plan preview asks for it.
 */
const props = withDefaults(
  defineProps<{
    session: SessionInfo;
    connectionName: string;
    object: ObjectRef;
    reportError?: ((error: unknown) => void) | null;
    reportDirty?: ((dirty: boolean) => void) | null;
  }>(),
  { reportError: null, reportDirty: null },
);
const emit = defineEmits<{ changed: []; reconnect: [] }>();

const inputId = useId();
const supported = computed(() => commentSupported(props.session.engine, props.object.kind));
/** The stored comment; `undefined` until read, or when it could not be read. */
const current = ref<string | null | undefined>(undefined);
const draft = ref('');
const loading = ref(false);
const loadError = ref<string | null>(null);
const dirty = computed(() => current.value !== undefined && commentChanged(draft.value, current.value));

const plan = useChangePlan({
  session: () => props.session,
  connectionName: () => props.connectionName,
  onChanged: () => emit('changed'),
  reportError: (error) => props.reportError?.(error),
});
const previewing = computed(() => plan.state.steps.length > 0 || plan.state.planning || plan.state.planProblem !== null);

const latest = latestOnly();

/** Reads the stored comment and replaces the draft; only the latest call writes. */
async function load(): Promise<void> {
  if (!supported.value) return;
  const token = latest.next();
  loading.value = true;
  loadError.value = null;
  try {
    const details = await catalogDescribe(props.session.id, props.object);
    if (!latest.isCurrent(token)) return;
    const comment = commentOf(details);
    if (comment === undefined) {
      current.value = undefined;
      draft.value = '';
      const what = kindLabel(props.object.kind).toLocaleLowerCase();
      loadError.value = commentHidden(details)
        ? `The comment of this ${what} cannot be read with this role, so it cannot be changed here.`
        : `The details of this ${what} do not include its comment, so it cannot be changed here.`;
      return;
    }
    current.value = comment;
    draft.value = comment ?? '';
  } catch (error: unknown) {
    props.reportError?.(error);
    if (!latest.isCurrent(token)) return;
    current.value = undefined;
    loadError.value = presentError(toCmdError(error)).message;
  } finally {
    if (latest.isCurrent(token)) loading.value = false;
  }
}

/** Reload on request: asks first when typed text would be discarded. */
async function reload(): Promise<void> {
  if (dirty.value) {
    const discard = await useDialogs().askConfirm({
      title: 'Discard the typed comment?',
      message: 'The comment you typed has not been applied. Reloading replaces it with the stored comment.',
      confirmLabel: 'Discard and reload',
      danger: false,
    });
    if (!discard) return;
  }
  plan.clear();
  await load();
}

async function preview(): Promise<void> {
  if (!dirty.value || props.session.readOnly) return;
  const change = buildObjectComment(props.session.engine, props.object, draft.value);
  if (change) await plan.plan([{ kind: 'edit', change }]);
}

function revert(): void {
  draft.value = current.value ?? '';
  plan.clear();
}

async function applied(): Promise<void> {
  plan.clear();
  await load();
}

watch(dirty, (value) => props.reportDirty?.(value));
onBeforeUnmount(() => {
  if (dirty.value) props.reportDirty?.(false);
});
// Typing after a preview makes the planned SQL stale.
watch(draft, () => {
  if (plan.state.steps.length > 0 && !plan.state.applying) plan.clear();
});
watch(
  () => props.object,
  () => {
    plan.clear();
    void load();
  },
);
onMounted(load);
</script>

<template>
  <section
    v-if="supported"
    class="section"
    :aria-label="`Comment on ${displayName(object)}`"
  >
    <div class="section-head">
      <h2>Comment</h2>
      <span
        v-if="session.readOnly"
        class="pill tone-neutral"
      ><Icon name="lock" />Read only</span>
      <span class="grow" />
      <button
        type="button"
        class="icon-btn"
        aria-label="Reload the comment"
        :disabled="loading"
        @click="reload"
      >
        <Icon name="refresh" />
      </button>
    </div>
    <p
      v-if="loadError"
      class="notice tone-danger"
      role="alert"
    >
      <Icon name="warning" />{{ loadError }}
    </p>
    <FormField
      label="Comment"
      :input-id="inputId"
      help="Saved exactly as typed, spaces and line breaks included. Leave it empty to remove the comment."
    >
      <textarea
        :id="inputId"
        v-model="draft"
        class="textarea"
        rows="2"
        :disabled="loading || current === undefined || session.readOnly"
      />
    </FormField>
    <div class="actions">
      <button
        type="button"
        class="btn compact"
        :disabled="!dirty"
        @click="revert"
      >
        Revert
      </button>
      <button
        type="button"
        class="btn compact"
        :disabled="session.readOnly || !dirty || plan.state.planning"
        @click="preview"
      >
        Preview change
      </button>
    </div>
    <PlanPreview
      v-if="previewing"
      :plan="plan"
      :engine="session.engine"
      :read-only="session.readOnly"
      apply-label="Set comment"
      @applied="applied"
      @reconnect="emit('reconnect')"
      @refresh="reload"
    />
  </section>
</template>

<style scoped src="./editor.css"></style>
<style scoped>
.textarea {
  font-family: var(--font-sans);
  font-size: 13px;
}

.actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}
</style>
