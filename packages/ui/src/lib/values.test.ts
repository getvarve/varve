import type { Value } from '../api/sessions';
import {
  isOversized,
  isPlaceholder,
  rustFloat,
  cellClass,
  cellText,
  charPositionToOffset,
  compareValues,
  formatBytes,
  formatCount,
  formatDuration,
  formatTtl,
  fullText,
  needsViewer,
  plainText,
  prettyJson,
} from './values';

describe('value formatting', () => {
  it('matches the backend plain text of every type', () => {
    expect(plainText({ t: 'null' })).toBeNull();
    expect(plainText({ t: 'bool', v: false })).toBe('false');
    expect(plainText({ t: 'int', v: -3 })).toBe('-3');
    expect(plainText({ t: 'uInt', v: 18446744073709 })).toBe('18446744073709');
    expect(plainText({ t: 'float', v: 1.25 })).toBe('1.25');
    expect(plainText({ t: 'decimal', v: '10.50' })).toBe('10.50');
    expect(plainText({ t: 'bytes', v: '00abff' })).toBe('\\x00abff');
    expect(plainText({ t: 'timestampTz', v: '2026-10-19T11:45:00Z' })).toBe('2026-10-19T11:45:00Z');
    expect(plainText({ t: 'json', v: '{"a": 1, "big": 12345678901234567890}' })).toBe('{"a": 1, "big": 12345678901234567890}');
    expect(plainText({ t: 'array', v: [{ t: 'int', v: 1 }, { t: 'null' }, { t: 'text', v: 'a' }] })).toBe('{1,NULL,a}');
  });

  it('passes exact strings for big integers and non-finite floats through', () => {
    expect(plainText({ t: 'float', v: 'NaN' })).toBe('NaN');
    expect(plainText({ t: 'float', v: '-Infinity' })).toBe('-inf');
    expect(plainText({ t: 'float', v: 'Infinity' })).toBe('inf');
    expect(plainText({ t: 'int', v: '-9223372036854775808' })).toBe('-9223372036854775808');
    expect(plainText({ t: 'uInt', v: '18446744073709551615' })).toBe('18446744073709551615');
  });

  it('shortens long values for cells', () => {
    expect(cellText({ t: 'null' })).toBe('NULL');
    const bytes: Value = { t: 'bytes', v: 'ab'.repeat(40) };
    expect(cellText(bytes)).toBe(`\\x${'ab'.repeat(32)}… (40 bytes)`);
    expect(cellText({ t: 'text', v: 'line 1\nline 2' })).toBe('line 1 ⏎ line 2');
    expect(cellText({ t: 'text', v: 'x'.repeat(300) })).toHaveLength(201);
    expect(cellText({ t: 'json', v: '{\n  "a": [1, 2]\n}' })).toBe('{ "a": [1, 2] }');
  });

  it('pretty prints JSON and arrays for the value panel', () => {
    expect(fullText({ t: 'json', v: '{"a":1,"n":12345678901234567890}' })).toBe('{\n  "a": 1,\n  "n": 12345678901234567890\n}');
    expect(fullText({ t: 'array', v: [{ t: 'int', v: 1 }] })).toBe('[\n  1\n]');
    expect(fullText({ t: 'null' })).toBe('NULL');
  });

  it('knows which values need the viewer', () => {
    expect(needsViewer({ t: 'json', v: '1' })).toBe(true);
    expect(needsViewer({ t: 'text', v: 'short' })).toBe(false);
    expect(needsViewer({ t: 'text', v: 'a\nb' })).toBe(true);
    expect(needsViewer({ t: 'int', v: 1 })).toBe(false);
  });

  it('classes cells for alignment and NULL styling', () => {
    expect(cellClass({ t: 'null' })).toBe('v-null');
    expect(cellClass({ t: 'decimal', v: '1' })).toBe('v-num');
    expect(cellClass({ t: 'uuid', v: 'x' })).toBe('v-text');
  });

  it('sorts NULL first and numbers numerically', () => {
    const values: Value[] = [{ t: 'int', v: 10 }, { t: 'null' }, { t: 'decimal', v: '9.5' }, { t: 'int', v: -1 }];
    expect([...values].sort(compareValues).map(plainText)).toEqual([null, '-1', '9.5', '10']);
    const big: Value[] = [{ t: 'int', v: '9007199254740993' }, { t: 'int', v: '9007199254740992' }];
    expect([...big].sort(compareValues).map(plainText)).toEqual(['9007199254740992', '9007199254740993']);
  });

  it('sorts non-finite floats as PostgreSQL does: -Infinity, numbers, Infinity, NaN', () => {
    const values: Value[] = [
      { t: 'float', v: 'NaN' },
      { t: 'float', v: 'Infinity' },
      { t: 'float', v: -7 },
      { t: 'float', v: '-Infinity' },
      { t: 'int', v: 3 },
      { t: 'decimal', v: 'NaN' },
      { t: 'decimal', v: '-Infinity' },
    ];
    expect([...values].sort(compareValues).map(plainText)).toEqual(['-inf', '-Infinity', '-7', '3', 'inf', 'NaN', 'NaN']);
  });

  it('sorts decimals exactly, beyond the precision of a double', () => {
    const values: Value[] = [
      { t: 'decimal', v: '-1.00000000000000000001' },
      { t: 'decimal', v: '-1.00000000000000000009' },
      { t: 'decimal', v: '1.00000000000000000009' },
      { t: 'decimal', v: '1.00000000000000000001' },
      { t: 'decimal', v: '123456789012345678901234567890.5' },
      { t: 'decimal', v: '123456789012345678901234567890.25' },
      { t: 'decimal', v: '0.5' },
    ];
    expect([...values].sort(compareValues).map(plainText)).toEqual([
      '-1.00000000000000000009',
      '-1.00000000000000000001',
      '0.5',
      '1.00000000000000000001',
      '1.00000000000000000009',
      '123456789012345678901234567890.25',
      '123456789012345678901234567890.5',
    ]);
    // Mixed kinds compare by value: 2.5 (float) after 2 (int) and before 2.50001 (decimal).
    const mixed: Value[] = [{ t: 'decimal', v: '2.50001' }, { t: 'float', v: 2.5 }, { t: 'int', v: 2 }, { t: 'float', v: 1e-7 }];
    expect([...mixed].sort(compareValues).map(plainText)).toEqual(['0.0000001', '2', '2.5', '2.50001']);
    // Equal values (-0 and 0, 1.50 and 1.5) compare equal in value and fall back to text.
    expect(compareValues({ t: 'decimal', v: '1.50' }, { t: 'float', v: 1.5 })).toBe('1.50'.localeCompare('1.5'));
    expect(compareValues({ t: 'float', v: -0 }, { t: 'int', v: 0 })).toBe('-0'.localeCompare('0'));
  });

  it('sorts intervals by length, in ISO 8601 and PostgreSQL style', () => {
    const iso: Value[] = [
      { t: 'interval', v: 'P10D' },
      { t: 'interval', v: 'P2D' },
      { t: 'interval', v: 'P1M' },
      { t: 'interval', v: 'P1D' },
      { t: 'interval', v: 'PT-1S' },
      { t: 'interval', v: 'PT36H' },
      { t: 'interval', v: 'P-1Y2M' },
      { t: 'interval', v: 'PT0.5S' },
    ];
    expect([...iso].sort(compareValues).map(plainText)).toEqual(['P-1Y2M', 'PT-1S', 'PT0.5S', 'P1D', 'PT36H', 'P2D', 'P10D', 'P1M']);
    const pg: Value[] = [
      { t: 'interval', v: '10 days' },
      { t: 'interval', v: '2 days' },
      { t: 'interval', v: '1 mon' },
      { t: 'interval', v: '1 day' },
      { t: 'interval', v: '1 day -01:00:00' },
      { t: 'interval', v: '1 year 2 mons 3 days 04:05:06.5' },
    ];
    expect([...pg].sort(compareValues).map(plainText)).toEqual(['1 day -01:00:00', '1 day', '2 days', '10 days', '1 mon', '1 year 2 mons 3 days 04:05:06.5']);
    // A form it does not read keeps the text order.
    expect(compareValues({ t: 'interval', v: 'b' }, { t: 'interval', v: 'P1D' })).toBe('b'.localeCompare('P1D'));
  });

  it('maps a 1-based character position to an editor offset', () => {
    expect(charPositionToOffset('SELECT x', 1)).toBe(0);
    expect(charPositionToOffset('SELECT x', 8)).toBe(7);
    // The emoji is one character but two UTF-16 units.
    expect(charPositionToOffset('😀 bad', 3)).toBe(3);
    expect(charPositionToOffset('abc', 99)).toBe(3);
  });

  it('formats durations, sizes and TTLs', () => {
    expect(formatDuration(38)).toBe('38 ms');
    expect(formatDuration(1500)).toBe('1.50 s');
    expect(formatDuration(192_000)).toBe('3 min 12 s');
    // Rounded before the unit is chosen.
    expect(formatDuration(999.6)).toBe('1.00 s');
    expect(formatDuration(9_996)).toBe('10.0 s');
    expect(formatDuration(59_940)).toBe('59.9 s');
    expect(formatDuration(59_960)).toBe('1 min 0 s');
    expect(formatDuration(119_600)).toBe('2 min 0 s');
    expect(formatBytes(512)).toBe('512 B');
    expect(formatBytes(1434)).toBe('1.4 KB');
    expect(formatTtl(null)).toBe('No expiry');
    expect(formatCount('18446744073709551615')).toBe('18,446,744,073,709,551,615');
    expect(formatBytes('2048')).toBe('2.0 KB');
    expect(formatTtl('4327000')).toBe('1h 12m');
    expect(formatTtl('59999')).toBe('59 s');
    expect(formatTtl('-1')).toBe('No expiry');
    expect(formatTtl('922337203685477580')).toBe('10675199116d 17h');
  });

  it('pretty prints JSON text only when it parses', () => {
    expect(prettyJson('{"a":1}')).toBe('{\n  "a": 1\n}');
    expect(prettyJson('not json')).toBeNull();
    expect(prettyJson('{broken')).toBeNull();
    expect(prettyJson('{"id":12345678901234567890,"z":1,"a":[],"s":"x, {y}: \\"q\\""}')).toBe(
      '{\n  "id": 12345678901234567890,\n  "z": 1,\n  "a": [],\n  "s": "x, {y}: \\"q\\""\n}',
    );
  });
});

describe('oversized entries', () => {
  it('recognizes the placeholder and never treats it as a viewable value', () => {
    const placeholder: Value = { t: 'other', v: '(too large to show: 17000000 bytes)' };
    expect(isOversized(placeholder)).toBe(true);
    expect(isOversized({ t: 'other', v: 'too large' })).toBe(false);
    expect(isOversized({ t: 'text', v: '(too large to show: 1 bytes)' })).toBe(false);
    expect(cellClass(placeholder)).toBe('v-placeholder');
    const member: Value = { t: 'other', v: '(too large to show: 9000000 bytes at the time of the scan)' };
    expect(isPlaceholder(member)).toBe(true);
    expect(cellClass(member)).toBe('v-placeholder');
    // The removed "no longer found" marker is ordinary text now.
    expect(isPlaceholder({ t: 'other', v: '(no longer found; it may have moved or been removed)' })).toBe(false);
    expect(isPlaceholder({ t: 'text', v: '(too large to show: 1 bytes)' })).toBe(false);
    expect(needsViewer({ t: 'other', v: `(too large to show: ${'9'.repeat(300)} bytes)` })).toBe(false);
  });
});

describe('rustFloat', () => {
  it('writes floats like Rust f64 Display', () => {
    expect(rustFloat(1e21)).toBe('1000000000000000000000');
    expect(rustFloat(1e-7)).toBe('0.0000001');
    expect(rustFloat(-0)).toBe('-0');
    expect(rustFloat(0.1 + 0.2)).toBe('0.30000000000000004');
    expect(rustFloat(1.5e-10)).toBe('0.00000000015');
    expect(rustFloat(-2.5e22)).toBe('-25000000000000000000000');
    expect(rustFloat(1.25)).toBe('1.25');
    expect(rustFloat(3)).toBe('3');
    expect(rustFloat('NaN')).toBe('NaN');
  });
});
