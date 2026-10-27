import type { PendingRestore } from '../../api/notices';
import type { SessionInfo } from '../../api/sessions';
import { formatRecordedAt, keychainLines, reloadNoticeText, restoreCandidates, restoreEngine, restoreHeadline, restoreProblem } from './notices';

const entry: PendingRestore = {
  server: 'mysql:3e11fa47-71ca-11e1-9e33-c80aa9429562',
  route: 'db.internal:3306',
  database: 'shop',
  collation: 'utf8mb4_0900_ai_ci',
  temporaries: ['utf8mb4_bin'],
  verified: true,
  connectionId: 'c1',
  recordedAt: 1_760_000_000_000,
};

function session(id: string, connectionId: string, engine: SessionInfo['engine']): SessionInfo {
  return {
    id,
    connectionId,
    engine,
    server: { engine, version: '8.4.0', major: 8, minor: 4, patch: 0 },
    environment: 'development',
    database: null,
    readOnly: false,
    confirmDestructive: false,
    purpose: 'user',
    openedAt: 0,
    encryption: { tls: true, mode: 'prefer', passwordUnencrypted: false },
  };
}

describe('pending restore helpers', () => {
  it('reads the engine from the server identity; old host:port entries have none', () => {
    expect(restoreEngine(entry)).toBe('mysql');
    expect(restoreEngine({ server: 'mariadb:abc|host|3306' })).toBe('mariadb');
    expect(restoreEngine({ server: 'db.internal:3306' })).toBeNull();
  });

  it('offers the recording connection first, then other MySQL and MariaDB sessions, once each', () => {
    const tabs = [
      { title: 'Other', session: session('s2', 'c2', 'mariadb') },
      { title: 'Pg', session: session('s3', 'c3', 'postgres') },
      { title: 'Shop', session: session('s1', 'c1', 'mysql') },
      { title: 'Shop again', session: session('s1', 'c1', 'mysql') },
      { title: 'Closed', session: null },
    ];
    const out = restoreCandidates(entry, tabs);
    expect(out.map((c) => [c.session.id, c.recording])).toEqual([
      ['s1', true],
      ['s2', false],
    ]);
    expect(restoreCandidates({ ...entry, server: 'db.internal:3306' }, tabs)).toEqual([]);
  });

  it('formats the time and the headline', () => {
    expect(formatRecordedAt(0)).toBe('at an unknown time');
    expect(formatRecordedAt(entry.recordedAt, 'en-GB')).toMatch(/2025/);
    expect(restoreHeadline(entry)).toBe('The default collation of database shop was left changed (it should be utf8mb4_0900_ai_ci).');
  });

  it('explains restore failures and offers Dismiss only for stale entries', () => {
    expect(restoreProblem({ kind: 'notFound', message: 'x' })).toMatchObject({ offerDismiss: false });
    expect(restoreProblem({ kind: 'notFound', message: 'x' }).message).toContain('another server');
    const stale = restoreProblem({ kind: 'invalid', message: 'The database is not at a collation this run left.' });
    expect(stale.offerDismiss).toBe(true);
    expect(stale.message).toContain('dismiss');
    expect(restoreProblem({ kind: 'internal', message: 'lock wait' })).toEqual({ message: 'lock wait', offerDismiss: false });
  });

  it('names keychain entries by connection, or as deleted', () => {
    const lines = keychainLines(
      [
        { connectionId: 'c1', kind: 'db_password' },
        { connectionId: 'gone', kind: 'ssh_key_passphrase' },
      ],
      (id) => (id === 'c1' ? { name: 'Shop' } : null),
    );
    expect(lines).toEqual(['The database password of connection Shop', 'The SSH key passphrase of a deleted connection']);
  });
});

describe('reload notice', () => {
  const names: Record<string, string> = { c1: 'Shop', c2: 'Billing' };
  const nameOf = (id: string) => names[id] ?? id;

  it('says how many sessions a reload closed and which transactions it rolled back', () => {
    expect(
      reloadNoticeText(
        {
          sessions: [
            { connectionId: 'c1', inTransaction: true },
            { connectionId: 'c1', inTransaction: true },
            { connectionId: 'c2', inTransaction: false },
            { connectionId: 'c2', inTransaction: null },
          ],
          opening: ['c1'],
        },
        nameOf,
      ),
    ).toBe(
      'The window was reloaded; 5 sessions were closed. Open transactions in Shop were rolled back. Statements still running in Billing were cancelled and rolled back.',
    );
    expect(reloadNoticeText({ sessions: [{ connectionId: 'c2', inTransaction: false }], opening: [] }, nameOf)).toBe(
      'The window was reloaded; 1 session was closed.',
    );
  });

  it('says nothing after the first load', () => {
    expect(reloadNoticeText({ sessions: [], opening: [] }, nameOf)).toBeNull();
  });
});

