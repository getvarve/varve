import type { AppInfo } from '../backend';

const REDUCED_TRANSPARENCY = '(prefers-reduced-transparency: reduce)';

/** The materials that lie behind the whole window, which glass.css is made for; anything else stays opaque. */
const MATERIALS: ReadonlySet<string> = new Set(['vibrancy', 'mica']);

/**
 * Marks `root` with the window's native glass. A material behind the whole window (`data-glass`) turns on
 * glass.css, which makes every surface translucent. Liquid Glass (`data-glass-shapes`) lies only behind the shapes
 * the UI reports, so the window stays opaque and only those shapes turn clear. Neither is set when the backend
 * applied none (Linux, or an effect that failed) or when the user asks for less transparency: then the UI keeps
 * its opaque design.
 */
export function applyGlass(root: HTMLElement, glass: AppInfo['glass'] | undefined, reducedTransparency: boolean): void {
  if (glass !== undefined && MATERIALS.has(glass) && !reducedTransparency) {
    root.dataset.glass = glass;
  } else {
    delete root.dataset.glass;
  }
  if (glass === 'liquidGlass' && !reducedTransparency) {
    root.dataset.glassShapes = '';
  } else {
    delete root.dataset.glassShapes;
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
