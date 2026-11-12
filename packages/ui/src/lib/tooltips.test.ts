import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { installTooltips, placeTip, takeTip, TIP_DELAY_MS, tipTarget } from './tooltips';

describe('takeTip', () => {
  it('moves the title out of the system\'s reach and keeps it readable', () => {
    const el = document.createElement('button');
    el.setAttribute('title', 'Collapse all');
    expect(takeTip(el)).toBe('Collapse all');
    expect(el.hasAttribute('title')).toBe(false);
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
    expect(document.getElementById('a')?.hasAttribute('title')).toBe(false);
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
});
