import { latestOnly, singleFlight } from './async';

describe('singleFlight', () => {
  it('ignores calls while the first one runs, then accepts new ones', async () => {
    let release: () => void = () => undefined;
    const action = vi.fn(
      () =>
        new Promise<void>((resolve) => {
          release = resolve;
        }),
    );
    const flight = singleFlight(action);
    const first = flight.run();
    await flight.run();
    await flight.run();
    expect(action).toHaveBeenCalledTimes(1);
    expect(flight.busy()).toBe(true);
    release();
    await first;
    expect(flight.busy()).toBe(false);
    const second = flight.run();
    release();
    await second;
    expect(action).toHaveBeenCalledTimes(2);
  });

  it('becomes free again when the action fails', async () => {
    const flight = singleFlight(async () => {
      throw new Error('boom');
    });
    await expect(flight.run()).rejects.toThrow('boom');
    expect(flight.busy()).toBe(false);
  });
});

describe('latestOnly', () => {
  it('only the newest token is current', () => {
    const latest = latestOnly();
    const a = latest.next();
    const b = latest.next();
    expect(latest.isCurrent(a)).toBe(false);
    expect(latest.isCurrent(b)).toBe(true);
  });
});
