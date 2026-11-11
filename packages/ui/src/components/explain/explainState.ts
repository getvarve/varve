import { reactive, ref, shallowRef } from 'vue';
import type { Ref, ShallowRef } from 'vue';
import type { CmdError } from '../../api/errors';
import type { ExplainCapabilities, ExplainFormat, ExplainOptions, Plan } from '../../api/explain';
import type { HighlightMode } from '../../lib/explainPlan';
import type { ExplainConfirmation } from '../../lib/explainRun';

/** What the view shows below the toolbar. */
export type ExplainResult =
  | { kind: 'plan'; plan: Plan; options: ExplainOptions; at: number }
  | { kind: 'error'; error: CmdError; analyze: boolean }
  | { kind: 'cancelled'; analyze: boolean }
  | { kind: 'declined' };

export type ExplainPlanView = 'visual' | 'timeline' | 'text' | 'raw';

/** The option checkboxes and format of the toolbar. */
export interface ExplainOptionState {
  buffers: boolean;
  verbose: boolean;
  settings: boolean;
  wal: boolean;
  format: ExplainFormat | null;
}

/**
 * Everything an explain view holds for its tab: the server's options, the
 * last result (an EXPLAIN ANALYZE took real time and may have needed a
 * confirmation), a run in progress with its confirmation question, and how
 * the plan is shown. A run writes here rather than into the component, so a
 * view that is mounted again for the same tab shows the same plan and the
 * running explain with its Cancel button, and does not explain again.
 */
export interface ExplainState {
  caps: ShallowRef<ExplainCapabilities | null>;
  capsError: ShallowRef<CmdError | null>;
  options: ExplainOptionState;
  showOptions: Ref<boolean>;
  result: ShallowRef<ExplainResult | null>;
  running: Ref<'explain' | 'analyze' | null>;
  cancelling: Ref<boolean>;
  confirmRequest: ShallowRef<ExplainConfirmation | null>;
  /** Answers the open confirmation question; `null` when none is open. */
  answerConfirm: ((yes: boolean) => void) | null;
  /** The shown plan was made for an earlier statement text. */
  stale: Ref<boolean>;
  view: Ref<ExplainPlanView>;
  highlight: Ref<HighlightMode>;
  selected: Ref<number | null>;
  /** The view's `autoRun` already ran for this tab; a view mounted again does not run it twice. */
  autoRan: boolean;
}

export function newExplainState(): ExplainState {
  return {
    caps: shallowRef(null),
    capsError: shallowRef(null),
    options: reactive({ buffers: true, verbose: false, settings: false, wal: false, format: null }),
    showOptions: ref(false),
    result: shallowRef(null),
    running: ref(null),
    cancelling: ref(false),
    confirmRequest: shallowRef(null),
    answerConfirm: null,
    stale: ref(false),
    view: ref('visual'),
    highlight: ref('share'),
    selected: ref(null),
    autoRan: false,
  };
}

const states = new Map<string, ExplainState>();

/**
 * The state of the explain view in workspace tab `tabId`, created on first
 * use. Entries of tabs not in `openTabIds` are dropped first (a closed tab
 * never comes back with its old plan). Without a tab the view gets state of
 * its own.
 */
export function explainState(tabId: string | null, openTabIds: Iterable<string>): ExplainState {
  const open = new Set(openTabIds);
  for (const id of [...states.keys()]) {
    if (!open.has(id)) states.delete(id);
  }
  if (tabId === null) return newExplainState();
  let state = states.get(tabId);
  if (!state) {
    state = newExplainState();
    states.set(tabId, state);
  }
  return state;
}
