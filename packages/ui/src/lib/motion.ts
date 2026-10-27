/**
 * Small motion helpers for exits and cross-fades that CSS alone cannot do,
 * because Vue removes or replaces the element at once. Everything here only
 * animates opacity, so it is the same with reduced motion, and the copies it
 * leaves behind are inert: they never take clicks, focus or screen reader
 * attention, so no interaction waits for them.
 */

/** Attributes a copy must not carry: ids and names must stay unique, and the copy is not part of the accessibility tree. */
const COPY_STRIP = /^(id|name|form|for|role|tabindex|autofocus|data-autofocus|contenteditable|aria-.+)$/;

/** Class every copy gets; base.css stops CSS animations and transitions inside it. */
export const COPY_CLASS = 'motion-copy';

/** Extra time before a copy is removed when its animation never reports the end (a paused timeline). */
export const COPY_GRACE_MS = 250;

/** What a copy needs from the live element that the clone does not carry: scroll offsets and the state of a running entrance. */
interface CopyState {
  /** Scroll offsets by index in `[root, ...descendants]`. */
  scroll: Array<{ at: number; top: number; left: number }>;
  /** Opacity the root shows now (below 1 while an entrance fade runs). */
  opacity: number;
}

const copyStates = new WeakMap<HTMLElement, CopyState>();

/**
 * Empties every form control of a copy. Cloning copies the current value of
 * inputs, textareas and selects, which would keep a typed password (shown in
 * plain text when revealed) alive in the document after its dialog closed.
 * Inputs that held a secret go back to `type="password"`.
 */
function blankControls(copy: HTMLElement): void {
  for (const input of copy.querySelectorAll('input')) {
    const secret = input.type === 'password' || /password/i.test(input.autocomplete) || /password/i.test(input.getAttribute('autocomplete') ?? '');
    if (secret) input.type = 'password';
    if (input.type === 'checkbox' || input.type === 'radio') continue;
    input.removeAttribute('value');
    input.value = '';
  }
  for (const area of copy.querySelectorAll('textarea')) {
    area.value = '';
    area.textContent = '';
  }
  for (const select of copy.querySelectorAll('select')) {
    for (const option of select.options) option.removeAttribute('selected');
    select.selectedIndex = -1;
  }
}

/** Reads a motion token from tokens.css, with `fallback` when the stylesheet is not loaded. */
export function motionToken(name: string, fallback: string): string {
  const value = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return value === '' ? fallback : value;
}

/** Whether the Web Animations API is there to run a scripted animation on `el`. */
export function canAnimate(el: Element): boolean {
  return typeof el.animate === 'function';
}

/**
 * A non-interactive copy of `source` for a fading exit: ids, names, roles and
 * ARIA attributes removed, inert and hidden from assistive technology.
 */
export function inertCopy(source: HTMLElement): HTMLElement {
  const copy = source.cloneNode(true) as HTMLElement;
  const from = [source, ...source.querySelectorAll<HTMLElement>('*')];
  const to = [copy, ...copy.querySelectorAll<HTMLElement>('*')];
  for (const node of to) {
    for (const attr of [...node.attributes]) {
      if (COPY_STRIP.test(attr.name)) node.removeAttribute(attr.name);
    }
  }
  blankControls(copy);
  // A running CSS entrance (dialog scale, plan rise) would be cut off by the
  // copy, which turns CSS animations off: freeze what it shows now instead.
  if (typeof source.getAnimations === 'function') {
    const index = new Map(from.map((node, at) => [node, at]));
    for (const animation of source.getAnimations({ subtree: true })) {
      const effect = animation.effect;
      const target = effect && 'target' in effect ? (effect as KeyframeEffect).target : null;
      const at = target ? index.get(target as HTMLElement) : undefined;
      const node = at === undefined ? undefined : to[at];
      if (!target || !node) continue;
      const now = getComputedStyle(target);
      node.style.opacity = now.opacity;
      if (now.transform !== 'none') node.style.transform = now.transform;
    }
  }
  const scroll: CopyState['scroll'] = [];
  from.forEach((node, at) => {
    if (node.scrollTop !== 0 || node.scrollLeft !== 0) scroll.push({ at, top: node.scrollTop, left: node.scrollLeft });
  });
  const opacity = Number.parseFloat(copy.style.opacity);
  copyStates.set(copy, { scroll, opacity: Number.isNaN(opacity) ? 1 : opacity });
  copy.classList.add(COPY_CLASS);
  copy.setAttribute('inert', '');
  copy.setAttribute('aria-hidden', 'true');
  copy.style.pointerEvents = 'none';
  return copy;
}

/**
 * Fades `copy` (from `inertCopy`) out over `duration` ms and removes it.
 * The copy must already be in the document: the scroll offsets of the
 * original are put back first (they only apply once the copy has a layout),
 * and the fade starts at the opacity the original showed.
 */
export function fadeOutAndRemove(copy: HTMLElement, duration: number): void {
  const state = copyStates.get(copy);
  if (state && state.scroll.length > 0) {
    const nodes = [copy, ...copy.querySelectorAll<HTMLElement>('*')];
    for (const { at, top, left } of state.scroll) {
      const node = nodes[at];
      if (!node) continue;
      node.scrollTop = top;
      node.scrollLeft = left;
    }
  }
  let removed = false;
  const remove = (): void => {
    if (removed) return;
    removed = true;
    copy.remove();
  };
  if (!canAnimate(copy)) {
    remove();
    return;
  }
  const animation = copy.animate([{ opacity: state?.opacity ?? 1 }, { opacity: 0 }], { duration, easing: 'ease-in', fill: 'forwards' });
  animation.finished.then(remove, remove);
  window.setTimeout(remove, duration + COPY_GRACE_MS);
}

/**
 * Leaves a fading, inert copy of `source` in place of an element Vue is about
 * to remove. `place` puts the copy into the document. Without the Web
 * Animations API nothing is left behind.
 */
export function leaveCopy(source: HTMLElement, duration: number, place: (copy: HTMLElement) => void): void {
  if (!canAnimate(source)) return;
  const copy = inertCopy(source);
  place(copy);
  fadeOutAndRemove(copy, duration);
}

/** Fades `el` in from transparent over `duration` ms with the `--ease-out` token. */
export function fadeIn(el: HTMLElement, duration: number): void {
  if (!canAnimate(el)) return;
  el.animate([{ opacity: 0 }, { opacity: 1 }], { duration, easing: motionToken('--ease-out', 'ease-out') });
}

const seen = new Set<string>();

/** True the first time it is called with `key` in this window, false after that. */
export function firstTime(key: string): boolean {
  if (seen.has(key)) return false;
  seen.add(key);
  return true;
}
