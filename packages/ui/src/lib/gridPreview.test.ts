import type { Value } from '../api/sessions';
import { cachedEditText, previewText, TOOLTIP_CHARS, tooltipText } from './gridPreview';
import { editText } from './gridValues';
import { cellText, fullText } from './values';

const long = 'x'.repeat(5000);
const samples: Value[] = [
  { t: 'null' },
  { t: 'int', v: 42 },
  { t: 'text', v: 'short' },
  { t: 'text', v: `a\nb\r\nc${long}` },
  { t: 'text', v: 'y'.repeat(200) },
  { t: 'text', v: 'y'.repeat(201) },
  { t: 'text', v: `${'z'.repeat(199)}\n` },
  { t: 'json', v: '{"a": 1}' },
  { t: 'json', v: `{\n  "a": "${long}",\n  "b": [1, 2]\n}` },
  { t: 'json', v: `[${'\n    1,'.repeat(300)}\n 2]` },
  { t: 'array', v: [{ t: 'int', v: 1 }, { t: 'null' }, { t: 'text', v: 'q' }] },
  { t: 'array', v: Array.from({ length: 400 }, (_, i): Value => ({ t: 'text', v: `item${i}` })) },
  { t: 'array', v: [{ t: 'array', v: [{ t: 'text', v: long }] }, { t: 'int', v: 2 }] },
  { t: 'bytes', v: 'ab'.repeat(100) },
];

describe('previewText', () => {
  it('shows exactly what cellText shows', () => {
    for (const value of samples) expect(previewText(value)).toBe(cellText(value));
  });

  it('formats a value once and serves it from the cache after that', () => {
    // Counts every read of the value's text: a cache hit reads nothing.
    let reads = 0;
    const body = `{"k": "${long}"}`;
    const value = {
      t: 'json',
      get v(): string {
        reads += 1;
        return body;
      },
    } as Value;
    const first = previewText(value);
    const afterFirst = reads;
    expect(afterFirst).toBeGreaterThan(0);
    expect(previewText(value)).toBe(first);
    expect(previewText(value)).toBe(first);
    expect(reads).toBe(afterFirst);

    const edited = cachedEditText('postgres', value);
    const afterEdit = reads;
    expect(cachedEditText('postgres', value)).toBe(edited);
    expect(reads).toBe(afterEdit);
    expect(edited).toBe(editText('postgres', { t: 'json', v: body }));
  });
});

describe('tooltipText', () => {
  it('adds an ellipsis only when something was cut', () => {
    expect(tooltipText({ t: 'text', v: 'a'.repeat(TOOLTIP_CHARS) })).toBe('a'.repeat(TOOLTIP_CHARS));
    expect(tooltipText({ t: 'text', v: 'a'.repeat(TOOLTIP_CHARS + 1) })).toBe(`${'a'.repeat(TOOLTIP_CHARS)}…`);
    const raw = `[${'1,'.repeat(15_000)}1]`;
    expect(tooltipText({ t: 'json', v: raw })).toBe(`${raw.slice(0, TOOLTIP_CHARS)}…`);
  });

  it('shows arrays as a re-indented JSON list, as before', () => {
    const small: Value = { t: 'array', v: [{ t: 'int', v: 1 }, { t: 'null' }, { t: 'text', v: 'q"x' }, { t: 'array', v: [{ t: 'bool', v: true }] }] };
    expect(tooltipText(small)).toBe(fullText(small));
    expect(tooltipText(small)).toBe('[\n  1,\n  null,\n  "q\\"x",\n  [\n    true\n  ]\n]');
  });

  it('stays within the cap for huge arrays and huge elements', () => {
    const many: Value = { t: 'array', v: Array.from({ length: 50_000 }, (_, i): Value => ({ t: 'int', v: i })) };
    const tip = tooltipText(many);
    expect(tip.length).toBe(TOOLTIP_CHARS + 1);
    expect(tip.endsWith('…')).toBe(true);
    expect(tip.startsWith(fullText({ t: 'array', v: many.v.slice(0, 50) }).slice(0, 100))).toBe(true);
    const wide: Value = { t: 'array', v: [{ t: 'text', v: 'w'.repeat(100_000) }] };
    expect(tooltipText(wide).length).toBe(TOOLTIP_CHARS + 1);
  });
});

/**
 * Benchmark of the grid formatting path: scrolls a 40 row viewport over
 * every row and formats every cell of it, as AG Grid does per frame.
 * Runs only with U3_PERF_DATA pointing at a JSON array of rows of jsonb
 * text values (exported from PostgreSQL); prints old and new timings.
 */
const perfData = (globalThis as { process?: { env: Record<string, string | undefined> } }).process?.env.U3_PERF_DATA;

async function load(path: string): Promise<string[][]> {
  const fsModule = 'node:fs';
  const fs = (await import(/* @vite-ignore */ fsModule)) as { readFileSync: (path: string, encoding: string) => string };
  return JSON.parse(fs.readFileSync(path, 'utf8')) as string[][];
}

/**
 * 1000 rows x 20 jsonb values shaped like the PostgreSQL 18 test table
 * (`jsonb::text` output, about 1.8 kB each, every 97th row about 72 kB).
 */
function synthetic(): string[][] {
  const hex = (n: number): string => (Math.imul(n, 2654435761) >>> 0).toString(16).padStart(8, '0').repeat(4);
  return Array.from({ length: 1000 }, (_, r) => {
    const g = r + 1;
    return Array.from({ length: 20 }, (_, c) => {
      const tags = Array.from({ length: g % 97 === 0 ? 2000 : 20 }, (_, k) => `"${hex(g * (k + 1))}"`).join(', ');
      return `{"id": ${g}, "col": ${c + 1}, "name": "${hex(g + c)}", "note": "line one\\nline two", "tags": [${tags}], "nested": {"a": ${g}, "b": "${'x'.repeat(200)}", "c": [1, 2, 3, ${g}]}}`;
    });
  });
}

describe.runIf(!!perfData)('formatting benchmark', () => {
  it('formats scrolled frames of a 1000 x 20 jsonb table', async () => {
    const raw = perfData === 'synthetic' ? synthetic() : await load(perfData ?? '');
    const rows = (): Value[][] => raw.map((r) => r.map((v): Value => ({ t: 'json', v })));
    const viewport = 40;
    const run = (format: (v: Value) => string, data: Value[][]): { total: number; frames: number; worst: number } => {
      let total = 0;
      let worst = 0;
      let frames = 0;
      for (let top = 0; top + viewport <= data.length; top += 1) {
        const start = performance.now();
        for (let r = top; r < top + viewport; r += 1) {
          for (const value of data[r] as Value[]) {
            editText('postgres', value);
            format(value);
          }
        }
        const ms = performance.now() - start;
        total += ms;
        worst = Math.max(worst, ms);
        frames += 1;
      }
      return { total, frames, worst };
    };
    const before = run(cellText, rows());
    const after = run(previewText, rows());
    const line = (name: string, r: { total: number; frames: number; worst: number }): string =>
      `${name}: ${r.frames} frames, ${r.total.toFixed(1)} ms total, ${(r.total / r.frames).toFixed(3)} ms per frame, worst frame ${r.worst.toFixed(2)} ms`;
    console.log(`${raw.length} rows x ${raw[0]?.length ?? 0} jsonb columns, ${viewport} rows per frame`);
    console.log(line('before (cellText)', before));
    console.log(line('after (previewText)', after));
    expect(after.total).toBeLessThan(before.total);
  });
});
