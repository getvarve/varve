import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { hideTooltip, installTooltips, placeTip, takeTip, TIP_DELAY_MS, tipTarget } from './tooltips';

describe('takeTip', () => {
  it('moves the title out of the system\'s reach and keeps it readable', () => {
    const el = document.createElement('button');
    el.setAttribute('title', 'Collapse all');
    expect(takeTip(el)).toBe('Collapse all');
    // The title stays, empty: no system tooltip, and a template that removes it later can be seen doing so.
    expect(el.getAttribute('title')).toBe('');
    expect(el.getAttribute('data-tip')).toBe('Collapse all');
    expect(el.getAttribute('aria-description')).toBe('Collapse all');
    // Asked again, the text is still there.
    expect(takeTip(el)).toBe('Collapse all');
  });

  it('follows a title the template puts back with new text', () => {
    const el = document.createElement('span');
    el.setAttribute('title', 'Connected');
    takeTip(el);
    el.setAttribute('title', 'Not connected');
    expect(takeTip(el)).toBe('Not connected');
    expect(el.getAttribute('aria-description')).toBe('Not connected');
  });

  it('leaves a description the template wrote itself', () => {
    const el = document.createElement('span');
    el.setAttribute('aria-description', 'Written by hand');
    el.setAttribute('title', 'Short');
    expect(takeTip(el)).toBe('Short');
    expect(el.getAttribute('aria-description')).toBe('Written by hand');
  });

  it('has no tooltip for an empty title', () => {
    const el = document.createElement('span');
    el.setAttribute('title', '  ');
    expect(takeTip(el)).toBeNull();
    expect(el.hasAttribute('data-tip')).toBe(false);
    expect(takeTip(document.createElement('span'))).toBeNull();
  });
});

describe('takeTip after the template took the title away', () => {
  it('forgets the text and the description it added when the title is removed', () => {
    const el = document.createElement('button');
    el.setAttribute('title', 'Type the connection name first');
    takeTip(el);
    el.removeAttribute('title');
    expect(takeTip(el)).toBeNull();
    expect(el.hasAttribute('data-tip')).toBe(false);
    expect(el.hasAttribute('aria-description')).toBe(false);
    expect(el.hasAttribute('data-tip-described')).toBe(false);
  });

  it('keeps a description the template wrote itself', () => {
    const el = document.createElement('button');
    el.setAttribute('aria-description', 'Written by hand');
    el.setAttribute('title', 'Short');
    takeTip(el);
    el.removeAttribute('title');
    expect(takeTip(el)).toBeNull();
    expect(el.getAttribute('aria-description')).toBe('Written by hand');
  });
});

describe('tipTarget', () => {
  it('finds the closest element with a tooltip', () => {
    const outer = document.createElement('button');
    outer.setAttribute('title', 'Close');
    const inner = document.createElement('svg');
    outer.append(inner);
    expect(tipTarget(inner)).toBe(outer);
    expect(tipTarget(document.createElement('div'))).toBeNull();
    expect(tipTarget(null)).toBeNull();
  });
});

describe('placeTip', () => {
  const viewport = { width: 1000, height: 600 };
  it('goes under the element, centred', () => {
    expect(placeTip({ left: 100, top: 100, width: 40, height: 20 }, { width: 80, height: 24 }, viewport)).toEqual({ left: 80, top: 126 });
  });
  it('goes above when there is no room under it', () => {
    expect(placeTip({ left: 100, top: 570, width: 40, height: 20 }, { width: 80, height: 24 }, viewport)).toEqual({ left: 80, top: 540 });
  });
  it('stays inside the window', () => {
    expect(placeTip({ left: 0, top: 100, width: 20, height: 20 }, { width: 200, height: 24 }, viewport).left).toBe(8);
    expect(placeTip({ left: 980, top: 100, width: 20, height: 20 }, { width: 200, height: 24 }, viewport).left).toBe(792);
  });
});

describe('installTooltips', () => {
  let remove: () => void;
  beforeEach(() => {
    vi.useFakeTimers();
    document.body.innerHTML = '<button id="a" title="Collapse all">x</button><button id="b">y</button>';
    remove = installTooltips(document);
  });
  afterEach(() => {
    remove();
    vi.useRealTimers();
  });
  const tip = () => document.querySelector<HTMLElement>('.tip');
  const over = (id: string) => document.getElementById(id)?.dispatchEvent(new MouseEvent('mouseover', { bubbles: true }));

  it('shows after the pointer rests, not before', () => {
    over('a');
    expect(document.getElementById('a')?.getAttribute('title')).toBe('');
    vi.advanceTimersByTime(TIP_DELAY_MS - 1);
    expect(tip()).toBeNull();
    vi.advanceTimersByTime(1);
    expect(tip()?.hidden).toBe(false);
    expect(tip()?.textContent).toBe('Collapse all');
    expect(tip()?.getAttribute('role')).toBe('tooltip');
  });

  it('hides when the pointer leaves, on a press and on Escape', () => {
    over('a');
    vi.advanceTimersByTime(TIP_DELAY_MS);
    document.getElementById('a')?.dispatchEvent(new MouseEvent('mouseout', { bubbles: true }));
    expect(tip()?.hidden).toBe(true);

    over('a');
    vi.advanceTimersByTime(TIP_DELAY_MS);
    expect(tip()?.hidden).toBe(false);
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));
    expect(tip()?.hidden).toBe(true);

    over('a');
    document.getElementById('a')?.dispatchEvent(new Event('pointerdown', { bubbles: true }));
    vi.advanceTimersByTime(TIP_DELAY_MS);
    expect(tip()?.hidden).toBe(true);
  });

  it('shows nothing for an element without a tooltip, and nothing after it is turned off', () => {
    over('b');
    vi.advanceTimersByTime(TIP_DELAY_MS);
    expect(tip()).toBeNull();
    remove();
    document.getElementById('b')?.setAttribute('title', 'Late');
    over('b');
    vi.advanceTimersByTime(TIP_DELAY_MS);
    expect(tip()).toBeNull();
  });

  const a = (): HTMLElement => document.getElementById('a') as HTMLElement;
  /** Lets the engine see the changes a template made (observers report in a microtask). */
  const rendered = async (): Promise<void> => {
    await Promise.resolve();
    await Promise.resolve();
  };
  const key = (name: string) => document.dispatchEvent(new KeyboardEvent('keydown', { key: name }));

  it('drops the tooltip and its description when the template removes the title while it shows', async () => {
    over('a');
    vi.advanceTimersByTime(TIP_DELAY_MS);
    expect(tip()?.hidden).toBe(false);
    // What Vue does for `:title="undefined"`: empty the property, then remove the attribute.
    a().title = '';
    a().removeAttribute('title');
    await rendered();
    expect(tip()?.hidden).toBe(true);
    expect(a().hasAttribute('data-tip')).toBe(false);
    expect(a().hasAttribute('aria-description')).toBe(false);
    // Nothing shows on the next visit either.
    a().dispatchEvent(new MouseEvent('mouseout', { bubbles: true }));
    over('a');
    vi.advanceTimersByTime(TIP_DELAY_MS);
    expect(tip()?.hidden).toBe(true);
  });

  it('drops them as well when the title is removed while the tooltip waits or after it hid', async () => {
    over('a');
    a().removeAttribute('title');
    await rendered();
    vi.advanceTimersByTime(TIP_DELAY_MS);
    expect(tip()).toBeNull();
    expect(a().hasAttribute('aria-description')).toBe(false);

    a().setAttribute('title', 'Collapse all');
    over('a');
    vi.advanceTimersByTime(TIP_DELAY_MS);
    a().dispatchEvent(new MouseEvent('mouseout', { bubbles: true }));
    expect(a().getAttribute('aria-description')).toBe('Collapse all');
    a().removeAttribute('title');
    await rendered();
    expect(a().hasAttribute('data-tip')).toBe(false);
    expect(a().hasAttribute('aria-description')).toBe(false);
  });

  it('follows a title that changes while the tooltip shows, and afterwards', async () => {
    over('a');
    vi.advanceTimersByTime(TIP_DELAY_MS);
    a().setAttribute('title', 'Expand all');
    await rendered();
    expect(tip()?.hidden).toBe(false);
    expect(tip()?.textContent).toBe('Expand all');
    expect(a().getAttribute('aria-description')).toBe('Expand all');
    expect(a().getAttribute('title')).toBe('');

    a().dispatchEvent(new MouseEvent('mouseout', { bubbles: true }));
    a().setAttribute('title', 'Collapse all');
    await rendered();
    // Not hovered, and still the description a screen reader gets is the new text.
    expect(a().getAttribute('aria-description')).toBe('Collapse all');
    expect(tip()?.hidden).toBe(true);
  });

  it('hides when its element leaves the document or is hidden', async () => {
    over('a');
    vi.advanceTimersByTime(TIP_DELAY_MS);
    expect(tip()?.hidden).toBe(false);
    a().remove();
    await rendered();
    expect(tip()?.hidden).toBe(true);

    document.body.insertAdjacentHTML('afterbegin', '<div id="page"><button id="c" title="Close tab">c</button></div>');
    over('c');
    vi.advanceTimersByTime(TIP_DELAY_MS);
    expect(tip()?.hidden).toBe(false);
    (document.getElementById('page') as HTMLElement).style.display = 'none';
    await rendered();
    expect(tip()?.hidden).toBe(true);
  });

  it('hides on any key press and when asked to, also while it waits', () => {
    over('a');
    vi.advanceTimersByTime(TIP_DELAY_MS);
    key('Enter');
    expect(tip()?.hidden).toBe(true);

    a().dispatchEvent(new MouseEvent('mouseout', { bubbles: true }));
    over('a');
    vi.advanceTimersByTime(TIP_DELAY_MS);
    expect(tip()?.hidden).toBe(false);
    hideTooltip(document);
    expect(tip()?.hidden).toBe(true);

    a().dispatchEvent(new MouseEvent('mouseout', { bubbles: true }));
    over('a');
    hideTooltip(document);
    vi.advanceTimersByTime(TIP_DELAY_MS);
    expect(tip()?.hidden).toBe(true);
  });

  it('shows at once on keyboard focus, not on focus that follows a press of the pointer', () => {
    key('Tab');
    a().focus();
    expect(tip()?.hidden).toBe(false);
    expect(tip()?.textContent).toBe('Collapse all');
    a().blur();
    expect(tip()?.hidden).toBe(true);

    document.body.insertAdjacentHTML('beforeend', '<input id="field" title="Filter by name">');
    const field = document.getElementById('field') as HTMLElement;
    field.dispatchEvent(new Event('pointerdown', { bubbles: true }));
    field.focus();
    expect(tip()?.hidden).toBe(true);
    // Typing in it does not bring the tooltip either.
    key('a');
    expect(tip()?.hidden).toBe(true);
  });

  it('does not show the tooltip of a row for a control focused inside it', () => {
    document.body.insertAdjacentHTML('beforeend', '<div title="Row"><input id="inner"></div>');
    key('Tab');
    (document.getElementById('inner') as HTMLElement).focus();
    expect(tip()).toBeNull();
  });

  it('shows for a disabled button, which only the element the pointer left tells about', () => {
    document.body.insertAdjacentHTML('beforeend', '<button id="off" title="Type the connection name first" disabled>Run</button>');
    const off = document.getElementById('off') as HTMLElement;
    const elementFromPoint = vi.fn(() => off as Element | null);
    Object.defineProperty(document, 'elementFromPoint', { configurable: true, value: elementFromPoint });
    try {
      // No mouseover reaches the disabled button; the neighbour reports the pointer leaving.
      document.getElementById('b')?.dispatchEvent(new MouseEvent('mouseout', { bubbles: true, clientX: 40, clientY: 12 }));
      expect(elementFromPoint).toHaveBeenCalledWith(40, 12);
      vi.advanceTimersByTime(TIP_DELAY_MS);
      expect(tip()?.hidden).toBe(false);
      expect(tip()?.textContent).toBe('Type the connection name first');
      // The next element that does get events ends it.
      elementFromPoint.mockReturnValue(document.getElementById('b'));
      over('b');
      expect(tip()?.hidden).toBe(true);
    } finally {
      Reflect.deleteProperty(document, 'elementFromPoint');
    }
  });

  it('gives the titles back when it is turned off', () => {
    over('a');
    vi.advanceTimersByTime(TIP_DELAY_MS);
    remove();
    expect(a().getAttribute('title')).toBe('Collapse all');
    expect(a().hasAttribute('data-tip')).toBe(false);
    expect(a().hasAttribute('aria-description')).toBe(false);
  });
});
