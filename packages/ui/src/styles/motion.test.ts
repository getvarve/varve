/**
 * Motion rules that can be checked from the source: the motion tokens exist,
 * every stylesheet that animates also says what happens with reduced motion,
 * and the screens that must stay still have no motion at all.
 */
const raw = import.meta.glob<string>(['../**/*.vue', '../**/*.css'], { query: '?raw', import: 'default', eager: true });
/** Source text by path relative to src, for example `components/common/Modal.vue`. */
// Vite keys files of this directory as `./name`, the rest as `../dir/name`.
const sources = new Map(Object.entries(raw).map(([path, text]) => [path.replace(/^\.\//, 'styles/').replace(/^\.\.\//, ''), text]));

function source(path: string): string {
  const text = sources.get(path);
  if (text === undefined) throw new Error(`No source at ${path}`);
  return text;
}

/** Paths of the sources at `path` (a file) or below it (a directory). */
function under(path: string): string[] {
  return [...sources.keys()].filter((p) => p === path || p.startsWith(`${path}/`));
}

/** The CSS of a file: the whole file for .css, the <style> blocks for .vue. */
function css(path: string): string {
  const text = source(path);
  if (path.endsWith('.css')) return text;
  return [...text.matchAll(/<style[^>]*>([\s\S]*?)<\/style>/g)].map((m) => m[1]).join('\n');
}

const MOTION = /^\s*(animation|animation-name|transition)\s*:(?!\s*none)/m;
const REDUCED = /@media\s*\(prefers-reduced-motion:\s*reduce\)/;

/** The reduced-motion blocks of a stylesheet. */
function reducedBlocks(text: string): string {
  const blocks: string[] = [];
  for (const match of text.matchAll(/@media\s*\(prefers-reduced-motion:\s*reduce\)\s*\{/g)) {
    let depth = 1;
    let at = (match.index ?? 0) + match[0].length;
    const start = at;
    while (depth > 0 && at < text.length) {
      if (text[at] === '{') depth += 1;
      else if (text[at] === '}') depth -= 1;
      at += 1;
    }
    blocks.push(text.slice(start, at - 1));
  }
  return blocks.join('\n');
}

describe('motion', () => {
  it('defines the motion tokens', () => {
    const tokens = source('styles/tokens.css');
    expect(tokens).toContain('--ease-out: cubic-bezier(0.23, 1, 0.32, 1);');
    expect(tokens).toContain('--ease-in-out: cubic-bezier(0.77, 0, 0.175, 1);');
    expect(tokens).toContain('--dur-fast: 150ms;');
  });

  it('gives every stylesheet with motion a reduced-motion rule', () => {
    expect(sources.size).toBeGreaterThan(100);
    const missing = [...sources.keys()].filter((path) => MOTION.test(css(path)) && !REDUCED.test(css(path)));
    expect(missing).toEqual([]);
  });

  it.each([
    ['components/common/Modal.vue', /\.panel\s*\{\s*animation-name:\s*panel-fade-in/],
    ['components/common/ContextMenu.vue', /\.root\s*\{\s*animation-name:\s*menu-fade-in/],
    ['components/notices/NoticeHost.vue', /\.notice-in\s*\{\s*animation-name:\s*notice-fade-in/],
    ['components/objects/ChangePanel.vue', /\.plan-in\s*\{\s*animation-name:\s*plan-fade-in/],
    ['components/table/PlanReview.vue', /\.steps\s*\{\s*animation-name:\s*plan-fade-in/],
    ['components/VarveMark.vue', /animation-name:\s*mark-fade-in/],
    ['components/common/ToastHost.vue', /\.toast-leave-to\s*\{\s*transform:\s*none/],
    ['styles/base.css', /:active:not\(:disabled\)\s*\{\s*transform:\s*none/],
  ])('%s keeps only a fade with reduced motion', (file, rule) => {
    const reduced = reducedBlocks(css(file));
    expect(reduced).toMatch(rule);
    // No movement inside the reduced-motion rules, other than taking it away.
    expect(reduced).not.toMatch(/transform:(?!\s*none)/);
  });

  it('uses fade-only keyframes for reduced motion', () => {
    for (const [file, name] of [
      ['components/common/Modal.vue', 'panel-fade-in'],
      ['components/common/ContextMenu.vue', 'menu-fade-in'],
      ['components/notices/NoticeHost.vue', 'notice-fade-in'],
      ['components/objects/ChangePanel.vue', 'plan-fade-in'],
      ['components/table/PlanReview.vue', 'plan-fade-in'],
      ['components/VarveMark.vue', 'mark-fade-in'],
    ] as const) {
      const body = new RegExp(`@keyframes ${name}\\s*\\{([^}]*\\})\\s*\\}`).exec(css(file))?.[1] ?? '';
      expect(body, `${file} ${name}`).toMatch(/opacity:\s*0/);
      expect(body, `${file} ${name}`).not.toMatch(/transform|stroke/);
    }
  });

  /**
   * Shared motion in base.css reaches every screen, the still ones included.
   * Allowed there: the press feedback of .btn and .icon-btn (a 120ms scale on
   * :active, not motion of the screen itself) and the switch knob. Anything
   * else shared needs the same decision first.
   */
  it('keeps the shared motion in base.css to press feedback and the switch', () => {
    const base = css('styles/base.css').replace(/\/\*[\s\S]*?\*\//g, '');
    const outside = base.replace(/@media[^{]*\{(?:[^{}]*\{[^{}]*\})*[^{}]*\}/g, '');
    const animated = [...outside.matchAll(/([^{}]+)\{([^{}]*)\}/g)]
      .filter((m) => MOTION.test(m[2] ?? ''))
      .map((m) => (m[1] ?? '').trim());
    expect(animated).toEqual(['.btn', '.icon-btn', '.switch::after']);
  });

  it('keeps the whole hit area of a pressed button', () => {
    const base = css('styles/base.css');
    expect(base).toMatch(/\.btn \{[^}]*position: relative;/);
    expect(base).toMatch(/\.icon-btn \{[^}]*position: relative;/);
    // The press scale takes 1.5% per side; the layer reaches 2% past each edge.
    expect(base).toMatch(/\.btn:active:not\(:disabled\)::after,\s*\.icon-btn:active:not\(:disabled\)::after \{\s*content: '';\s*position: absolute;\s*inset: -2%;/);
    expect(base).toMatch(/:active:not\(:disabled\) \{\s*transform: scale\(0\.97\);/);
  });

  it('never lets a fading copy take a click', () => {
    expect(css('styles/base.css')).toMatch(/\.motion-copy,\s*\.motion-copy \* \{[^}]*pointer-events: none !important;/);
  });

  /** Screens that stay still. The spinning refresh icon of the live panels predates this rule and has its own reduced-motion rule. */
  const STILL = ['components/AppShell.vue', 'components/StatusBar.vue', 'components/query/ResultGrid.vue', 'components/overview', 'components/live', 'components/redis', 'components/tree'];
  const STILL_EXCEPTIONS = new Set(['components/live/RefreshControls.vue']);

  it.each(STILL)('leaves %s without motion', (path) => {
    const targets = under(path);
    expect(targets.length).toBeGreaterThan(0);
    for (const file of targets) {
      if (STILL_EXCEPTIONS.has(file)) continue;
      expect(css(file), file).not.toMatch(MOTION);
    }
  });
});
