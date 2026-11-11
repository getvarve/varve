import { computed, onBeforeUnmount, provide, reactive, ref, toRaw, watch } from 'vue';
import type { Ref, UnwrapRef } from 'vue';
import { catalogDescribe } from '../../api/ddl';
import type { EngineChange, ObjectDetails, ObjectRef } from '../../api/ddl';
import { presentError, toCmdError } from '../../api/errors';
import { latestOnly } from '../../lib/async';
import { isMissing } from '../../lib/ddlApply';
import { buildDrop } from '../../lib/ddlPlan';
import type { FieldErrors } from '../../lib/ddlPlan';
import type { StepSpec } from '../../lib/ddlApply';
import { OBJECT_EDITOR_CONTEXT, displayName } from './types';
import type { EditorNotice, ObjectEditorProps } from './types';
import { useChangePlan } from './useChangePlan';

/** How long the form must be still before its change is planned on the server. */
const PLAN_DELAY_MS = 400;
/** Edits that build no change. */
const NO_EFFECT = 'These edits change nothing on the server. Revert them or edit another field.';

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
  /**
   * For a save that cannot be planned from the form again once some of its
   * steps ran (a materialized view recreated under a temporary name). Called
   * after such a partial run and on every reload until the save is over:
   * reads what exists on the server and resolves to the sentence that says
   * so, or to `null` when nothing is left half done (this was no such save,
   * or it was cleaned up by hand). While it resolves to text, or rejects
   * because the server could not be read, the save is unfinished: the form,
   * the object and the steps that did not run stay as they are, nothing is
   * planned from the form, and the editor offers to plan the remaining steps.
   */
  unfinished?: (form: F, details: D | null) => Promise<string | null>;
  /** Names of form fields for the notice that lists what changed on the server; a field without one is named after its key. */
  fieldLabels?: Record<string, string>;
}

/** Line endings do not count as edits: CodeMirror hands back `\n` for a body stored with `\r\n`. */
function sameLineEndings(_key: string, value: unknown): unknown {
  return typeof value === 'string' ? value.replace(/\r\n?/g, '\n') : value;
}

interface FetchOptions {
  /** Keep what the user typed: fields they changed stay, the others follow the server; drop mode stays. */
  keepEdits: boolean;
  /** Leave the plan preview alone (a failed apply keeps its message and its "Plan again"). */
  keepPlan?: boolean;
  /** Some steps of the last apply ran: when the object is gone under its old name, look under the name it was being saved as. */
  afterPartialRun?: boolean;
}

/** `comment`, or `definer user` for `definerUser`: a form key as words. */
function keyAsWords(key: string): string {
  return key.replace(/([a-z0-9])([A-Z])/g, '$1 $2').toLowerCase();
}

/** `a`, `a and b`, `a, b and c`. */
function listOf(items: string[]): string {
  if (items.length <= 1) return items[0] ?? '';
  return `${items.slice(0, -1).join(', ')} and ${items.at(-1) ?? ''}`;
}

/**
 * The shared life of an object editor: describe the object, keep a form
 * and its original, plan the change while the user edits (debounced), apply
 * it, then reload under the object's new identity. Drop mode plans the
 * object's drop instead.
 *
 * `load()` reads the object again without throwing away the form: fields
 * the user changed stay (on top of the new details, so they diff against
 * what the server has now), drop mode stays, and the sections stay mounted
 * while it reads (only the first load shows "Loading"). When the object
 * is gone and the form holds edits, the editor turns into a create form
 * with those edits. Edited DDL in the DDL section counts like those edits:
 * while it has not run, no reload replaces the sections (an object that is
 * gone is reported in a notice instead). A field the user edited that also
 * changed on the server keeps the user's value, and a notice names it and
 * offers the server's value. After a save the editor reloads from scratch.
 */
export function useObjectEditor<D, F extends object>(options: ObjectEditorOptions<D, F>) {
  const { props, emit } = options;
  /** The object the editor shows now; `null` while creating. */
  const current = ref<ObjectRef | null>(props.object) as Ref<ObjectRef | null>;
  const details = ref<D | null>(null) as Ref<D | null>;
  const form = reactive(options.formFrom(null)) as UnwrapRef<F>;
  const baseline = ref('');
  /** Nothing to show yet (first load, or a load after a failed one): the sections wait. */
  const loading = ref(false);
  /** The loaded object is being read again; the sections stay. */
  const refreshing = ref(false);
  const loadError = ref<string | null>(null);
  /** Loading the pickers' lists failed (names can still be typed). */
  const listError = ref<string | null>(null);
  const notice = ref<EditorNotice | null>(null);
  const mode = ref<'edit' | 'drop'>('edit');
  const cascade = ref(false);
  /** Edited DDL in the DDL section that has not run. */
  const ddlDirty = ref(false);
  /** A save stopped part way and cannot be planned from the form again (`options.unfinished`): its remaining steps are the plan. */
  const unfinished = ref(false);
  /** Form fields the user edited that changed on the server too; the user's values are in the form. */
  const conflicts = ref<string[]>([]);
  const latest = latestOnly();
  /** The `drop` intent is honoured once, on the first load. */
  let dropOffered = false;
  /** A plan was asked for while a plan applied; plan once the apply is over. */
  let replanWhenIdle = false;
  /** The sections were shown once: later loads keep them on screen. */
  let everLoaded = false;

  const plan = useChangePlan({
    session: () => props.session,
    connectionName: () => props.connectionName,
    onChanged: () => emit('changed'),
    reportError: (error, sessionId) => props.reportError?.(error, sessionId),
    beforeApply: options.beforeApply,
    confirmWarnings: options.confirmWarnings,
    afterRun: options.afterRun,
    onPartial: () => void afterPartialRun(),
  });

  const formValue = (): F => structuredClone(toRaw(form)) as F;
  const snapshot = (): string => JSON.stringify(form, sameLineEndings);
  baseline.value = snapshot();
  const dirty = computed(() => snapshot() !== baseline.value);
  const errors = computed<FieldErrors>(() => options.validate(form as F));
  /**
   * Why the edits cannot be planned: the editor's own reason, or edits that
   * change nothing the server stores (so the preview does not just say
   * "No changes yet" under an unsaved form).
   */
  const blockedReason = computed(() => {
    // The remaining steps of an unfinished save are not planned from the form: nothing about the form blocks them.
    if (unfinished.value) return null;
    const reason = options.blocked?.(form as F, details.value) ?? null;
    if (reason !== null) return reason;
    if (mode.value !== 'edit' || !dirty.value || Object.keys(errors.value).length > 0) return null;
    return options.build(formValue(), details.value).length === 0 ? NO_EFFECT : null;
  });
  const creating = computed(() => current.value === null);
  const busy = computed(() => plan.state.applying);

  provide(OBJECT_EDITOR_CONTEXT, {
    busy,
    refreshing,
    notice,
    reportDdlDirty: (value) => {
      ddlDirty.value = value;
    },
    ddlDirty,
    session: computed(() => props.session),
    current,
    cascade,
    reportError: (error, sessionId) => props.reportError?.(error, sessionId),
  });

  function resetForm(value: F): void {
    const target = form as unknown as Record<string, unknown>;
    for (const key of Object.keys(target)) Reflect.deleteProperty(target, key);
    Object.assign(target, value);
    baseline.value = snapshot();
  }

  /** The fields the user changed against the loaded state. */
  function editedFields(): Partial<F> {
    const before = JSON.parse(baseline.value) as Record<string, unknown>;
    const now = formValue() as Record<string, unknown>;
    const out: Record<string, unknown> = {};
    for (const [key, value] of Object.entries(now)) {
      if (JSON.stringify(value, sameLineEndings) !== JSON.stringify(before[key], sameLineEndings)) out[key] = value;
    }
    return out as Partial<F>;
  }

  const stable = (value: unknown): string => JSON.stringify(value, sameLineEndings);

  /** Of the edited fields, the ones whose loaded value changed on the server (from `before` to `fresh`) to something else than the edit. */
  function changedOnServer(edits: Partial<F>, before: Record<string, unknown>, fresh: F): string[] {
    const server = fresh as Record<string, unknown>;
    return Object.entries(edits)
      .filter(([key, value]) => stable(server[key]) !== stable(before[key]) && stable(server[key]) !== stable(value))
      .map(([key]) => key);
  }

  /** The notice for `conflicts`, or `null` when there are none. */
  function conflictNotice(): EditorNotice | null {
    const keys = conflicts.value;
    if (keys.length === 0) return null;
    const names = listOf(keys.map((key) => options.fieldLabels?.[key] ?? keyAsWords(key)));
    const text =
      keys.length === 1
        ? `${names} changed on the server since you edited it. Your value is kept: saving overwrites the server's.`
        : `${names} changed on the server since you edited them. Your values are kept: saving overwrites the server's.`;
    return {
      text: `${text.charAt(0).toUpperCase()}${text.slice(1)}`,
      tone: 'warning',
      retry: false,
      action: { label: keys.length === 1 ? "Take the server's value" : "Take the server's values", run: takeServerValues },
    };
  }

  /** Puts the server's values into the fields of `conflicts`, dropping the user's edits of those fields. */
  function takeServerValues(): void {
    if (plan.state.applying || conflicts.value.length === 0) return;
    const server = options.formFrom(details.value) as Record<string, unknown>;
    const target = form as unknown as Record<string, unknown>;
    for (const key of conflicts.value) target[key] = structuredClone(server[key]);
    conflicts.value = [];
    notice.value = null;
  }

  function failureText(error: unknown): string {
    const p = presentError(toCmdError(error));
    return p.detail ? `${p.message} (${p.detail})` : p.message;
  }

  /** Reads the object (again). See the composable's description for what `keepEdits` keeps. */
  async function fetchDetails(fetch: FetchOptions): Promise<void> {
    const token = latest.next();
    const keepEdits = fetch.keepEdits;
    const dropping = keepEdits && mode.value === 'drop';
    const keptCascade = cascade.value;
    if (!keepEdits) {
      mode.value = 'edit';
      unfinished.value = false;
      conflicts.value = [];
    }
    listError.value = null;
    if (!fetch.keepPlan) plan.clear();
    const blocking = !everLoaded || loadError.value !== null;
    if (current.value === null) {
      details.value = null;
      loadError.value = null;
      if (blocking) loading.value = true;
      else refreshing.value = true;
      try {
        await options.afterLoad?.(null);
      } catch (error: unknown) {
        // Picker lists are a convenience: the form works without them, and the editor says they are missing.
        listError.value = presentError(toCmdError(error)).message;
      }
      if (!latest.isCurrent(token)) return;
      loading.value = false;
      refreshing.value = false;
      everLoaded = true;
      const kept = keepEdits && dirty.value ? formValue() : null;
      resetForm(options.formFrom(null));
      if (kept) Object.assign(form as object, kept);
      if (!fetch.keepPlan) replanNow();
      return;
    }
    if (blocking) loading.value = true;
    else refreshing.value = true;
    loadError.value = null;
    const settle = (): void => {
      loading.value = false;
      refreshing.value = false;
    };
    const sessionId = props.session.id;
    try {
      const candidates: ObjectRef[] = [current.value];
      if (fetch.afterPartialRun) {
        // The steps may have renamed it already: look for it under the name it was being saved as, too.
        const target = options.identity(formValue(), details.value);
        if (JSON.stringify(target) !== JSON.stringify(current.value)) candidates.push(target);
      }
      let found: { object: ObjectRef; details: D } | null = null;
      let missingError: unknown = null;
      for (const candidate of candidates) {
        try {
          const described = await catalogDescribe(sessionId, candidate);
          if (!latest.isCurrent(token)) return;
          const picked = options.pick(described);
          if (typeof picked === 'string') {
            // Edited DDL that has not run lives in the sections: say it in a notice instead of replacing them.
            if (!blocking && ddlDirty.value) notice.value = { text: `${picked} The DDL you edited is kept.`, tone: 'danger', retry: true };
            else loadError.value = picked;
            return;
          }
          found = { object: candidate, details: picked };
          break;
        } catch (error: unknown) {
          if (!latest.isCurrent(token)) return;
          if (!isMissing(error)) throw error;
          missingError ??= error;
        }
      }
      if (found === null) {
        if (keepEdits && !blocking && ddlDirty.value) {
          // Gone, and the DDL section holds text that has not run (a script that dropped the object and then failed
          // leaves the only copy of its definition there): the sections stay, with the DDL and its error.
          const gone = displayName(candidates[0] as ObjectRef, props.session.engine);
          notice.value = {
            text: dirty.value ? `${gone} no longer exists. The DDL you edited and the form are kept.` : `${gone} no longer exists. The DDL you edited is kept.`,
            tone: 'warning',
            retry: true,
          };
          settle();
          return;
        }
        if (keepEdits && dirty.value) {
          // Gone (dropped, or renamed elsewhere): keep the typed form as a create form, so nothing typed is lost.
          const gone = candidates[0] as ObjectRef;
          const kept = formValue();
          current.value = null;
          details.value = null;
          mode.value = 'edit';
          conflicts.value = [];
          resetForm(options.formFrom(null));
          Object.assign(form as object, kept);
          notice.value = { text: `${displayName(gone, props.session.engine)} no longer exists. Saving creates it from the form.`, tone: 'warning', retry: false };
          settle();
          if (!fetch.keepPlan) replanNow();
          return;
        }
        throw missingError;
      }
      if (JSON.stringify(found.object) !== JSON.stringify(current.value)) {
        current.value = found.object;
        emit('opened', found.object);
      }
      details.value = found.details;
      try {
        await options.afterLoad?.(found.details);
      } catch (error: unknown) {
        listError.value = presentError(toCmdError(error)).message;
      }
      if (!latest.isCurrent(token)) return;
      everLoaded = true;
      // Taken now, not before the request: what the user typed while it ran counts too.
      const edits = keepEdits && dirty.value ? editedFields() : null;
      const before = JSON.parse(baseline.value) as Record<string, unknown>;
      const fresh = options.formFrom(found.details);
      resetForm(fresh);
      if (edits) {
        Object.assign(form as object, edits);
        const server = fresh as Record<string, unknown>;
        const kept: Record<string, unknown> = Object.fromEntries(Object.entries(edits));
        // An edit made on top of a value the server changed meanwhile: the edit stays, and the notice says so.
        // After a partial run the server changed through this save's own steps, which is no conflict.
        const fromBefore = conflicts.value.filter((key) => key in kept && stable(kept[key]) !== stable(server[key]));
        const now = fetch.afterPartialRun ? [] : changedOnServer(edits, before, fresh);
        conflicts.value = [...new Set([...fromBefore, ...now])];
      } else {
        conflicts.value = [];
      }
      notice.value = conflictNotice();
      if (props.intent === 'drop' && !dropOffered) {
        dropOffered = true;
        mode.value = 'drop';
      } else if (dropping) {
        cascade.value = keptCascade;
        mode.value = 'drop';
      }
      settle();
      if (keepEdits && !fetch.keepPlan) replanNow();
    } catch (error: unknown) {
      props.reportError?.(error, sessionId);
      if (!latest.isCurrent(token)) return;
      if (blocking || isMissing(error)) {
        loadError.value = failureText(error);
      } else {
        // The sections keep showing what was read before; say that it may be out of date.
        notice.value = { text: `Could not read it again: ${failureText(error)}`, tone: 'danger', retry: true };
      }
    } finally {
      if (latest.isCurrent(token)) settle();
    }
  }

  /**
   * Reads where an unfinished save stands (`options.unfinished`) and says it
   * in the notice. Resolves true while the save is unfinished: the form, the
   * object and the steps that did not run then stay as they are.
   */
  async function checkUnfinished(): Promise<boolean> {
    const read = options.unfinished;
    if (!read) return false;
    const token = latest.next();
    const action = { label: 'Plan the remaining steps', run: () => void plan.planRemaining() };
    refreshing.value = true;
    const sessionId = props.session.id;
    try {
      const text = await read(formValue(), details.value);
      if (!latest.isCurrent(token)) return true;
      if (text === null) {
        unfinished.value = false;
        return false;
      }
      unfinished.value = true;
      notice.value = { text, tone: 'warning', retry: false, action };
      return true;
    } catch (error: unknown) {
      props.reportError?.(error, sessionId);
      if (!latest.isCurrent(token)) return true;
      // Steps ran and the server cannot be read (a lost connection): nothing is given up, the remaining steps stay.
      unfinished.value = true;
      notice.value = {
        text: `The save stopped part way, and what exists on the server now could not be read: ${failureText(error)}`,
        tone: 'danger',
        retry: true,
        action,
      };
      return true;
    } finally {
      if (latest.isCurrent(token)) refreshing.value = false;
    }
  }

  /** Some steps of a save ran and a later one did not: re-read the object, keeping the form and the plan. */
  async function afterPartialRun(): Promise<void> {
    if (mode.value === 'edit' && (await checkUnfinished())) return;
    await fetchDetails({ keepEdits: true, keepPlan: true, afterPartialRun: true });
  }

  /** Reads the object again, keeping what the user typed (see the composable's description). */
  async function load(): Promise<void> {
    if (unfinished.value && (await checkUnfinished())) return;
    await fetchDetails({ keepEdits: true });
  }

  /** A new session for the same tab: read the object again on it; an unfinished save plans its remaining steps there (plans belong to a session). */
  async function onNewSession(): Promise<void> {
    if (unfinished.value && (await checkUnfinished())) {
      await plan.planRemaining();
      return;
    }
    await fetchDetails({ keepEdits: true });
  }

  let timer: ReturnType<typeof setTimeout> | null = null;
  function schedulePlan(): void {
    // The remaining steps of an unfinished save are the plan, whatever the form holds.
    if (unfinished.value) return;
    if (timer !== null) clearTimeout(timer);
    // Until the new plan is there, Apply must not run the previous one.
    plan.markStale();
    timer = setTimeout(() => {
      timer = null;
      void planNow();
    }, PLAN_DELAY_MS);
  }

  /** Plans at once (after a reload the form may be unchanged, so no edit schedules it). */
  function replanNow(): void {
    if (timer !== null) clearTimeout(timer);
    timer = null;
    void planNow();
  }

  /**
   * Plans when something other than the form let a waiting plan through (an editor's check of the server came
   * back). While an edit's plan is on its way (the debounce runs) that plan covers it; nothing is planned twice.
   */
  function planIfDue(): void {
    if (timer === null) void planNow();
  }

  async function planNow(): Promise<void> {
    if (plan.state.applying) {
      replanWhenIdle = true;
      return;
    }
    if (unfinished.value) return;
    if (mode.value === 'drop') {
      const object = current.value;
      const change = object ? (options.dropChange ?? ((o, c) => buildDrop(props.session.engine, o, c)))(object, cascade.value) : null;
      await plan.plan(change ? [{ kind: 'edit', change }] : []);
      return;
    }
    // While the object is read (again) the details are about to change: plan against the new ones.
    if (!dirty.value || Object.keys(errors.value).length > 0 || blockedReason.value !== null || loading.value || refreshing.value) {
      await plan.plan([]);
      return;
    }
    await plan.plan(options.build(formValue(), details.value));
  }

  watch(() => snapshot(), schedulePlan);
  watch(
    () => dirty.value || ddlDirty.value,
    (value) => props.reportDirty?.(value),
    { immediate: true },
  );
  watch([mode, cascade], () => void planNow());
  watch(
    () => plan.state.applying,
    (applying) => {
      if (applying || !replanWhenIdle) return;
      replanWhenIdle = false;
      // A finished save reloads and plans by itself.
      if (!plan.state.applied) void planNow();
    },
  );
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
      const sessionId = props.session.id;
      try {
        next = await options.resolveIdentity(next, value, details.value);
      } catch (error: unknown) {
        props.reportError?.(error, sessionId);
      }
    }
    const before = current.value;
    current.value = next;
    notice.value = null;
    if (!before || JSON.stringify(before) !== JSON.stringify(next)) emit('opened', next);
    await fetchDetails({ keepEdits: false });
  }

  function revert(): void {
    if (plan.state.applying) return;
    if (unfinished.value) {
      // Gives up the rest of the save: the remaining steps go, and the editor shows what the server has.
      plan.clear();
      notice.value = null;
      void fetchDetails({ keepEdits: false });
      return;
    }
    if (details.value !== null || current.value === null) resetForm(options.formFrom(details.value));
    if (conflicts.value.length > 0) {
      conflicts.value = [];
      notice.value = null;
    }
    plan.clear();
  }

  function startDrop(): void {
    if (plan.state.applying || unfinished.value) return;
    cascade.value = false;
    mode.value = 'drop';
  }

  function cancelDrop(): void {
    if (plan.state.applying) return;
    mode.value = 'edit';
    void planNow();
  }

  watch(
    () => props.object,
    (object) => {
      if (JSON.stringify(object) === JSON.stringify(current.value)) return;
      current.value = object;
      void fetchDetails({ keepEdits: false });
    },
  );
  // A new session for the same tab (reconnect, or "run from another database"): read the object again on it,
  // keeping what the user typed and the drop in progress.
  watch(
    () => props.session.id,
    () => void onNewSession(),
  );

  return {
    current,
    details,
    form,
    errors,
    dirty,
    creating,
    loading,
    refreshing,
    loadError,
    listError,
    notice,
    blockedReason,
    unfinished,
    mode,
    cascade,
    plan,
    busy,
    load,
    revert,
    onApplied,
    startDrop,
    cancelDrop,
    planNow,
    planIfDue,
  };
}
