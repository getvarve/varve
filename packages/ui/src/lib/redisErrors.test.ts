import type { CmdError, DriverError } from '../api/errors';
import { initCommandProblem } from './redisInit';
import { presentRedisFailure, readOnlyHint, redisErrorView, refusedCommand, transactionHint } from './redisErrors';

function driver(partial: Partial<DriverError>): CmdError {
  return {
    kind: 'session',
    error: {
      kind: 'driver',
      error: { kind: 'query', message: '', code: null, detail: null, hint: null, position: null, ...partial },
    },
  };
}

describe('redisErrorView', () => {
  it('explains MULTI_OPEN and offers a retry', () => {
    const view = redisErrorView(driver({ code: 'MULTI_OPEN', message: 'A MULTI transaction is open in the console' }));
    expect(view.title).toBe('MULTI open in the console');
    expect(view.action).toBe('retry');
    expect(view.hint).toContain('EXEC or DISCARD');
    expect(view.refused).toBe(true);
  });

  it('explains TRANSACTION_RESET without a retry', () => {
    const view = redisErrorView(driver({ code: 'TRANSACTION_RESET', message: 'Line 3: the WATCH was lost' }));
    expect(view.title).toBe('Transaction state lost');
    expect(view.action).toBeNull();
    expect(view.hint).toContain('WATCH');
  });

  it('gives read-only refusals of KEYS, SORT_RO and EVAL_RO their reason', () => {
    const keys = redisErrorView(driver({ kind: 'read_only', message: 'Line 1: KEYS changes data and this connection is read-only' }));
    expect(keys.hint).toContain('SCAN');
    expect(readOnlyHint('Line 2: SORT_RO changes data and this connection is read-only')).toContain('@dangerous');
    expect(readOnlyHint('EVAL_RO changes data')).toContain('scripts');
    expect(readOnlyHint('Line 1: SET changes data and this connection is read-only')).toBeNull();
    expect(refusedCommand('Line 12: fcall_ro x')).toBe('FCALL_RO');
  });

  it('shows the NOPERM hint of a refused script check and keeps it refused', () => {
    const view = redisErrorView(driver({ code: 'NOPERM', message: "this user has no permissions to run the 'eval' command", hint: 'The group check runs as a Lua script: allow EVAL for this user.' }));
    expect(view).toMatchObject({ title: 'No permission', hint: 'The group check runs as a Lua script: allow EVAL for this user.', refused: true });
  });

  it('turns readOnly and reconnectRequired into their own views', () => {
    expect(redisErrorView({ kind: 'readOnly', message: 'Read-only.' }).title).toBe('Read-only connection');
    const reconnect = redisErrorView({ kind: 'reconnectRequired', message: 'The connection was edited.' });
    expect(reconnect.action).toBe('reconnect');
  });

  it('leaves other errors as presented', () => {
    const view = redisErrorView({ kind: 'internal', message: 'boom' });
    expect(view).toMatchObject({ message: 'boom', action: null, refused: false });
  });
});

describe('initCommandProblem', () => {
  it('reads the line and reason from a save error', () => {
    expect(initCommandProblem({ kind: 'invalid', message: 'Init command on line 2: CLIENT cannot be an init command' })).toEqual({
      line: 2,
      reason: 'CLIENT cannot be an init command',
      credentials: false,
    });
  });

  it('marks AUTH and HELLO AUTH as belonging in the credentials', () => {
    const auth = initCommandProblem(driver({ kind: 'connect', message: 'Init command on line 1: AUTH cannot be an init command: it puts a password in the init text' }));
    expect(auth?.credentials).toBe(true);
    const hello = initCommandProblem({ kind: 'invalid', message: 'Init command on line 3: HELLO cannot be an init command: it puts a password' });
    expect(hello).toMatchObject({ line: 3, credentials: true });
    const proto = initCommandProblem({ kind: 'invalid', message: 'Init command on line 1: HELLO cannot be an init command: it switches the protocol' });
    expect(proto?.credentials).toBe(false);
  });

  it('ignores other errors', () => {
    expect(initCommandProblem({ kind: 'invalid', message: 'Enter the SSH user.' })).toBeNull();
    expect(initCommandProblem({ kind: 'secretRequired', secret: 'db_password' })).toBeNull();
  });
});

describe('console failures', () => {
  it('gives console read-only refusals their reason and keeps the code', () => {
    const failure = presentRedisFailure(driver({ kind: 'read_only', message: 'Line 1: KEYS changes data and this connection is read-only', code: 'READ_ONLY' }));
    expect(failure.title).toBe('Read-only connection');
    expect(failure.hint).toContain('SCAN');
    expect(failure.code).toBe('READ_ONLY');
    expect(presentRedisFailure(driver({ kind: 'read_only', message: 'Line 2: EVAL_RO changes data' })).hint).toContain('scripts');
  });

  it('explains MULTI and WATCH mistakes', () => {
    expect(presentRedisFailure(driver({ message: 'Line 3: ERR EXEC without MULTI', code: 'ERR' })).title).toBe('No transaction open');
    expect(presentRedisFailure(driver({ message: 'ERR MULTI calls can not be nested', code: 'ERR' })).hint).toContain('EXEC');
    expect(presentRedisFailure(driver({ message: 'ERR WATCH inside MULTI is not allowed', code: 'ERR' })).hint).toContain('DISCARD');
    const abort = presentRedisFailure(driver({ message: 'EXECABORT Transaction discarded because of previous errors.', code: 'EXECABORT' }));
    expect(abort.title).toBe('Transaction discarded');
    expect(transactionHint({ kind: 'query', message: 'ERR wrong number of arguments', code: 'ERR', detail: null, hint: null, position: null })).toBeNull();
  });

  it('keeps the MULTI_OPEN and TRANSACTION_RESET hints of the key browser', () => {
    expect(presentRedisFailure(driver({ code: 'TRANSACTION_RESET', message: 'Line 3: the WATCH was lost' })).hint).toContain('WATCH');
    expect(presentRedisFailure(driver({ code: 'MULTI_OPEN', message: 'open' })).hint).toContain('EXEC or DISCARD');
  });

  it('falls back to the plain presentation with detail and hint', () => {
    const failure = presentRedisFailure(driver({ message: 'WRONGTYPE Operation against a key', code: 'WRONGTYPE', detail: 'd', hint: 'h' }));
    expect(failure.code).toBe('WRONGTYPE');
    expect(failure.detail).toBe('d');
    expect(failure.hint).toBe('h');
  });
});

describe('time series under a console WATCH', () => {
  it('explains WATCH_OPEN and offers a retry', () => {
    const view = redisErrorView(driver({ code: 'WATCH_OPEN', message: 'A WATCH is open in the console of this connection.' }));
    expect(view.title).toBe('WATCH open in the console');
    expect(view.hint).toContain('UNWATCH');
    expect(view.action).toBe('retry');
  });
});
