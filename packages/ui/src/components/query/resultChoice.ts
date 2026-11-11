import { toRaw } from 'vue';
import type { RunResult } from '../../composables/useTabs';

/** A result set by its position among the run's row results, or the message log. */
export type ResultChoice = number | 'messages';

/**
 * The result set the user picked for each run, kept with the run (a run
 * belongs to its tab's state) rather than with the panel that shows it, so
 * a results panel that is mounted again opens on the same choice. Held
 * weakly: a run that is replaced or whose tab closes takes its entry along.
 */
const chosen = new WeakMap<RunResult, ResultChoice>();

/**
 * What a run opens on: the first result set when it returned rows and did
 * not fail, otherwise the message log (a failed run opens on its error, even
 * when earlier statements returned rows).
 */
export function defaultChoice(run: RunResult | null): ResultChoice {
  const hasRows = (run?.output?.results ?? []).some((r) => r.kind === 'rows');
  return run?.output && hasRows && !run.error ? 0 : 'messages';
}

/** The result set to show for `run`: the one the user picked, else the default. */
export function resultChoice(run: RunResult | null): ResultChoice {
  if (!run) return 'messages';
  return chosen.get(toRaw(run)) ?? defaultChoice(run);
}

/** Remembers the result set the user picked for `run`. */
export function rememberChoice(run: RunResult | null, choice: ResultChoice): void {
  if (run) chosen.set(toRaw(run), choice);
}
