import { computed, onBeforeUnmount, reactive, ref, toRaw, watch } from 'vue';
import type { Ref, UnwrapRef } from 'vue';
import { catalogDescribe } from '../../api/ddl';
import type { EngineChange, ObjectDetails, ObjectRef } from '../../api/ddl';
import { presentError, toCmdError } from '../../api/errors';
import { latestOnly } from '../../lib/async';
import { buildDrop } from '../../lib/ddlPlan';
import type { FieldErrors } from '../../lib/ddlPlan';
import type { StepSpec } from '../../lib/ddlApply';
import type { ObjectEditorProps } from './types';
import { useChangePlan } from './useChangePlan';

/** How long the form must be still before its change is planned on the server. */
const PLAN_DELAY_MS = 400;

export interface ObjectEditorEmit {
  (event: 'changed'): void;
  (event: 'opened', object: ObjectRef): void;
  // Separate overloads on purpose: `defineEmits` produces one overload per event, and a combined signature is not assignable from it.
  // eslint-disable-next-line @typescript-eslint/unified-signatures
  (event: 'dropped', object: ObjectRef): void;
}

export interface ObjectEditorOptions<D, F extends object> {
  props: ObjectEditorProps;
  emit: ObjectEditorEmit;
  /** The described object of the editor's kind, or a message when the details are of another kind. */
  pick: (details: ObjectDetails) => D | string;
  /** Loads extra data after the details (lists for pickers); optional. */
  afterLoad?: (details: D | null) => Promise<void>;
  /** The form for the loaded object, or a blank one for a new object. */
  formFrom: (details: D | null) => F;
  /** Messages per field; empty when the form can be planned. */
  validate: (form: F) => FieldErrors;
  /** A reason the change cannot be planned at all (shown instead of the SQL), or `null`. */
  blocked?: (form: F, details: D | null) => string | null;
  /** The steps that save the form. */
  build: (form: F, details: D | null) => StepSpec[];
  /** The object as it is called after a successful save (rename, create). */
  identity: (form: F, details: D | null) => ObjectRef;
  /** Looks the saved object up when `identity` cannot know its exact reference (routine signatures as the server prints them). */
  resolveIdentity?: (guess: ObjectRef, form: F, details: D | null) => Promise<ObjectRef>;
  /** The drop change; defaults to `buildDrop` (editors with drop options such as FORCE supply their own). */
  dropChange?: (object: ObjectRef, cascade: boolean) => EngineChange | null;
  /** Runs right before a plan runs (after the user confirmed a destructive one). */
  beforeApply?: () => Promise<void>;
  /** Extra sentences for the destructive confirmation. */
  confirmWarnings?: () => string[];
  /** Runs after every step ran, before the tree refreshes. */
  afterRun?: () => Promise<void>;
}

/**
 * The shared life of an object editor: describe the object, keep a form
 * and its original, plan the change while the user edits (debounced), apply
 * it, then reload under the object's new identity. Drop mode plans the
 * object's drop instead.
 */
export function useObjectEditor<D, F extends object>(options: ObjectEditorOptions<D, F>) {
  const { props, emit } = options;
  /** The object the editor shows now; `null` while creating. */
  const current = ref<ObjectRef | null>(props.object) as Ref<ObjectRef | null>;
  const details = ref<D | null>(null) as Ref<D | null>;
  const form = reactive(options.formFrom(null)) as UnwrapRef<F>;
  const baseline = ref('');
  const loading = ref(false);
  const loadError = ref<string | null>(null);
  /** Loading the pickers' lists failed (names can still be typed). */
  const listError = ref<string | null>(null);
  const mode = ref<'edit' | 'drop'>('edit');
  const cascade = ref(false);
  const latest = latestOnly();
  /** The `drop` intent is honoured once, on the first load. */
  let dropOffered = false;

  const plan = useChangePlan({
    session: () => props.session,
    connectionName: () => props.connectionName,
    onChanged: () => emit('changed'),
    reportError: (error) => props.reportError?.(error),
    beforeApply: options.beforeApply,
    confirmWarnings: options.confirmWarnings,
    afterRun: options.afterRun,
  });

  const formValue = (): F => structuredClone(toRaw(form)) as F;
  const snapshot = (): string => JSON.stringify(form);
  baseline.value = snapshot();
  const dirty = computed(() => snapshot() !== baseline.value);
  const errors = computed<FieldErrors>(() => options.validate(form as F));
  const blockedReason = computed(() => options.blocked?.(form as F, details.value) ?? null);
  const creating = computed(() => current.value === null);

  function resetForm(value: F): void {
    const target = form as unknown as Record<string, unknown>;
    for (const key of Object.keys(target)) Reflect.deleteProperty(target, key);
    Object.assign(target, value);
    baseline.value = snapshot();
  }

  async function load(): Promise<void> {
    const token = latest.next();
    mode.value = 'edit';
    listError.value = null;
    plan.clear();
    if (current.value === null) {
      details.value = null;
      loadError.value = null;
      loading.value = true;
      try {
        await options.afterLoad?.(null);
      } catch (error: unknown) {
        // Picker lists are a convenience: the form works without them, and the editor says they are missing.
        listError.value = presentError(toCmdError(error)).message;
      }
      if (!latest.isCurrent(token)) return;
      loading.value = false;
      resetForm(options.formFrom(null));
      return;
    }
    loading.value = true;
    loadError.value = null;
    try {
      const described = await catalogDescribe(props.session.id, current.value);
      if (!latest.isCurrent(token)) return;
      const picked = options.pick(described);
      if (typeof picked === 'string') {
        loadError.value = picked;
        return;
      }
      details.value = picked;
      try {
        await options.afterLoad?.(picked);
      } catch (error: unknown) {
        listError.value = presentError(toCmdError(error)).message;
      }
      if (!latest.isCurrent(token)) return;
      resetForm(options.formFrom(picked));
      if (props.intent === 'drop' && !dropOffered) {
        dropOffered = true;
        mode.value = 'drop';
      }
    } catch (error: unknown) {
      props.reportError?.(error);
      if (!latest.isCurrent(token)) return;
      const p = presentError(toCmdError(error));
      loadError.value = p.detail ? `${p.message} (${p.detail})` : p.message;
    } finally {
      if (latest.isCurrent(token)) loading.value = false;
    }
  }

  let timer: ReturnType<typeof setTimeout> | null = null;
  function schedulePlan(): void {
    if (timer !== null) clearTimeout(timer);
    timer = setTimeout(() => {
      timer = null;
      void planNow();
    }, PLAN_DELAY_MS);
  }

  async function planNow(): Promise<void> {
    if (mode.value === 'drop') {
      const object = current.value;
      const change = object ? (options.dropChange ?? ((o, c) => buildDrop(props.session.engine, o, c)))(object, cascade.value) : null;
      await plan.plan(change ? [{ kind: 'edit', change }] : []);
      return;
    }
    if (!dirty.value || Object.keys(errors.value).length > 0 || blockedReason.value !== null || loading.value) {
      await plan.plan([]);
      return;
    }
    await plan.plan(options.build(formValue(), details.value));
  }

  watch(() => snapshot(), schedulePlan);
  watch(dirty, (value) => props.reportDirty?.(value), { immediate: true });
  watch([mode, cascade], () => void planNow());
  onBeforeUnmount(() => {
    if (timer !== null) clearTimeout(timer);
    props.reportDirty?.(false);
  });

  /** After the plan ran: follow the new identity and reload. */
  async function onApplied(): Promise<void> {
    if (mode.value === 'drop') {
      const dropped = current.value;
      if (dropped) emit('dropped', dropped);
      mode.value = 'edit';
      plan.clear();
      return;
    }
    const value = formValue();
    let next = options.identity(value, details.value);
    if (options.resolveIdentity) {
      try {
        next = await options.resolveIdentity(next, value, details.value);
      } catch (error: unknown) {
        props.reportError?.(error);
      }
    }
    const before = current.value;
    current.value = next;
    if (!before || JSON.stringify(before) !== JSON.stringify(next)) emit('opened', next);
    await load();
  }

  function revert(): void {
    if (details.value !== null || current.value === null) resetForm(options.formFrom(details.value));
    plan.clear();
  }

  function startDrop(): void {
    cascade.value = false;
    mode.value = 'drop';
  }

  function cancelDrop(): void {
    mode.value = 'edit';
    void planNow();
  }

  watch(
    () => props.object,
    (object) => {
      if (JSON.stringify(object) === JSON.stringify(current.value)) return;
      current.value = object;
      void load();
    },
  );
  // A new session for the same tab (reconnect, or "run from another database"): reload, but keep what the
  // user typed and the drop in progress.
  watch(
    () => props.session.id,
    async () => {
      const kept = dirty.value ? formValue() : null;
      const dropping = mode.value === 'drop';
      const keptCascade = cascade.value;
      await load();
      if (loadError.value !== null) return;
      if (kept) {
        const target = form as unknown as Record<string, unknown>;
        Object.assign(target, kept);
      }
      if (dropping && current.value !== null) {
        cascade.value = keptCascade;
        mode.value = 'drop';
      }
    },
  );

  return {
    current,
    details,
    form,
    errors,
    dirty,
    creating,
    loading,
    loadError,
    listError,
    blockedReason,
    mode,
    cascade,
    plan,
    load,
    revert,
    onApplied,
    startDrop,
    cancelDrop,
    planNow,
  };
}
