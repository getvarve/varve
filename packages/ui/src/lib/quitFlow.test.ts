import { createQuitFlow } from './quitFlow';

function steps(confirm: () => Promise<boolean>, quit: () => Promise<void> = async () => undefined) {
  const order: string[] = [];
  return {
    order,
    acknowledge: vi.fn(async () => {
      order.push('acknowledge');
    }),
    confirmUnsaved: vi.fn(async () => {
      order.push('confirm');
      return confirm();
    }),
    quit: vi.fn(async () => {
      order.push('quit');
      return quit();
    }),
    stay: vi.fn(async () => {
      order.push('stay');
    }),
    failed: vi.fn(),
  };
}

describe('createQuitFlow', () => {
  it('acknowledges before asking, asks once for overlapping requests and quits after a yes', async () => {
    let answer: (ok: boolean) => void = () => undefined;
    const s = steps(
      () =>
        new Promise<boolean>((resolve) => {
          answer = resolve;
        }),
    );
    const request = createQuitFlow(s);
    const first = request();
    const second = request();
    await Promise.resolve();
    await Promise.resolve();
    answer(true);
    await expect(first).resolves.toBe(true);
    await expect(second).resolves.toBe(true);
    // Each request is acknowledged, the join too; the question is asked once.
    expect(s.order).toEqual(['acknowledge', 'acknowledge', 'confirm', 'quit']);
  });

  it('stays when the user cancels, and a request right after the cancel is answered', async () => {
    const s = steps(async () => false);
    let release: () => void = () => undefined;
    s.stay.mockImplementationOnce(
      () =>
        new Promise<void>((resolve) => {
          release = resolve;
        }),
    );
    const request = createQuitFlow(s);
    const first = request();
    for (let i = 0; i < 5; i += 1) await Promise.resolve();
    // cancel_quit has not finished yet, but the flow is already free.
    const second = request();
    expect(second).not.toBe(first);
    release();
    await expect(first).resolves.toBe(false);
    await expect(second).resolves.toBe(false);
    expect(s.confirmUnsaved).toHaveBeenCalledTimes(2);
    expect(s.quit).not.toHaveBeenCalled();
  });

  it('resolves false, reports the error and stays when quitting fails', async () => {
    const s = steps(
      async () => true,
      async () => {
        throw new Error('confirm_quit failed');
      },
    );
    await expect(createQuitFlow(s)()).resolves.toBe(false);
    expect(s.failed).toHaveBeenCalledTimes(1);
    expect(s.stay).toHaveBeenCalledTimes(1);
  });

  it('stays and reports the error when the unsaved check itself fails', async () => {
    const s = steps(async () => {
      throw new Error('dialog broke');
    });
    await expect(createQuitFlow(s)()).resolves.toBe(false);
    expect(s.failed).toHaveBeenCalledWith(new Error('dialog broke'));
    expect(s.stay).toHaveBeenCalledTimes(1);
    expect(s.quit).not.toHaveBeenCalled();
  });
});
