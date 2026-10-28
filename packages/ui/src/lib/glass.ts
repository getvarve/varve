import type { AppInfo } from '../backend';

const REDUCED_TRANSPARENCY = '(prefers-reduced-transparency: reduce)';

/** The native materials glass.css is made for; anything else (an older backend) stays opaque. */
const MATERIALS: ReadonlySet<string> = new Set(['liquidGlass', 'vibrancy', 'mica']);

/**
 * Marks `root` with the window's native glass, which turns on glass.css. Off when the
 * backend applied none (Linux, or an effect that failed) or when the user asks for less
 * transparency: then the UI keeps its opaque design.
 */
export function applyGlass(root: HTMLElement, glass: AppInfo['glass'] | undefined, reducedTransparency: boolean): void {
  if (glass !== undefined && MATERIALS.has(glass) && !reducedTransparency) {
    root.dataset.glass = glass;
  } else {
    delete root.dataset.glass;
  }
}

/**
 * Applies `glass` to the document and follows the reduced-transparency setting while the
 * app runs. A webview that does not know the media feature reports no preference.
 */
export function watchGlass(glass: AppInfo['glass'] | undefined, root: HTMLElement = document.documentElement): void {
  const query = typeof window.matchMedia === 'function' ? window.matchMedia(REDUCED_TRANSPARENCY) : null;
  const update = () => applyGlass(root, glass, query?.matches ?? false);
  update();
  query?.addEventListener('change', update);
}
