import { primaryFill } from './topologyColors';

describe('primaryFill', () => {
  it('gives the first 18 primaries 18 different fills', () => {
    const fills = Array.from({ length: 18 }, (_, i) => primaryFill(i));
    expect(new Set(fills).size).toBe(18);
    expect(fills[0]).toBe('var(--tag-violet)');
    expect(fills[6]).toBe('color-mix(in srgb, var(--tag-violet) 55%, var(--surface))');
    expect(fills[12]).toContain('repeating-linear-gradient');
  });
});
