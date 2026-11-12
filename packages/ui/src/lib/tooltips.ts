/**
 * The app's tooltips. Every element with a `title` gets the app's own tooltip instead of the system's: dark, small,
 * shown 400 ms after the pointer rests on the element and at once when the keyboard focuses it. So a tooltip is
 * written as a plain `title` (or `:title`) in a template, and looks the same everywhere.
 *
 * When a tooltip is about to show, the element's `title` moves to `data-tip` (so the system's own tooltip does not
 * show as well) and to `aria-description` (so assistive technology still reads it). A template that binds `:title`
 * puts the attribute back on its next render, and it is picked up again the next time.
 */

/** How long the pointer rests on an element before its tooltip shows. */
export const TIP_DELAY_MS = 400;

/** Space between the element and its tooltip, and between the tooltip and the window's edge. */
const GAP = 6;
const EDGE = 8;

/** The text of an element's tooltip, taking it over from `title` when that is still there. */
export function takeTip(el: Element): string | null {
  const title = el.getAttribute('title');
  if (title !== null) {
    el.removeAttribute('title');
    if (title.trim() === '') {
      el.removeAttribute('data-tip');
      return null;
    }
    el.setAttribute('data-tip', title);
    // A description the template wrote itself stays; one that came from an earlier title follows the new title.
    if (!el.hasAttribute('aria-description') || el.hasAttribute('data-tip-described')) {
      el.setAttribute('aria-description', title);
      el.setAttribute('data-tip-described', '');
    }
    return title;
  }
  const tip = el.getAttribute('data-tip');
  return tip !== null && tip.trim() !== '' ? tip : null;
}

/** The closest element, from `start` up, that has a tooltip. */
export function tipTarget(start: EventTarget | null): Element | null {
  return start instanceof Element ? start.closest('[title], [data-tip]') : null;
}

/** Where the tooltip goes: under the element, above it when there is no room, and inside the window. */
export function placeTip(
  target: { left: number; top: number; width: number; height: number },
  tip: { width: number; height: number },
  viewport: { width: number; height: number },
): { left: number; top: number } {
  const below = target.top + target.height + GAP;
  const above = target.top - GAP - tip.height;
  const top = below + tip.height + EDGE <= viewport.height || above < EDGE ? below : above;
  const centred = target.left + target.width / 2 - tip.width / 2;
  const left = Math.max(EDGE, Math.min(centred, viewport.width - tip.width - EDGE));
  return { left: Math.round(left), top: Math.round(top) };
}

/**
 * Turns on the app's tooltips for a document. Returns the function that turns them off again.
 */
export function installTooltips(doc: Document = document): () => void {
  const view: Window | null = doc.defaultView;
  if (view === null) return () => undefined;
  const win: Window = view;
  let box: HTMLElement | null = null;
  let shownFor: Element | null = null;
  let timer: ReturnType<typeof setTimeout> | null = null;

  function hide(): void {
    if (timer !== null) clearTimeout(timer);
    timer = null;
    shownFor = null;
    if (box) box.hidden = true;
  }

  function show(el: Element): void {
    const text = takeTip(el);
    if (text === null || !el.isConnected) {
      hide();
      return;
    }
    if (!box) {
      box = doc.createElement('div');
      box.className = 'tip';
      box.setAttribute('role', 'tooltip');
      doc.body.append(box);
    }
    box.textContent = text;
    box.hidden = false;
    const at = el.getBoundingClientRect();
    const size = box.getBoundingClientRect();
    const { left, top } = placeTip(at, size, { width: win.innerWidth, height: win.innerHeight });
    box.style.left = `${left}px`;
    box.style.top = `${top}px`;
    shownFor = el;
  }

  function onOver(event: Event): void {
    const el = tipTarget(event.target);
    if (el === shownFor) return;
    hide();
    if (!el) return;
    // Take the title now, so the system's tooltip never shows while ours waits.
    if (takeTip(el) === null) return;
    timer = setTimeout(() => show(el), TIP_DELAY_MS);
  }

  function onOut(event: Event): void {
    const to = (event as MouseEvent).relatedTarget;
    const current = shownFor ?? tipTarget(event.target);
    if (current && to instanceof Node && current.contains(to)) return;
    hide();
  }

  function onFocus(event: Event): void {
    const el = tipTarget(event.target);
    // Only for keyboard focus: a click focuses too, and the pointer is then already on the element.
    if (!el || !(event.target instanceof Element) || !event.target.matches(':focus-visible')) return;
    hide();
    show(el);
  }

  function onKey(event: KeyboardEvent): void {
    if (event.key === 'Escape') hide();
  }

  doc.addEventListener('mouseover', onOver);
  doc.addEventListener('mouseout', onOut);
  doc.addEventListener('focusin', onFocus);
  doc.addEventListener('focusout', hide);
  doc.addEventListener('pointerdown', hide, true);
  doc.addEventListener('keydown', onKey, true);
  doc.addEventListener('scroll', hide, true);
  win.addEventListener('blur', hide);
  return () => {
    hide();
    doc.removeEventListener('mouseover', onOver);
    doc.removeEventListener('mouseout', onOut);
    doc.removeEventListener('focusin', onFocus);
    doc.removeEventListener('focusout', hide);
    doc.removeEventListener('pointerdown', hide, true);
    doc.removeEventListener('keydown', onKey, true);
    doc.removeEventListener('scroll', hide, true);
    win.removeEventListener('blur', hide);
    box?.remove();
    box = null;
  };
}
