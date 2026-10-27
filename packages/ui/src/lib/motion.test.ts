import { COPY_GRACE_MS, fadeOutAndRemove, firstTime, inertCopy, leaveCopy } from './motion';

/** A Web Animations stand-in: jsdom has no `Element.animate`. Resolve `finish` to end every animation. */
function stubAnimate(): { calls: Array<{ el: Element; keyframes: Keyframe[]; options: KeyframeAnimationOptions }>; finish: () => void } {
  const calls: Array<{ el: Element; keyframes: Keyframe[]; options: KeyframeAnimationOptions }> = [];
  let finish = (): void => {};
  const finished = new Promise<void>((resolve) => {
    finish = resolve;
  });
  Element.prototype.animate = function animate(this: Element, keyframes: Keyframe[], options: KeyframeAnimationOptions) {
    calls.push({ el: this, keyframes, options });
    return { finished } as unknown as Animation;
  } as Element['animate'];
  return { calls, finish: () => finish() };
}

function source(): HTMLElement {
  const el = document.createElement('div');
  el.id = 'dialog';
  el.setAttribute('role', 'dialog');
  el.setAttribute('aria-labelledby', 'title');
  el.innerHTML = '<h2 id="title">Title</h2><input name="choice" type="radio" checked autofocus><button tabindex="0" aria-label="Close">x</button>';
  return el;
}

describe('motion helpers', () => {
  afterEach(() => {
    Reflect.deleteProperty(Element.prototype, 'animate');
    document.body.innerHTML = '';
  });

  it('makes an inert copy without ids, names, roles or ARIA', () => {
    const copy = inertCopy(source());
    expect(copy.hasAttribute('inert')).toBe(true);
    expect(copy.getAttribute('aria-hidden')).toBe('true');
    expect(copy.style.pointerEvents).toBe('none');
    expect(copy.classList.contains('motion-copy')).toBe(true);
    expect(copy.querySelector('[id], [name], [role], [tabindex], [autofocus], [aria-label], [aria-labelledby]')).toBeNull();
    expect(copy.hasAttribute('id') || copy.hasAttribute('role') || copy.hasAttribute('aria-labelledby')).toBe(false);
  });

  it('leaves nothing behind without the Web Animations API', () => {
    leaveCopy(source(), 120, (copy) => document.body.append(copy));
    expect(document.body.children).toHaveLength(0);
  });

  it('fades a copy out by opacity only and removes it when the fade ends', async () => {
    const { calls, finish } = stubAnimate();
    leaveCopy(source(), 120, (copy) => document.body.append(copy));
    const copy = document.body.querySelector('.motion-copy');
    expect(copy).not.toBeNull();
    expect(calls).toHaveLength(1);
    expect(calls[0]?.options.duration).toBe(120);
    expect(calls[0]?.keyframes.flatMap((k) => Object.keys(k))).toEqual(['opacity', 'opacity']);
    finish();
    await Promise.resolve();
    await Promise.resolve();
    expect(document.body.querySelector('.motion-copy')).toBeNull();
  });

  it('leaves no typed value in a copy, and secret inputs stay password inputs', () => {
    const form = document.createElement('form');
    form.innerHTML = [
      '<input class="hidden-pw" type="password">',
      '<input class="shown-pw" type="text" autocomplete="current-password">',
      '<input class="plain" type="text" value="default">',
      '<textarea class="area">first</textarea>',
      '<select class="pick"><option>a</option><option selected>b</option></select>',
      '<input class="check" type="checkbox" checked>',
    ].join('');
    (form.querySelector('.hidden-pw') as HTMLInputElement).value = 's3cret';
    (form.querySelector('.shown-pw') as HTMLInputElement).value = 'plain s3cret';
    (form.querySelector('.plain') as HTMLInputElement).value = 'typed';
    (form.querySelector('.area') as HTMLTextAreaElement).value = 'typed text';

    const copy = inertCopy(form);
    const hidden = copy.querySelector<HTMLInputElement>('.hidden-pw');
    const shown = copy.querySelector<HTMLInputElement>('.shown-pw');
    expect(hidden?.value).toBe('');
    expect(hidden?.type).toBe('password');
    expect(shown?.value).toBe('');
    expect(shown?.type).toBe('password');
    expect(copy.querySelector<HTMLInputElement>('.plain')?.value).toBe('');
    expect(copy.querySelector<HTMLInputElement>('.plain')?.hasAttribute('value')).toBe(false);
    expect(copy.querySelector<HTMLTextAreaElement>('.area')?.value).toBe('');
    expect(copy.querySelector<HTMLSelectElement>('.pick')?.selectedIndex).toBe(-1);
    expect(copy.querySelector<HTMLInputElement>('.check')?.checked).toBe(true);
    expect(copy.innerHTML).not.toContain('s3cret');
    // The original keeps its values.
    expect((form.querySelector('.hidden-pw') as HTMLInputElement).value).toBe('s3cret');
  });

  it('removes a copy after the grace time when its animation never ends', () => {
    vi.useFakeTimers();
    try {
      Element.prototype.animate = function animate() {
        return { finished: new Promise(() => {}) } as unknown as Animation;
      } as Element['animate'];
      const copy = inertCopy(source());
      document.body.append(copy);
      fadeOutAndRemove(copy, 120);
      vi.advanceTimersByTime(120 + COPY_GRACE_MS - 1);
      expect(copy.isConnected).toBe(true);
      vi.advanceTimersByTime(1);
      expect(copy.isConnected).toBe(false);
    } finally {
      vi.useRealTimers();
    }
  });

  it('puts the scroll offsets of the original back on the placed copy', () => {
    stubAnimate();
    const el = document.createElement('div');
    el.innerHTML = '<div class="body"><p>long</p></div>';
    document.body.append(el);
    const body = el.querySelector('.body') as HTMLElement;
    body.scrollTop = 140;
    body.scrollLeft = 12;
    const copy = inertCopy(el);
    document.body.append(copy);
    fadeOutAndRemove(copy, 120);
    const copied = copy.querySelector('.body') as HTMLElement;
    expect([copied.scrollTop, copied.scrollLeft]).toEqual([140, 12]);
  });

  it('freezes a running entrance and fades out from the opacity shown now', () => {
    const { calls } = stubAnimate();
    const el = document.createElement('div');
    el.innerHTML = '<div class="panel"></div>';
    document.body.append(el);
    const panel = el.querySelector('.panel') as HTMLElement;
    // What getComputedStyle reports in the middle of the entrance; the inline styles stay empty.
    const now = new Map<Element, Partial<CSSStyleDeclaration>>([
      [el, { opacity: '0.5', transform: 'none' }],
      [panel, { opacity: '0.4', transform: 'matrix(0.985, 0, 0, 0.985, 0, 0)' }],
    ]);
    const spy = vi.spyOn(window, 'getComputedStyle').mockImplementation((target) => now.get(target) as CSSStyleDeclaration);
    el.getAnimations = () => [{ effect: { target: el } }, { effect: { target: panel } }] as unknown as Animation[];

    const copy = inertCopy(el);
    spy.mockRestore();
    const copiedPanel = copy.querySelector('.panel') as HTMLElement;
    expect(copy.style.opacity).toBe('0.5');
    expect(copy.style.transform).toBe('');
    expect(copiedPanel.style.opacity).toBe('0.4');
    expect(copiedPanel.style.transform).toBe('matrix(0.985, 0, 0, 0.985, 0, 0)');
    document.body.append(copy);
    fadeOutAndRemove(copy, 120);
    expect(calls[0]?.keyframes).toEqual([{ opacity: 0.5 }, { opacity: 0 }]);
  });

  it('answers true only the first time for a key', () => {
    expect(firstTime('motion-test-key')).toBe(true);
    expect(firstTime('motion-test-key')).toBe(false);
    expect(firstTime('motion-test-other')).toBe(true);
  });
});
