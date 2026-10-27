import {
  anyDriverErrorOf,
  CommandFailure,
  driverErrorOf,
  isCmdError,
  isReconnectRequired,
  openTransactionOf,
  presentError,
  toCmdError,
} from './errors';

describe('CmdError parsing', () => {
  it('accepts the shapes pinned by the Rust tests in commands.rs', () => {
    expect(isCmdError({ kind: 'secretRequired', secret: 'ssh_password' })).toBe(true);
    expect(
      isCmdError({
        kind: 'tunnel',
        message: 'unknown host key',
        code: 'host_key_unknown',
        fingerprints: ['SHA256:abc'],
        presentedKey: 'ssh-ed25519 AAAA',
      }),
    ).toBe(true);
    expect(isCmdError({ kind: 'invalid', message: 'Enter the host name or IP address.' })).toBe(true);
    expect(isCmdError({ kind: 'session', error: { kind: 'notFound', sessionId: 's1' } })).toBe(true);
    expect(isCmdError({ kind: 'session', error: { kind: 'noDriver', engine: 'redis' } })).toBe(true);
    expect(isCmdError({ kind: 'session', error: { kind: 'noDriver' } })).toBe(false);
  });

  it('accepts driver and confirmation session errors', () => {
    expect(
      isCmdError({
        kind: 'session',
        error: {
          kind: 'driver',
          error: { kind: 'query', message: 'syntax error', code: '42601', detail: null, hint: null, position: 8 },
        },
      }),
    ).toBe(true);
    expect(
      isCmdError({
        kind: 'session',
        error: { kind: 'confirmationRequired', environment: 'production', statements: ['Drop', 'DeleteAll'] },
      }),
    ).toBe(true);
  });

  it('accepts readOnly, catalog and live errors', () => {
    expect(isCmdError({ kind: 'readOnly', message: 'This session is read-only.' })).toBe(true);
    expect(isCmdError({ kind: 'catalog', error: { kind: 'notFound', message: 'No table orders' } })).toBe(true);
    expect(isCmdError({ kind: 'live', error: { kind: 'permissionDenied', message: 'denied', code: '42501' } })).toBe(true);
    expect(isCmdError({ kind: 'live', error: { kind: 'ownConnection', id: 12 } })).toBe(true);
    expect(isCmdError({ kind: 'catalog', error: { kind: 'permissionDenied', message: 'x' } })).toBe(false);
    expect(presentError({ kind: 'live', error: { kind: 'permissionDenied', message: 'denied', code: '42501' } })).toEqual({
      title: 'Permission denied',
      message: 'denied',
      code: '42501',
    });
    expect(presentError({ kind: 'catalog', error: { kind: 'unsupported', message: 'Needs PostgreSQL 15' } }).title).toBe('Not supported');
  });

  it('keeps the partial output of a failed script', () => {
    const partial = {
      results: [{ kind: 'command', tag: 'GRANT', rowsAffected: null }],
      notices: [],
      elapsedMs: 4,
      failedStatement: 1,
      completed: ['committed'],
    };
    const raw = {
      kind: 'session',
      error: { kind: 'driver', error: { kind: 'query', message: 'boom', code: null, detail: null, hint: null, position: null, partial } },
    };
    expect(isCmdError(raw)).toBe(true);
    expect(driverErrorOf(toCmdError(raw))?.partial).toEqual(partial);
    const broken = { ...raw, error: { ...raw.error, error: { ...raw.error.error, partial: { ...partial, completed: ['maybe'] } } } };
    expect(isCmdError(broken)).toBe(false);
  });

  it('rejects malformed values', () => {
    expect(isCmdError(null)).toBe(false);
    expect(isCmdError('boom')).toBe(false);
    expect(isCmdError({ kind: 'invalid' })).toBe(false);
    expect(isCmdError({ kind: 'secretRequired', secret: 'pin_code' })).toBe(false);
    expect(isCmdError({ kind: 'tunnel', message: 'x', code: 'auth', fingerprints: [1] })).toBe(false);
    expect(isCmdError({ kind: 'session', error: { kind: 'driver', error: { kind: 'nope', message: 'x' } } })).toBe(false);
    expect(isCmdError({ kind: 'mystery', message: 'x' })).toBe(false);
  });

  it('turns anything else into an internal error', () => {
    expect(toCmdError('command tree not found')).toEqual({ kind: 'internal', message: 'command tree not found' });
    expect(toCmdError(new Error('network'))).toEqual({ kind: 'internal', message: 'network' });
    expect(toCmdError(42)).toMatchObject({ kind: 'internal' });
  });

  it('unwraps CommandFailure and fills missing optional fields with null', () => {
    const failure = new CommandFailure({ kind: 'notFound', message: 'gone' });
    expect(toCmdError(failure)).toEqual({ kind: 'notFound', message: 'gone' });
    expect(failure.message).toBe('gone');
    const parsed = toCmdError({ kind: 'session', error: { kind: 'driver', error: { kind: 'auth', message: 'denied' } } });
    expect(driverErrorOf(parsed)).toEqual({ kind: 'auth', message: 'denied', code: null, detail: null, hint: null, position: null, partial: null });
    expect(toCmdError({ kind: 'tunnel', message: 'm', code: 'auth', fingerprints: [] })).toMatchObject({ presentedKey: null, host: null, port: null });
    expect(
      toCmdError({ kind: 'tunnel', message: 'm', code: 'host_key_unknown', fingerprints: ['f'], presentedKey: 'k', host: 'bastion', port: 22 }),
    ).toMatchObject({ host: 'bastion', port: 22 });
    expect(isCmdError({ kind: 'tunnel', message: 'm', code: 'x', fingerprints: [], port: '22' })).toBe(false);
  });
});

describe('presentError', () => {
  it('shows driver code, detail and hint', () => {
    expect(
      presentError({
        kind: 'session',
        error: {
          kind: 'driver',
          error: { kind: 'query', message: 'relation "x" does not exist', code: '42P01', detail: 'd', hint: 'h', position: 15 },
        },
      }),
    ).toEqual({ title: 'Query failed', message: 'relation "x" does not exist', code: '42P01', detail: 'd', hint: 'h' });
  });

  it('names the secret that is needed', () => {
    expect(presentError({ kind: 'secretRequired', secret: 'ssh_key_passphrase' }).message).toBe(
      'Enter the SSH key passphrase to continue.',
    );
  });
});

describe('M2 error kinds', () => {
  it('parses reconnectRequired and keeps its kind', () => {
    const raw = { kind: 'reconnectRequired', message: 'The connection was edited; reconnect this tab.' };
    expect(isCmdError(raw)).toBe(true);
    const parsed = toCmdError(raw);
    expect(isReconnectRequired(parsed)).toBe(true);
    expect(presentError(parsed)).toEqual({ title: 'Reconnect needed', message: raw.message });
    expect(isCmdError({ kind: 'reconnectRequired' })).toBe(false);
  });

  it('parses openTransaction session errors', () => {
    const raw = { kind: 'session', error: { kind: 'openTransaction', message: 'The session has an open transaction.' } };
    expect(isCmdError(raw)).toBe(true);
    const parsed = toCmdError(raw);
    expect(openTransactionOf(parsed)).toBe('The session has an open transaction.');
    expect(presentError(parsed).title).toBe('Transaction open');
    expect(openTransactionOf({ kind: 'notFound', message: 'x' })).toBeNull();
    expect(isCmdError({ kind: 'session', error: { kind: 'openTransaction' } })).toBe(false);
  });

  it('parses needsConfirmation catalog errors but not as live errors', () => {
    const raw = { kind: 'catalog', error: { kind: 'needsConfirmation', message: 'm', warnings: ['a', 'b'] } };
    expect(isCmdError(raw)).toBe(true);
    expect(presentError(toCmdError(raw)).message).toBe('a b');
    expect(isCmdError({ kind: 'live', error: { kind: 'needsConfirmation', message: 'm', warnings: [] } })).toBe(false);
    expect(isCmdError({ kind: 'catalog', error: { kind: 'needsConfirmation', message: 'm', warnings: [1] } })).toBe(false);
  });

  it('fills optional driver fields inside catalog and live errors', () => {
    const catalog = toCmdError({ kind: 'catalog', error: { kind: 'driver', error: { kind: 'query', message: 'boom' } } });
    expect(anyDriverErrorOf(catalog)).toEqual({ kind: 'query', message: 'boom', code: null, detail: null, hint: null, position: null, partial: null });
    const live = toCmdError({ kind: 'live', error: { kind: 'driver', error: { kind: 'timeout', message: 'slow', code: 'X' } } });
    expect(anyDriverErrorOf(live)?.code).toBe('X');
    expect(anyDriverErrorOf({ kind: 'internal', message: 'x' })).toBeNull();
  });
});
