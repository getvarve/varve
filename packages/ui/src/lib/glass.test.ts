import { afterEach, describe, expect, it, vi } from 'vitest';
import { applyGlass, watchGlass } from './glass';

function stubReducedTransparency(matches: boolean) {
  const listeners: Array<() => void> = [];
  const query = {
    matches,
    addEventListener: (_: string, listener: () => void) => listeners.push(listener),
  };
  vi.stubGlobal('matchMedia', vi.fn(() => query));
  return {
    set(next: boolean) {
      query.matches = next;
      listeners.forEach((listener) => listener());
    },
  };
}

describe('applyGlass', () => {
  it('marks the root with the native glass', () => {
    const root = document.createElement('html');
    applyGlass(root, 'liquidGlass', false);
    expect(root.dataset.glass).toBe('liquidGlass');
  });

  it('keeps the opaque design without native glass', () => {
    const root = document.createElement('html');
    root.dataset.glass = 'mica';
    applyGlass(root, 'none', false);
    expect(root.dataset.glass).toBeUndefined();
  });

  it('keeps the opaque design for a material it does not know', () => {
    const root = document.createElement('html');
    applyGlass(root, undefined, false);
    expect(root.dataset.glass).toBeUndefined();
    applyGlass(root, 'acrylic' as never, false);
    expect(root.dataset.glass).toBeUndefined();
  });

  it('keeps the opaque design when the user asks for less transparency', () => {
    const root = document.createElement('html');
    applyGlass(root, 'vibrancy', true);
    expect(root.dataset.glass).toBeUndefined();
  });
});

describe('watchGlass', () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('follows the reduced-transparency setting while the app runs', () => {
    const setting = stubReducedTransparency(false);
    const root = document.createElement('html');
    watchGlass('liquidGlass', root);
    expect(root.dataset.glass).toBe('liquidGlass');

    setting.set(true);
    expect(root.dataset.glass).toBeUndefined();

    setting.set(false);
    expect(root.dataset.glass).toBe('liquidGlass');
  });

  it('turns glass on in a webview without matchMedia', () => {
    vi.stubGlobal('matchMedia', undefined);
    const root = document.createElement('html');
    watchGlass('mica', root);
    expect(root.dataset.glass).toBe('mica');
  });
});
