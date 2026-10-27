import { describe, expect, it } from 'vitest';
import type { ClientInfo, ConfigEntry, MemoryAnalysis, Process, ServerVariable } from '../api/live';
import { DEFAULT_MEMORY_SCAN } from '../api/live';
import {
  bigKeys,
  blockingChains,
  checkVariableValue,
  clientType,
  configNote,
  filterConfig,
  libraryNameOf,
  slotCount,
  slotSegments,
  slotText,
  filterClients,
  filterProcesses,
  filterVariables,
  formatSeconds,
  isChanged,
  isLongRunning,
  memoryOptionsFrom,
  prefixBytes,
  sortProcesses,
  stopsPolling,
  variableCategory,
} from './live';

function proc(id: number, extra: Partial<Process> = {}): Process {
  return {
    id,
    own: false,
    user: 'app_rw',
    database: 'shop',
    application: 'shop-api',
    clientAddress: '10.20.4.17',
    clientHostname: null,
    clientPort: 50122,
    backendType: 'client backend',
    command: null,
    state: 'idle',
    waitEventType: null,
    waitEvent: null,
    backendStart: null,
    xactStart: null,
    queryStart: null,
    durationMs: 1000,
    query: 'SELECT 1',
    blockedBy: [],
    leaderId: null,
    queryId: null,
    progress: null,
    stage: null,
    maxStage: null,
    memoryBytes: null,
    rowsExamined: null,
    ...extra,
  };
}

const noFilter = { text: '', hideIdle: false, showBackground: true, includeOwn: true, states: new Set<never>() };

describe('formatSeconds', () => {
  it('picks the two largest units', () => {
    expect(formatSeconds(42)).toBe('42s');
    expect(formatSeconds(30 * 60 + 43)).toBe('30m 43s');
    expect(formatSeconds(5 * 3600 + 2 * 60)).toBe('5h 02m');
    expect(formatSeconds(3 * 86400 + 4 * 3600)).toBe('3d 4h');
    expect(formatSeconds(-5)).toBe('0s');
  });
});

describe('processes', () => {
  const list = [
    proc(1, { state: 'active', query: 'UPDATE orders SET x = 1', durationMs: 400_000 }),
    proc(2, { state: 'idle', durationMs: 900_000 }),
    proc(3, { backendType: 'autovacuum worker', user: null, state: null }),
    proc(4, { own: true, state: 'active' }),
    proc(5, { state: 'idle in transaction', application: 'psql' }),
  ];

  it('filters by text, idle, background workers, own connection and state chips', () => {
    expect(filterProcesses(list, 'postgres', noFilter).map((p) => p.id)).toEqual([1, 2, 3, 4, 5]);
    expect(filterProcesses(list, 'postgres', { ...noFilter, showBackground: false }).map((p) => p.id)).toEqual([1, 2, 4, 5]);
    expect(filterProcesses(list, 'postgres', { ...noFilter, includeOwn: false }).map((p) => p.id)).toEqual([1, 2, 3, 5]);
    expect(filterProcesses(list, 'postgres', { ...noFilter, hideIdle: true }).map((p) => p.id)).toEqual([1, 3, 4, 5]);
    expect(filterProcesses(list, 'postgres', { ...noFilter, text: 'PSQL' }).map((p) => p.id)).toEqual([5]);
    expect(filterProcesses(list, 'postgres', { ...noFilter, text: 'update orders' }).map((p) => p.id)).toEqual([1]);
    expect(filterProcesses(list, 'postgres', { ...noFilter, states: new Set(['idle in transaction', 'other'] as const) }).map((p) => p.id)).toEqual([3, 5]);
  });

  it('treats MySQL Sleep as idle and system threads as background', () => {
    const my = [
      proc(10, { command: 'Sleep', state: '' }),
      proc(11, { command: 'Query', state: 'executing' }),
      proc(12, { user: 'event_scheduler', command: 'Daemon' }),
      proc(13, { user: 'system user', command: 'Connect' }),
    ];
    expect(filterProcesses(my, 'mysql', { ...noFilter, hideIdle: true, showBackground: false }).map((p) => p.id)).toEqual([11]);
  });

  it('flags long running statements but not idle or background sessions', () => {
    expect(isLongRunning(list[0] as Process, 'postgres', 300_000)).toBe(true);
    expect(isLongRunning(list[1] as Process, 'postgres', 300_000)).toBe(false);
    expect(isLongRunning(list[0] as Process, 'postgres', 0)).toBe(false);
    expect(isLongRunning(proc(1, { command: 'Query', durationMs: 301_000 }), 'mariadb', 300_000)).toBe(true);
    expect(isLongRunning(proc(1, { command: 'Sleep', durationMs: 301_000 }), 'mariadb', 300_000)).toBe(false);
  });

  it('sorts by duration descending with a stable tie on id, missing durations last', () => {
    const sorted = sortProcesses([proc(3, { durationMs: 5 }), proc(1, { durationMs: null }), proc(2, { durationMs: 5 })], 'duration', true);
    expect(sorted.map((p) => p.id)).toEqual([3, 2, 1]);
    expect(sortProcesses(sorted, 'id', false).map((p) => p.id)).toEqual([1, 2, 3]);
  });
});

describe('blockingChains', () => {
  it('builds a chain from the root blocker down, depth first', () => {
    const chains = blockingChains([
      proc(4398, { state: 'idle in transaction' }),
      proc(4411, { state: 'active', blockedBy: [4398] }),
      proc(4420, { state: 'active', blockedBy: [4411] }),
      proc(4430, { state: 'active', blockedBy: [4398] }),
      proc(5000, { state: 'idle' }),
    ]);
    expect(chains).toHaveLength(1);
    expect(chains[0]?.map((l) => [l.process.id, l.depth, l.waitsFor])).toEqual([
      [4398, 0, null],
      [4411, 1, 4398],
      [4420, 2, 4411],
      [4430, 1, 4398],
    ]);
  });

  it('shows a wait cycle once, starting at its lowest id', () => {
    const chains = blockingChains([proc(7, { blockedBy: [9] }), proc(9, { blockedBy: [7] }), proc(11, { blockedBy: [9] })]);
    expect(chains).toHaveLength(1);
    expect(chains[0]?.map((l) => l.process.id)).toEqual([7, 9, 11]);
  });

  it('ignores blockers that are not in the list and returns nothing without waits', () => {
    expect(blockingChains([proc(1, { blockedBy: [999] }), proc(2)])).toEqual([]);
  });
});

function variable(name: string, extra: Partial<ServerVariable> = {}): ServerVariable {
  return {
    name,
    value: '100',
    sessionValue: null,
    globalValue: null,
    defaultValue: '100',
    resetValue: null,
    unit: null,
    category: null,
    description: null,
    varType: 'integer',
    typeInferred: false,
    context: null,
    settableScopes: ['global', 'session'],
    dynamic: true,
    readOnly: false,
    restartRequired: false,
    pendingRestart: false,
    minValue: null,
    maxValue: null,
    enumValues: [],
    source: null,
    sourceFile: null,
    setBy: null,
    setTime: null,
    persistedValue: null,
    masked: false,
    ...extra,
  };
}

describe('server variables', () => {
  it('detects values changed from the default, also from the source alone', () => {
    expect(isChanged(variable('a', { value: '800', defaultValue: '151' }))).toBe(true);
    expect(isChanged(variable('a', { value: 'ON', defaultValue: 'on' }))).toBe(false);
    expect(isChanged(variable('a', { defaultValue: null, source: 'configuration file' }))).toBe(true);
    expect(isChanged(variable('a', { defaultValue: null, source: 'default' }))).toBe(false);
    expect(isChanged(variable('a', { defaultValue: null, source: 'COMPILED' }))).toBe(false);
    expect(isChanged(variable('a', { masked: true, value: '********', defaultValue: '', source: 'DYNAMIC' }))).toBe(true);
  });

  it("uses PostgreSQL's category and derives one for MySQL names", () => {
    expect(variableCategory(variable('work_mem', { category: 'Resource Usage / Memory' }))).toBe('Resource Usage / Memory');
    expect(variableCategory(variable('innodb_buffer_pool_size'))).toBe('InnoDB');
    expect(variableCategory(variable('wsrep_provider'))).toBe('Galera (wsrep)');
    expect(variableCategory(variable('binlog_format'))).toBe('Replication');
    expect(variableCategory(variable('max_connections'))).toBe('General');
  });

  it('filters by text, category, changed and dynamic', () => {
    const vars = [
      variable('max_connections', { value: '800', defaultValue: '151', description: 'Most client connections' }),
      variable('innodb_log_file_size', { dynamic: false }),
      variable('version', { readOnly: true }),
      variable('secret_cmd', { masked: true, value: 'abc' }),
    ];
    const none = { text: '', category: '', changedOnly: false, dynamicOnly: false };
    expect(filterVariables(vars, { ...none, text: 'client' }).map((v) => v.name)).toEqual(['max_connections']);
    expect(filterVariables(vars, { ...none, category: 'InnoDB' }).map((v) => v.name)).toEqual(['innodb_log_file_size']);
    expect(filterVariables(vars, { ...none, changedOnly: true }).map((v) => v.name)).toEqual(['max_connections']);
    expect(filterVariables(vars, { ...none, dynamicOnly: true }).map((v) => v.name)).toEqual(['max_connections', 'secret_cmd']);
    // A masked value is never matched by its (masked) text.
    expect(filterVariables(vars, { ...none, text: 'abc' })).toEqual([]);
  });

  it('checks typed values against type, unit, range and allowed values', () => {
    const conns = variable('max_connections', { minValue: '10', maxValue: '100000' });
    expect(checkVariableValue(conns, '800')).toBeNull();
    expect(checkVariableValue(conns, '5')).toBe('The smallest allowed value is 10.');
    expect(checkVariableValue(conns, '200000')).toBe('The largest allowed value is 100000.');
    expect(checkVariableValue(conns, '1.5')).toBe('Enter a whole number.');
    expect(checkVariableValue(conns, 'many')).toBe('Enter a whole number.');
    expect(checkVariableValue(conns, '')).toBe('Enter a value.');
    expect(checkVariableValue(conns, '8MB')).toBe('This variable takes a plain number, without a unit.');
    expect(checkVariableValue(variable('work_mem', { unit: 'kB' }), '64MB')).toBeNull();
    expect(checkVariableValue(variable('ratio', { varType: 'real' }), '0.5')).toBeNull();
    expect(checkVariableValue(variable('flag', { varType: 'boolean' }), 'OFF')).toBeNull();
    expect(checkVariableValue(variable('flag', { varType: 'boolean' }), 'maybe')).toBe('Enter ON or OFF.');
    const level = variable('log_min_messages', { varType: 'enum', enumValues: ['debug', 'info', 'warning'] });
    expect(checkVariableValue(level, 'INFO')).toBeNull();
    expect(checkVariableValue(level, 'loud')).toBe('Choose one of: debug, info, warning.');
    const mode = variable('sql_mode', { varType: 'set', enumValues: ['STRICT_TRANS_TABLES', 'ANSI_QUOTES'] });
    expect(checkVariableValue(mode, '')).toBeNull();
    expect(checkVariableValue(mode, 'ansi_quotes, STRICT_TRANS_TABLES')).toBeNull();
    expect(checkVariableValue(mode, 'ANSI_QUOTES,NOPE')).toBe('Not allowed: NOPE.');
    expect(checkVariableValue(variable('s', { varType: 'string' }), '')).toBeNull();
  });
});

function client(id: number, extra: Partial<ClientInfo> = {}): ClientInfo {
  return {
    id,
    isSelf: false,
    addr: '10.0.0.1:5000',
    laddr: null,
    name: null,
    user: 'default',
    age: 100,
    idle: 0,
    flags: 'N',
    flagNames: [],
    db: 0,
    sub: 0,
    psub: 0,
    ssub: null,
    multi: -1,
    qbuf: 0,
    omem: 0,
    totMem: 20_000,
    cmd: 'get',
    resp: 2,
    libName: null,
    libVer: null,
    other: [],
    ...extra,
  };
}

describe('Redis clients', () => {
  it('derives the client type from flags and subscriptions', () => {
    expect(clientType(client(1))).toBe('normal');
    expect(clientType(client(1, { flags: 'S' }))).toBe('replica');
    expect(clientType(client(1, { flags: 'M' }))).toBe('master');
    expect(clientType(client(1, { flags: 'O' }))).toBe('monitor');
    expect(clientType(client(1, { flags: 'P' }))).toBe('pubsub');
    expect(clientType(client(1, { psub: 2 }))).toBe('pubsub');
  });

  it('filters by text, type and idle time', () => {
    const list = [client(1, { name: 'worker-1', idle: 900 }), client(2, { flags: 'P' }), client(3, { user: 'kim', idle: 5 })];
    expect(filterClients(list, { text: 'WORKER', type: '', idleOverSeconds: 0 }).map((c) => c.id)).toEqual([1]);
    expect(filterClients(list, { text: '', type: 'pubsub', idleOverSeconds: 0 }).map((c) => c.id)).toEqual([2]);
    expect(filterClients(list, { text: '', type: '', idleOverSeconds: 600 }).map((c) => c.id)).toEqual([1]);
    expect(filterClients(list, { text: '3', type: '', idleOverSeconds: 0 }).map((c) => c.id)).toEqual([3]);
  });
});

describe('Redis memory analysis', () => {
  const form = { pattern: '*', delimiter: ':', depth: 1, maxKeys: 10_000, timeBudgetSeconds: 10, samples: 5 };

  it('sends every field, with `*` meaning all keys', () => {
    expect(memoryOptionsFrom(form, DEFAULT_MEMORY_SCAN)).toEqual({ options: { ...DEFAULT_MEMORY_SCAN }, problems: [] });
    const out = memoryOptionsFrom({ ...form, pattern: ' user:* ', depth: '2', timeBudgetSeconds: '30', samples: 0 }, DEFAULT_MEMORY_SCAN);
    expect(out.options).toMatchObject({ pattern: 'user:*', depth: 2, timeBudgetMs: 30_000, samples: 0, scanCount: 500, topKeys: 10, maxPrefixes: 100 });
  });

  it('lists every problem of the form', () => {
    const out = memoryOptionsFrom({ pattern: '', delimiter: '', depth: 0, maxKeys: '', timeBudgetSeconds: '1.5', samples: -1 }, DEFAULT_MEMORY_SCAN);
    expect(out.options).toBeNull();
    expect(out.problems).toEqual([
      'Prefix depth: between 1 and 16.',
      'Most keys: enter a number.',
      'Time budget: enter a whole number.',
      'Samples: between 0 and 1,000.',
      'Delimiter: enter at least one character.',
    ]);
  });

  it('merges the biggest keys of every type, biggest first, or of one type', () => {
    const analysis: MemoryAnalysis = {
      scannedKeys: 3,
      dbSize: 3,
      complete: true,
      stoppedBy: 'complete',
      elapsedMs: 5,
      totalBytes: 600,
      prefixes: [],
      otherPrefixes: null,
      types: [
        { keyType: 'hash', keys: 2, bytes: 400, biggest: [{ key: { t: 'text', v: 'h1' }, bytes: 300, length: 10 }, { key: { t: 'text', v: 'h2' }, bytes: 100, length: 2 }] },
        { keyType: 'string', keys: 1, bytes: 200, biggest: [{ key: { t: 'text', v: 's1' }, bytes: 200, length: null }] },
      ],
    };
    expect(bigKeys(analysis, '').map((k) => [k.keyType, k.bytes])).toEqual([
      ['hash', 300],
      ['string', 200],
      ['hash', 100],
    ]);
    expect(bigKeys(analysis, 'string')).toHaveLength(1);
  });

  it('prefers the whole-database estimate and marks it as one', () => {
    expect(prefixBytes({ bytes: 100, estimatedBytes: 1000 })).toEqual({ bytes: 1000, estimated: true });
    expect(prefixBytes({ bytes: 100, estimatedBytes: 100 })).toEqual({ bytes: 100, estimated: false });
    expect(prefixBytes({ bytes: 100, estimatedBytes: null })).toEqual({ bytes: 100, estimated: false });
  });
});

function cfg(name: string, extra: Partial<ConfigEntry> = {}): ConfigEntry {
  return { name, value: 'x', sensitive: false, hasValue: true, dangerous: false, immutable: false, protected: false, ...extra };
}

describe('Redis config', () => {
  const list = [
    cfg('maxmemory', { value: '100mb' }),
    cfg('requirepass', { value: null, sensitive: true, dangerous: true }),
    cfg('dir', { value: '/data', protected: true }),
    cfg('daemonize', { value: 'no', immutable: true }),
  ];

  it('filters by name and by value, never by a secret value', () => {
    expect(filterConfig(list, 'MEMORY', false).map((e) => e.name)).toEqual(['maxmemory']);
    expect(filterConfig(list, '/data', false).map((e) => e.name)).toEqual(['dir']);
    expect(filterConfig([cfg('masterauth', { value: 'hunter2', sensitive: true })], 'hunter', false)).toEqual([]);
    expect(filterConfig(list, '', true).map((e) => e.name)).toEqual(['requirepass', 'dir', 'daemonize']);
  });

  it('labels secret, startup-only, protected and risky parameters', () => {
    expect(configNote(list[1] as ConfigEntry).map((n) => n.label)).toEqual(['secret', 'risky']);
    expect(configNote(list[2] as ConfigEntry).map((n) => n.label)).toEqual(['protected']);
    expect(configNote(list[3] as ConfigEntry).map((n) => n.label)).toEqual(['startup only']);
    expect(configNote(list[0] as ConfigEntry)).toEqual([]);
  });
});

describe('Redis functions', () => {
  it('reads the library name from the shebang line', () => {
    expect(libraryNameOf('#!lua name=mylib\nredis.register_function(...)')).toBe('mylib');
    expect(libraryNameOf('#!lua engine=x name=other_lib\n')).toBe('other_lib');
    expect(libraryNameOf('\uFEFF#!lua name=bom\r\n')).toBe('bom');
    expect(libraryNameOf('redis.register_function(...)')).toBeNull();
    expect(libraryNameOf('#!lua\n')).toBeNull();
  });
});

describe('Redis cluster slots', () => {
  it('lays out assigned and unassigned ranges in slot order', () => {
    const segs = slotSegments(
      [
        { range: { start: 5461, end: 10922 }, nodeId: 'b' },
        { range: { start: 0, end: 5460 }, nodeId: 'a' },
      ],
      [{ start: 10923, end: 16383 }],
    );
    expect(segs.map((s) => s.nodeId)).toEqual(['a', 'b', null]);
    expect(segs[0]?.start).toBe(0);
    expect(segs.reduce((sum, s) => sum + s.width, 0)).toBeCloseTo(1, 10);
    expect(segs[2]?.start).toBeCloseTo(10923 / 16384, 10);
  });

  it('counts and writes slot ranges', () => {
    const ranges = [{ start: 0, end: 5460 }, { start: 7000, end: 7000 }];
    expect(slotCount(ranges)).toBe(5462);
    expect(slotText(ranges)).toBe('0-5460, 7000');
  });
});

describe('stopsPolling', () => {
  it('stops a panel the server does not support, and only that', () => {
    expect(stopsPolling({ kind: 'live', error: { kind: 'unsupported', message: 'This node is not part of a Galera cluster.' } })).toBe(true);
    expect(stopsPolling({ kind: 'live', error: { kind: 'permissionDenied', message: 'no', code: null } })).toBe(false);
    expect(stopsPolling({ kind: 'session', error: { kind: 'notFound', sessionId: 's' } })).toBe(false);
  });
});
