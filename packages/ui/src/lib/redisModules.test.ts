import type { BulkTtlStatus, TsMeta } from '../api/redis';
import {
  addEdit,
  buildQuery,
  bulkProgressText,
  cmsIncrements,
  formatRetention,
  formatTimestamp,
  labelProblems,
  parseTimestamp,
  previewText,
  queriesFor,
  retentionShrinks,
  settingsForm,
  tsAlterEdit,
  ttlFromSeconds,
} from './redisModules';

const meta: TsMeta = { retentionMs: '60000', labels: [{ name: 'sensor', value: 'a' }], duplicatePolicy: 'block' };

describe('time series helpers', () => {
  it('reads timestamps as milliseconds or dates', () => {
    expect(parseTimestamp('')).toBeUndefined();
    expect(parseTimestamp(' 1718000000000 ')).toBe('1718000000000');
    expect(parseTimestamp('2026-10-21T00:00:00Z')).toBe(String(Date.UTC(2026, 9, 21)));
    expect(parseTimestamp('soon')).toBeNull();
    expect(parseTimestamp('9223372036854775808')).toBeNull();
    expect(formatTimestamp('0')).toBe('1970-01-01T00:00:00.000Z');
    expect(formatTimestamp('9223372036854775807')).toBe('9223372036854775807');
  });

  it('words retentions', () => {
    expect(formatRetention('0')).toBe('forever');
    expect(formatRetention('86400000')).toBe('1 d');
    expect(formatRetention('90000')).toBe('90 s');
    expect(formatRetention('1500')).toBe('1500 ms');
  });

  it('checks label rows', () => {
    expect(labelProblems([{ name: 'a', value: ' x ' }])).toEqual([]);
    expect(labelProblems([{ name: '', value: '1' }, { name: 'a', value: '1' }, { name: 'a', value: '2' }])).toHaveLength(2);
  });

  it('keeps label values exactly and sends no labels when only the retention changed', () => {
    const spaced: TsMeta = { retentionMs: '0', labels: [{ name: 'k=v', value: '  padded  ' }], duplicatePolicy: null };
    const form = settingsForm(spaced);
    const edit = tsAlterEdit(spaced, { ...form, retentionMs: '1000' }).edit;
    expect(edit?.op === 'tsAlter' && edit.labels).toBeNull();
    form.labels[0] = { name: 'k=v', value: ' changed ' };
    const changed = tsAlterEdit(spaced, form).edit;
    expect(changed?.op === 'tsAlter' && changed.labels).toEqual([{ name: 'k=v', value: ' changed ' }]);
  });

  it('sends only what changed in the settings', () => {
    const form = settingsForm(meta);
    expect(tsAlterEdit(meta, form)).toEqual({ edit: null, problems: [] });
    expect(tsAlterEdit(meta, { ...form, retentionMs: '0' }).edit).toEqual({
      op: 'tsAlter',
      retentionMs: '0',
      labels: null,
      duplicatePolicy: null,
      original: meta,
    });
    const labels = tsAlterEdit(meta, { ...form, labels: [{ name: 'sensor', value: 'b' }, { name: 'room', value: '1' }], duplicatePolicy: 'LAST' }).edit;
    expect(labels?.op === 'tsAlter' && labels.labels).toEqual([{ name: 'sensor', value: 'b' }, { name: 'room', value: '1' }]);
    expect(labels?.op === 'tsAlter' && labels.duplicatePolicy).toBe('last');
    expect(tsAlterEdit(meta, { ...form, retentionMs: '-1', duplicatePolicy: 'newest' }).problems).toHaveLength(2);
  });

  it('knows when a lower retention drops samples', () => {
    expect(retentionShrinks('0', '1000')).toBe(true);
    expect(retentionShrinks('60000', '1000')).toBe(true);
    expect(retentionShrinks('60000', '0')).toBe(false);
    expect(retentionShrinks('60000', '120000')).toBe(false);
  });
});

describe('probabilistic helpers', () => {
  it('offers the queries of each kind', () => {
    expect(queriesFor('bloom').map((q) => q.op)).toEqual(['bloomExists']);
    expect(queriesFor('tDigest')).toHaveLength(6);
    expect(queriesFor('timeSeries')).toEqual([]);
  });

  it('builds queries from one input per line', () => {
    expect(buildQuery('bloomExists', 'a\n\nb')).toEqual({ op: 'bloomExists', items: [{ t: 'text', v: 'a' }, { t: 'text', v: 'b' }] });
    expect(buildQuery('tdigestQuantile', '0.5')).toEqual({ op: 'tdigestQuantile', quantiles: [{ t: 'text', v: '0.5' }] });
    expect(buildQuery('tdigestTrimmedMean', '0.1, 0.9')).toEqual({ op: 'tdigestTrimmedMean', low: { t: 'text', v: '0.1' }, high: { t: 'text', v: '0.9' } });
    expect(buildQuery('tdigestTrimmedMean', '0.1')).toBeNull();
    expect(buildQuery('cmsQuery', '  ')).toBeNull();
  });

  it('builds adds and Count-Min increments', () => {
    expect(addEdit('cuckoo', 'x', true)).toEqual({ op: 'cuckooAdd', items: [{ t: 'text', v: 'x' }], nx: true });
    expect(addEdit('topK', 'a\nb')).toEqual({ op: 'topkAdd', items: [{ t: 'text', v: 'a' }, { t: 'text', v: 'b' }] });
    expect(addEdit('tDigest', '1\n2.5')).toEqual({ op: 'tdigestAdd', values: [{ t: 'text', v: '1' }, { t: 'text', v: '2.5' }] });
    expect(addEdit('tDigest', '1\nabc')).toBeNull();
    expect(addEdit('countMin', 'page 5\nhome')).toEqual({
      op: 'cmsIncrBy',
      items: [
        { item: { t: 'text', v: 'page' }, increment: '5' },
        { item: { t: 'text', v: 'home' }, increment: '1' },
      ],
    });
    expect(cmsIncrements('page 0').problems).toHaveLength(1);
    expect(addEdit('bloom', '')).toBeNull();
  });
});

describe('bulk TTL texts', () => {
  it('says whether a count is complete', () => {
    expect(previewText({ keys: '1200', complete: true, sample: [], db: 2, cluster: false })).toBe('1,200 keys match on database 2.');
    expect(previewText({ keys: '1', complete: false, sample: [], db: 0, cluster: false })).toContain('At least 1 key');
  });

  it('describes progress and how a job ended', () => {
    const status: BulkTtlStatus = {
      jobId: 'j',
      state: 'running',
      counts: { steps: '1', matched: '500', changed: '480', skipped: '20' },
      error: null,
      elapsedMs: 10,
      db: 0,
      action: { kind: 'persist' },
      filter: { pattern: '*', keyType: null },
    };
    expect(bulkProgressText(status)).toBe('480 of 500 keys changed so far, 20 skipped (gone, or without a TTL).');
    expect(bulkProgressText({ ...status, state: 'stopped' })).toContain('keep their new TTL');
    expect(bulkProgressText({ ...status, state: 'failed', error: { kind: 'query', message: 'NOPERM', code: 'NOPERM', detail: null, hint: null, position: null } })).toContain('NOPERM');
  });

  it('reads TTL seconds', () => {
    expect(ttlFromSeconds('60')).toBe('60000');
    expect(ttlFromSeconds('0')).toBeNull();
    expect(ttlFromSeconds('1.5')).toBeNull();
  });
});
