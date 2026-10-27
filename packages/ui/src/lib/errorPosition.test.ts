import { errorMark, markedErrorRuns, rememberMarkedRun } from './errorPosition';

describe('errorMark', () => {
  it('marks the token at the position, with line and column from the run text', () => {
    const doc = 'SELECT 1;\nSELECT * FROM missing_table;';
    // The second statement was run on its own (a selection starting at offset 10).
    const text = 'SELECT * FROM missing_table;';
    expect(errorMark(7, { position: 15 }, doc, text, 10)).toEqual({ key: 7, from: 24, to: 37, line: 2, column: 15, doc });
  });

  it('marks at least one character and ignores missing positions', () => {
    expect(errorMark(1, { position: 3 }, 'a +', 'a +', 0)).toMatchObject({ from: 2, to: 3 });
    expect(errorMark(1, { position: null }, 'x', 'x', 0)).toBeNull();
  });
});

describe('error marks', () => {
  it('counts the backend position in characters, also after astral characters', () => {
    // "😀" is one character but two UTF-16 units: character 5 is the "x", at UTF-16 offset 5.
    const doc = "'😀' x";
    expect(errorMark(2, { position: 5 }, doc, doc, 0)).toMatchObject({ from: 5, to: 6 });
  });

  it('keeps the remembered runs bounded', () => {
    for (let i = 0; i < 300; i += 1) rememberMarkedRun(10_000 + i);
    expect(markedErrorRuns.size).toBeLessThanOrEqual(200);
    expect(markedErrorRuns.has(10_299)).toBe(true);
    expect(markedErrorRuns.has(10_000)).toBe(false);
  });
});
