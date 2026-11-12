import { describe, expect, it } from 'vitest';
import { CHANGE_KIND_TIPS, isCut, keysText, keyTypeTip, liveTip, offReason, READ_ONLY_TIP, tabTip, withKeys } from './tips';

describe('keysText', () => {
  it('writes macOS shortcuts as symbols in the system order', () => {
    expect(keysText('CmdOrCtrl+W', true)).toBe('⌘W');
    expect(keysText('CmdOrCtrl+Shift+S', true)).toBe('⇧⌘S');
    expect(keysText('Ctrl+Cmd+S', true)).toBe('⌃⌘S');
    expect(keysText('CmdOrCtrl+Enter', true)).toBe('⌘↩');
    expect(keysText('Escape', true)).toBe('Esc');
  });

  it('writes other systems with Ctrl and plus signs', () => {
    expect(keysText('CmdOrCtrl+W', false)).toBe('Ctrl+W');
    expect(keysText('CmdOrCtrl+Shift+S', false)).toBe('Ctrl+Shift+S');
    expect(keysText('CmdOrCtrl+Enter', false)).toBe('Ctrl+Enter');
    expect(keysText('Escape', false)).toBe('Escape');
  });
});

describe('withKeys', () => {
  it('puts the shortcut after the label', () => {
    expect(withKeys('Close tab', 'CmdOrCtrl+W', true)).toBe('Close tab (⌘W)');
    expect(withKeys('Run', 'CmdOrCtrl+Enter', false)).toBe('Run (Ctrl+Enter)');
  });

  it('leaves the label alone without a shortcut', () => {
    expect(withKeys('Refresh', null, true)).toBe('Refresh');
  });
});

describe('offReason', () => {
  it('gives the first reason that holds', () => {
    expect(offReason([false, 'a'], [true, READ_ONLY_TIP], [true, 'b'])).toBe(READ_ONLY_TIP);
  });

  it('gives no tooltip when no reason holds', () => {
    expect(offReason([false, 'a'])).toBeUndefined();
    expect(offReason()).toBeUndefined();
  });
});

describe('tabTip', () => {
  it('gives the full title and the environment on a second line', () => {
    expect(tabTip('orders · shop', { name: 'shop', environment: 'Production' })).toBe('orders · shop\nProduction environment');
  });

  it('names the connection when the title does not', () => {
    expect(tabTip('New table', { name: 'shop', environment: 'Staging' })).toBe('New table\nshop · Staging environment');
  });

  it('is only the title when the connection is gone', () => {
    expect(tabTip('Query 1', null)).toBe('Query 1');
  });
});

describe('liveTip', () => {
  it('says what pausing does with new messages', () => {
    expect(liveTip(true, 'messages')).toBe('Paused: new messages wait until you resume');
    expect(liveTip(false, 'notifications')).toBe('New notifications show as they arrive');
  });
});

describe('keyTypeTip', () => {
  it('names core types in full', () => {
    expect(keyTypeTip('zset')).toBe('Sorted set');
    expect(keyTypeTip('string')).toBe('String');
    expect(keyTypeTip('json')).toBe('JSON document');
  });

  it('names module types by their module', () => {
    expect(keyTypeTip({ other: 'TSDB-TYPE' })).toBe('Time series');
    expect(keyTypeTip({ other: 'MBbloom--' })).toBe('Bloom filter');
    expect(keyTypeTip({ other: 'ReJSON-RL' })).toBe('JSON document');
    expect(keyTypeTip({ other: 'acme-type' })).toBe('Module type acme-type');
  });

  it('says why the type of a scanned key is not known yet', () => {
    expect(keyTypeTip('unknown')).toMatch(/read when you open the key/);
  });
});

describe('CHANGE_KIND_TIPS', () => {
  it('explains every change kind a server variable can have', () => {
    for (const kind of ['Read only', 'Restart', 'Reload', 'Not settable', 'Dynamic']) expect(CHANGE_KIND_TIPS[kind]).toBeTruthy();
  });
});

describe('isCut', () => {
  it('is true only when the text is wider than its box', () => {
    expect(isCut({ scrollWidth: 400, clientWidth: 300 })).toBe(true);
    expect(isCut({ scrollWidth: 300, clientWidth: 300 })).toBe(false);
    expect(isCut({ scrollWidth: 0, clientWidth: 0 })).toBe(false);
  });
});
