import type { Column, Value } from '../api/sessions';
import { countPlaceholders, csvCell, placeholderNotice, neutralizeFormula, quoteCsv, toCsv, toJson, toTsv, uniqueColumnKeys } from './exportResult';

const text = (v: string): Value => ({ t: 'text', v });

describe('CSV export', () => {
  it('quotes separators, quotes, line breaks and edge spaces', () => {
    expect(quoteCsv('plain')).toBe('plain');
    expect(quoteCsv('a,b')).toBe('"a,b"');
    expect(quoteCsv('say "hi"')).toBe('"say ""hi"""');
    expect(quoteCsv('two\nlines')).toBe('"two\nlines"');
    expect(quoteCsv('cr\rlf')).toBe('"cr\rlf"');
    expect(quoteCsv(' padded ')).toBe('" padded "');
  });

  it('writes NULL as an empty field and keeps empty text distinguishable', () => {
    expect(csvCell({ t: 'null' })).toBe('');
    expect(csvCell(text(''))).toBe('');
    expect(csvCell(text(' '))).toBe('" "');
  });

  it('prefixes text that a spreadsheet would run as a formula', () => {
    expect(neutralizeFormula('=SUM(A1:A9)')).toBe("'=SUM(A1:A9)");
    expect(csvCell(text('+31 6 1234'))).toBe("'+31 6 1234");
    expect(csvCell(text('-2+3'))).toBe("'-2+3");
    expect(csvCell(text('@cmd'))).toBe("'@cmd");
    expect(csvCell(text('=HYPERLINK("x")'))).toBe('"\'=HYPERLINK(""x"")"');
    expect(csvCell(text('safe=text'))).toBe('safe=text');
  });

  it('leaves typed numbers alone', () => {
    expect(csvCell({ t: 'int', v: -5 })).toBe('-5');
    expect(csvCell({ t: 'decimal', v: '-10.50' })).toBe('-10.50');
    expect(csvCell({ t: 'float', v: 1.5 })).toBe('1.5');
    expect(csvCell({ t: 'float', v: '-Infinity' })).toBe('-inf');
    expect(csvCell({ t: 'int', v: '-9223372036854775808' })).toBe('-9223372036854775808');
  });

  it('formats bytes, JSON, arrays and booleans like the backend', () => {
    expect(csvCell({ t: 'bytes', v: 'dead' })).toBe('\\xdead');
    expect(csvCell({ t: 'json', v: '{"a":1,"b":"x"}' })).toBe('"{""a"":1,""b"":""x""}"');
    expect(csvCell({ t: 'array', v: [{ t: 'int', v: 1 }, { t: 'null' }] })).toBe('"{1,NULL}"');
    expect(csvCell({ t: 'bool', v: true })).toBe('true');
  });

  it('builds a document with a header row and CRLF line ends', () => {
    const columns: Column[] = [
      { name: 'id', typeName: 'int8' },
      { name: 'note', typeName: 'text' },
    ];
    const rows: Value[][] = [
      [{ t: 'int', v: 1 }, text('hello, world')],
      [{ t: 'int', v: 2 }, { t: 'null' }],
    ];
    expect(toCsv(columns, rows)).toBe('id,note\r\n1,"hello, world"\r\n2,\r\n');
  });

  it('neutralizes header names too', () => {
    expect(toCsv([{ name: '=cmd', typeName: 'text' }], [])).toBe("'=cmd\r\n");
  });
});

describe('JSON export', () => {
  it('writes one object per row with typed values', () => {
    const columns: Column[] = [
      { name: 'id', typeName: 'int8' },
      { name: 'total', typeName: 'numeric' },
      { name: 'meta', typeName: 'jsonb' },
      { name: 'gone', typeName: 'text' },
    ];
    const rows: Value[][] = [[{ t: 'int', v: 7 }, { t: 'decimal', v: '10.50' }, { t: 'json', v: '{"a":[1]}' }, { t: 'null' }]];
    expect(JSON.parse(toJson(columns, rows))).toEqual([{ id: 7, total: '10.50', meta: { a: [1] }, gone: null }]);
  });

  it('writes exact big integers, JSON text and non-finite floats without losing anything', () => {
    const columns: Column[] = [
      { name: 'big', typeName: 'int8' },
      { name: 'doc', typeName: 'jsonb' },
      { name: 'f', typeName: 'float8' },
    ];
    const rows: Value[][] = [[{ t: 'int', v: '9223372036854775807' }, { t: 'json', v: '{"n":12345678901234567890}' }, { t: 'float', v: 'NaN' }]];
    const text = toJson(columns, rows);
    expect(text).toContain('"big": 9223372036854775807');
    expect(text).toContain('"doc": {"n":12345678901234567890}');
    expect(text).toContain('"f": "NaN"');
    expect(toJson(columns, [])).toBe('[]\n');
  });

  it('keeps every column when names repeat', () => {
    const columns: Column[] = [
      { name: 'id', typeName: 'int8' },
      { name: 'id', typeName: 'int8' },
      { name: 'id_2', typeName: 'int8' },
    ];
    expect(uniqueColumnKeys(columns)).toEqual(['id', 'id_2', 'id_2_2']);
  });
});

describe('copying rows', () => {
  it('copies placeholders of oversized entries as empty fields', () => {
    expect(toTsv([[{ t: 'int', v: 1 }, { t: 'other', v: '(too large to show: 99 bytes)' }, { t: 'text', v: 'a\tb' }]])).toBe('1\t\ta b');
    expect(toTsv([[{ t: 'array', v: [{ t: 'other', v: '(too large to show: 5 bytes)' }, { t: 'int', v: 2 }] }]])).toBe('{NULL,2}');
  });
});

describe('oversized cells in exports', () => {
  const big: Value = { t: 'other', v: '(too large to show: 17000000 bytes)' };
  const columns: Column[] = [
    { name: 'key', typeName: 'value' },
    { name: 'value', typeName: 'value' },
  ];
  const rows: Value[][] = [
    [text('a'), big],
    [text('b'), { t: 'array', v: [big, { t: 'int', v: 1 }] }],
  ];

  it('writes an empty CSV field and JSON null, never the placeholder', () => {
    expect(toCsv(columns, rows)).toBe('key,value\r\na,\r\nb,"{NULL,1}"\r\n');
    const json = toJson(columns, rows);
    expect(JSON.parse(json)).toEqual([
      { key: 'a', value: null },
      { key: 'b', value: [null, 1] },
    ]);
    expect(json).not.toContain('too large');
  });

  it('treats set member placeholders with their scan-time size like other placeholders', () => {
    const member: Value = { t: 'other', v: '(too large to show: 9000000 bytes at the time of the scan)' };
    expect(csvCell(member)).toBe('');
    expect(JSON.parse(toJson([{ name: 'member', typeName: 'value' }], [[member]]))).toEqual([{ member: null }]);
    expect(countPlaceholders([[member, big]])).toBe(2);
  });

  it('counts the cells that were left out', () => {
    expect(countPlaceholders(rows)).toBe(2);
    expect(countPlaceholders([[text('x')]])).toBe(0);
  });

  it('warns how many cells were left out', () => {
    expect(placeholderNotice(0, 'csv')).toBeNull();
    expect(placeholderNotice(1, 'csv')?.title).toBe('1 cell was left out of the export');
    expect(placeholderNotice(1, 'csv')?.message).toContain('an empty field');
    expect(placeholderNotice(1200, 'json')).toEqual({
      title: '1,200 cells were left out of the export',
      message: 'They were too large to load, so the file has null in their place. Read such values in the console.',
    });
  });
});
