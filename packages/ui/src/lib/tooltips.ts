/**
 * The app's tooltips. Every element with a `title` gets the app's own tooltip instead of the system's: dark, small,
 * shown 400 ms after the pointer rests on the element and at once when the keyboard focuses it. So a tooltip is
 * written as a plain `title` (or `:title`) in a template, and looks the same everywhere.
 *
 * When the pointer or the keyboard reaches an element, its `title` text moves to `data-tip` and to
 * `aria-description` (so assistive technology still reads it), and `title` itself is left empty: an empty title
 * shows no system tooltip, and it keeps the attribute in place, so every later change a template makes to it can be
 * seen. From then on the engine follows that element's `title`: a new text is taken over again (and replaces the
 * tooltip when it is showing), and a title the template removes or empties takes `data-tip`, the description the
 * engine added and the tooltip with it.
 */

/** How long the pointer rests on an element before its tooltip shows. */
export const TIP_DELAY_MS = 400;

/** Space between the element and its tooltip, and between the tooltip and the window's edge. */
const GAP = 6;
const EDGE = 8;

/** Forgets an element's tooltip: the text and the description the engine added. Its `title` is not touched. */
export function dropTip(el: Element): void {
  el.removeAttribute('data-tip');
  if (!el.hasAttribute('data-tip-described')) return;
  el.removeAttribute('aria-description');
  el.removeAttribute('data-tip-described');
}

/**
 * The text of an element's tooltip. A `title` with text is taken over: the text moves to `data-tip` and the
 * description, and `title` stays behind empty. An empty `title` next to `data-tip` is that state, so the text is
 * still there. Anything else (no `title`, or an empty one the template wrote) means no tooltip, and what an earlier
 * title left behind is removed.
 */
export function takeTip(el: Element): string | null {
  const title = el.getAttribute('title');
  if (title !== null && title.trim() !== '') {
    el.setAttribute('title', '');
    el.setAttribute('data-tip', title);
    // A description the template wrote itself stays; one that came from an earlier title follows the new title.
    if (!el.hasAttribute('aria-description') || el.hasAttribute('data-tip-described')) {
      el.setAttribute('aria-description', title);
      el.setAttribute('data-tip-described', '');
    }
    return title;
  }
  const tip = el.getAttribute('data-tip');
  if (title === '' && tip !== null && tip.trim() !== '') return tip;
  dropTip(el);
  return null;
}

/** The closest element, from `start` up, that has a tooltip. */
export function tipTarget(start: EventTarget | null): Element | null {
  return start instanceof Element ? start.closest('[title], [data-tip]') : null;
}

/** Whether the element itself (not one around it) has a tooltip. */
function hasOwnTip(el: Element): boolean {
  return el.hasAttribute('title') || el.hasAttribute('data-tip');
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

/** The function that hides the tooltip of each document with tooltips turned on. */
const hiders = new Map<Document, () => void>();

/** Hides the tooltip that is showing or waiting, for example when the page under it changes. */
export function hideTooltip(doc: Document = document): void {
  hiders.get(doc)?.();
}

/**
 * Turns on the app's tooltips for a document. Returns the function that turns them off again.
 */
export function installTooltips(doc: Document = document): () => void {
  const view: Window | null = doc.defaultView;
  if (view === null) return () => undefined;
  const win: Window = view;
  let box: HTMLElement | null = null;
  /** The element whose tooltip is waiting or showing. */
  let active: Element | null = null;
  let shown = false;
  let timer: ReturnType<typeof setTimeout> | null = null;
  /**
   * Whether the last input came from the keyboard. A tooltip shows on focus only then: a click focuses too (and a
   * text field matches `:focus-visible` after a click), with the pointer already on the element.
   */
  let keyboard = false;

  /** Follows the `title` of every element whose title was taken over; it holds them weakly. */
  const titles = new MutationObserver((records) => titlesChanged(records));
  const followed = new WeakSet<Element>();
  /** While a tooltip waits or shows: sees its element leave the document or get hidden. */
  const surroundings = new MutationObserver(() => {
    if (active && !present(active)) hide();
  });

  /** Whether the element is in the document and not hidden by itself or by anything around it. */
  function present(el: Element): boolean {
    if (!el.isConnected) return false;
    for (let at: Element | null = el; at; at = at.parentElement) {
      if (win.getComputedStyle(at).display === 'none') return false;
    }
    return win.getComputedStyle(el).visibility !== 'hidden';
  }

  function hide(): void {
    if (timer !== null) clearTimeout(timer);
    timer = null;
    if (active) surroundings.disconnect();
    active = null;
    shown = false;
    if (box) box.hidden = true;
  }

  /**
   * Takes over an element's title. The engine's own writes are taken out of the observer's queue right away, so a
   * record that reaches `titlesChanged` is always a change someone else made; changes that were already waiting
   * are handled first, so they are not lost with them.
   */
  function take(el: Element): string | null {
    titlesChanged(titles.takeRecords());
    const text = takeTip(el);
    titles.takeRecords();
    if (text !== null && !followed.has(el)) {
      followed.add(el);
      titles.observe(el, { attributes: true, attributeFilter: ['title'] });
    }
    return text;
  }

  /** A template (or other code) changed the title of an element the engine follows. */
  function titlesChanged(records: readonly MutationRecord[]): void {
    const changed = new Set<Element>();
    for (const record of records) if (record.target instanceof Element) changed.add(record.target);
    for (const el of changed) {
      const title = el.getAttribute('title');
      if (title === null || title.trim() === '') {
        // Removed or emptied: the element has no tooltip any more.
        dropTip(el);
        if (el === active) hide();
        continue;
      }
      const text = takeTip(el);
      titles.takeRecords();
      if (text !== null && el === active && shown) render(el, text);
    }
  }

  function render(el: Element, text: string): void {
    if (!box) {
      box = doc.createElement('div');
      box.className = 'tip';
      box.setAttribute('role', 'tooltip');
      doc.body.append(box);
    }
    box.textContent = text;
    box.hidden = false;
    // Measured at the window's corner: at its last place the box would be squeezed between there and the edge.
    box.style.left = '0px';
    box.style.top = '0px';
    const at = el.getBoundingClientRect();
    const size = box.getBoundingClientRect();
    const { left, top } = placeTip(at, size, { width: win.innerWidth, height: win.innerHeight });
    box.style.left = `${left}px`;
    box.style.top = `${top}px`;
  }

  /** Makes `el` the element whose tooltip waits or shows. */
  function activate(el: Element): void {
    active = el;
    surroundings.observe(doc.body, {
      childList: true,
      subtree: true,
      attributes: true,
      attributeFilter: ['style', 'class', 'hidden'],
    });
  }

  function show(el: Element): void {
    timer = null;
    const text = take(el);
    if (text === null || !present(el)) {
      hide();
      return;
    }
    if (active !== el) {
      hide();
      activate(el);
    }
    shown = true;
    render(el, text);
  }

  /** The pointer is on `el` now (`null`: on nothing with a tooltip). */
  function pointAt(el: Element | null): void {
    if (el !== null && el === active) return;
    hide();
    if (!el) return;
    // Take the title now, so the system's tooltip never shows while ours waits.
    if (take(el) === null) return;
    activate(el);
    timer = setTimeout(() => show(el), TIP_DELAY_MS);
  }

  function onOver(event: Event): void {
    pointAt(tipTarget(event.target));
  }

  /**
   * The pointer left an element. What it is on now is looked up at the pointer's place, not taken from an event:
   * a disabled button gets no mouse events in every engine (WebKit sends none, Chromium only in newer versions),
   * so entering one is only ever announced by this event on the element the pointer came from. That is how a
   * disabled control still explains why it is off.
   */
  function onOut(event: Event): void {
    const mouse = event as MouseEvent;
    const hit = typeof doc.elementFromPoint === 'function' ? doc.elementFromPoint(mouse.clientX, mouse.clientY) : null;
    pointAt(tipTarget(hit ?? mouse.relatedTarget));
  }

  function onFocus(event: Event): void {
    hide();
    const el = event.target;
    // Only the focused element's own tooltip: a field inside a titled row is not what the row's text describes.
    if (!keyboard || !(el instanceof Element) || !hasOwnTip(el)) return;
    show(el);
  }

  function onKey(): void {
    keyboard = true;
    hide();
  }

  function onPress(): void {
    keyboard = false;
    hide();
  }

  doc.addEventListener('mouseover', onOver);
  doc.addEventListener('mouseout', onOut);
  doc.documentElement.addEventListener('mouseleave', hide);
  doc.addEventListener('focusin', onFocus);
  doc.addEventListener('focusout', hide);
  doc.addEventListener('pointerdown', onPress, true);
  doc.addEventListener('keydown', onKey, true);
  doc.addEventListener('scroll', hide, true);
  win.addEventListener('blur', hide);
  hiders.set(doc, hide);
  return () => {
    hide();
    hiders.delete(doc);
    titles.disconnect();
    // Give back the titles that were taken over, so nothing is lost without the engine.
    for (const el of doc.querySelectorAll('[data-tip]')) {
      const tip = el.getAttribute('data-tip');
      if (tip !== null && el.getAttribute('title') === '') el.setAttribute('title', tip);
      dropTip(el);
    }
    doc.removeEventListener('mouseover', onOver);
    doc.removeEventListener('mouseout', onOut);
    doc.documentElement.removeEventListener('mouseleave', hide);
    doc.removeEventListener('focusin', onFocus);
    doc.removeEventListener('focusout', hide);
    doc.removeEventListener('pointerdown', onPress, true);
    doc.removeEventListener('keydown', onKey, true);
    doc.removeEventListener('scroll', hide, true);
    win.removeEventListener('blur', hide);
    box?.remove();
    box = null;
  };
}
